//! A synthetic camera backend.
//!
//! It exists so that the CLI, the GUI and the test-suite can be exercised
//! end to end with no hardware and no vendor SDK — including the failure
//! paths that are otherwise only reachable by physically yanking a USB cable.
//! [`SimHandle`] injects device loss, USB stalls and frame stalls on demand.
//!
//! The image itself is a drifting star field: enough structure to tell
//! whether debayering, stretching, ROI and binning are wired up correctly.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::{Receiver, Sender, unbounded};

use crate::camera::{Backend, Camera, CameraId, CameraInfo};
use crate::control::{Binning, BitDepth, ControlId, ControlInfo, Roi, WhiteBalance};
use crate::error::{Error, Result};
use crate::event::CameraEvent;
use crate::frame::{BayerPattern, Frame, FrameMeta, PixelFormat};
use crate::ring::{FrameRing, StreamStop};

pub const BACKEND_NAME: &str = "simulator";

/// Shortest gap the simulator will put between frames, so a 1 us exposure
/// does not turn into a busy loop that pins a core.
const MIN_FRAME_INTERVAL: Duration = Duration::from_millis(5);
/// Pretend readout cost added to every exposure.
const READOUT: Duration = Duration::from_millis(3);
/// How long the producer sleeps between checks of the stop flag, so
/// `stop_streaming` stays responsive during a 60 s exposure.
const TICK: Duration = Duration::from_millis(10);

/// Faults a test (or a curious user) can inject into a simulated camera.
///
/// All of these mirror something real hardware does: an unplug, a stalled
/// bulk endpoint, a camera that stops delivering frames without saying why.
#[derive(Debug, Default)]
struct Faults {
    detached: AtomicBool,
    device_lost: AtomicBool,
    usb_stall: AtomicBool,
    freeze: AtomicBool,
    open_busy: AtomicBool,
    /// Milliseconds of extra latency on every control call, to prove the UI
    /// does not do camera IO on its own thread.
    control_latency_ms: AtomicU64,
}

#[derive(Debug)]
struct SimDevice {
    info: CameraInfo,
    faults: Faults,
    open: AtomicBool,
    seed: u64,
}

/// Handle for injecting faults into one simulated camera.
///
/// Cheap to clone and safe to use from any thread, including while the
/// camera is streaming.
#[derive(Debug, Clone)]
pub struct SimHandle(Arc<SimDevice>);

impl SimHandle {
    pub fn id(&self) -> &CameraId {
        &self.0.info.id
    }

    /// Simulate an unplug: the device disappears from enumeration and the
    /// open handle starts failing with [`Error::DeviceLost`].
    pub fn unplug(&self) {
        self.0.faults.detached.store(true, Ordering::SeqCst);
        self.0.faults.device_lost.store(true, Ordering::SeqCst);
    }

    /// Simulate the replug. Existing handles stay dead — exactly like real
    /// hardware, where the old file descriptor never comes back to life.
    pub fn replug(&self) {
        self.0.faults.detached.store(false, Ordering::SeqCst);
        self.0.faults.device_lost.store(false, Ordering::SeqCst);
        self.0.faults.usb_stall.store(false, Ordering::SeqCst);
        self.0.faults.freeze.store(false, Ordering::SeqCst);
    }

    pub fn is_attached(&self) -> bool {
        !self.0.faults.detached.load(Ordering::SeqCst)
    }

    /// Stall the USB pipe on the next transfer.
    pub fn stall_usb(&self) {
        self.0.faults.usb_stall.store(true, Ordering::SeqCst);
    }

    /// Stop delivering frames without reporting anything, the way a camera
    /// with a wedged sensor does. Consumers should see timeouts.
    pub fn freeze_frames(&self, frozen: bool) {
        self.0.faults.freeze.store(frozen, Ordering::SeqCst);
    }

    /// Make the device report itself as already open by another process.
    pub fn set_busy(&self, busy: bool) {
        self.0.faults.open_busy.store(busy, Ordering::SeqCst);
    }

    /// Add latency to every control call.
    pub fn set_control_latency(&self, latency: Duration) {
        self.0
            .faults
            .control_latency_ms
            .store(latency.as_millis() as u64, Ordering::SeqCst);
    }
}

/// The simulated backend. One or more synthetic cameras.
#[derive(Debug)]
pub struct SimulatorBackend {
    devices: Vec<Arc<SimDevice>>,
}

impl Default for SimulatorBackend {
    fn default() -> Self {
        SimulatorBackend::new()
    }
}

impl SimulatorBackend {
    /// Two cameras: a 1920x1080 colour one shaped like an SV305C Pro, and a
    /// smaller mono one, so colour and mono paths both get exercised.
    pub fn new() -> SimulatorBackend {
        SimulatorBackend {
            devices: vec![
                Arc::new(SimDevice {
                    info: sim_info(
                        "sim-colour-0",
                        "FirstLight Simulator (colour)",
                        "SIM-1080C",
                        1920,
                        1080,
                        2.9,
                        PixelFormat::Bayer(BayerPattern::Grbg),
                    ),
                    faults: Faults::default(),
                    open: AtomicBool::new(false),
                    seed: 0x5EED_0001,
                }),
                Arc::new(SimDevice {
                    info: sim_info(
                        "sim-mono-1",
                        "FirstLight Simulator (mono)",
                        "SIM-960M",
                        1280,
                        960,
                        3.75,
                        PixelFormat::Mono,
                    ),
                    faults: Faults::default(),
                    open: AtomicBool::new(false),
                    seed: 0x5EED_0002,
                }),
            ],
        }
    }

    /// A backend with exactly one camera of the given shape. Tests use this
    /// to keep frames small and fast.
    pub fn single(width: u32, height: u32, format: PixelFormat) -> SimulatorBackend {
        SimulatorBackend {
            devices: vec![Arc::new(SimDevice {
                info: sim_info(
                    "sim-0",
                    "FirstLight Simulator",
                    "SIM-TEST",
                    width,
                    height,
                    3.0,
                    format,
                ),
                faults: Faults::default(),
                open: AtomicBool::new(false),
                seed: 0x5EED_0003,
            })],
        }
    }

    /// Fault-injection handle for the nth camera.
    pub fn handle(&self, index: usize) -> Option<SimHandle> {
        self.devices.get(index).cloned().map(SimHandle)
    }

    pub fn handle_for(&self, id: &CameraId) -> Option<SimHandle> {
        self.devices
            .iter()
            .find(|d| &d.info.id == id)
            .cloned()
            .map(SimHandle)
    }
}

fn sim_info(
    id: &str,
    display: &str,
    model: &str,
    width: u32,
    height: u32,
    pixel_um: f32,
    format: PixelFormat,
) -> CameraInfo {
    CameraInfo {
        id: CameraId::new(id),
        display_name: display.to_string(),
        model: model.to_string(),
        serial: format!("SN-{id}"),
        backend: BACKEND_NAME,
        max_width: width,
        max_height: height,
        pixel_size_um: pixel_um,
        pixel_format: format,
        bit_depths: vec![BitDepth::EIGHT, BitDepth::TWELVE, BitDepth::SIXTEEN],
        binnings: vec![Binning(1), Binning(2), Binning(4)],
        has_cooler: false,
    }
}

impl Backend for SimulatorBackend {
    fn name(&self) -> &'static str {
        BACKEND_NAME
    }

    fn enumerate(&self) -> Result<Vec<CameraInfo>> {
        Ok(self
            .devices
            .iter()
            .filter(|d| !d.faults.detached.load(Ordering::SeqCst))
            .map(|d| d.info.clone())
            .collect())
    }

    fn open(&self, id: &CameraId) -> Result<Box<dyn Camera>> {
        let dev = self
            .devices
            .iter()
            .find(|d| &d.info.id == id)
            .ok_or_else(|| Error::NotFound(id.to_string()))?;
        if dev.faults.detached.load(Ordering::SeqCst) {
            return Err(Error::NotFound(format!("{id} is not attached")));
        }
        if dev.faults.open_busy.load(Ordering::SeqCst) {
            return Err(Error::Busy(format!("{id} is held by another process")));
        }
        let mut camera = SimulatorCamera::new(dev.clone());
        camera.connect()?;
        Ok(Box::new(camera))
    }
}

/// Snapshot of everything the frame generator needs. Shared with the producer
/// thread so exposure and gain changes take effect on the next frame without
/// restarting the stream.
#[derive(Debug, Clone)]
struct SimSettings {
    roi: Roi,
    binning: Binning,
    bit_depth: BitDepth,
    format: PixelFormat,
    exposure_us: u64,
    gain: i64,
    offset: i64,
    wb: WhiteBalance,
    sensor_width: u32,
    sensor_height: u32,
}

impl SimSettings {
    fn frame_interval(&self) -> Duration {
        (Duration::from_micros(self.exposure_us) + READOUT).max(MIN_FRAME_INTERVAL)
    }
}

pub struct SimulatorCamera {
    dev: Arc<SimDevice>,
    connected: bool,
    streaming: bool,
    values: BTreeMap<ControlId, i64>,
    settings: Arc<Mutex<SimSettings>>,
    ring: Arc<FrameRing>,
    stop_flag: Arc<AtomicBool>,
    producer: Option<JoinHandle<()>>,
    watchdog_stop: Arc<AtomicBool>,
    watchdog: Option<JoinHandle<()>>,
    events_tx: Sender<CameraEvent>,
    events_rx: Receiver<CameraEvent>,
    reported_drops: u64,
}

impl SimulatorCamera {
    fn new(dev: Arc<SimDevice>) -> SimulatorCamera {
        let (events_tx, events_rx) = unbounded();
        let settings = SimSettings {
            roi: Roi::full(dev.info.max_width, dev.info.max_height),
            binning: Binning::ONE,
            bit_depth: BitDepth::SIXTEEN,
            format: dev.info.pixel_format,
            exposure_us: 20_000,
            gain: 100,
            offset: 10,
            wb: WhiteBalance {
                red: 100,
                green: 100,
                blue: 100,
            },
            sensor_width: dev.info.max_width,
            sensor_height: dev.info.max_height,
        };
        let values = BTreeMap::from([
            (ControlId::ExposureUs, 20_000),
            (ControlId::Gain, 100),
            (ControlId::Offset, 10),
            (ControlId::WbRed, 100),
            (ControlId::WbGreen, 100),
            (ControlId::WbBlue, 100),
            (ControlId::UsbBandwidth, 100),
            (ControlId::Vendor(16), 215),
        ]);
        SimulatorCamera {
            dev,
            connected: false,
            streaming: false,
            values,
            settings: Arc::new(Mutex::new(settings)),
            ring: Arc::new(FrameRing::new(3)),
            stop_flag: Arc::new(AtomicBool::new(false)),
            producer: None,
            watchdog_stop: Arc::new(AtomicBool::new(false)),
            watchdog: None,
            events_tx,
            events_rx,
            reported_drops: 0,
        }
    }

    fn settings(&self) -> SimSettings {
        self.settings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn edit_settings(&self, f: impl FnOnce(&mut SimSettings)) {
        let mut guard = self.settings.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut guard);
    }

    /// Every entry point checks this first: a call on a dead handle must fail
    /// loudly and immediately rather than time out somewhere deeper.
    fn check_alive(&self) -> Result<()> {
        if !self.connected {
            return Err(Error::NotConnected);
        }
        if self.dev.faults.device_lost.load(Ordering::SeqCst) {
            return Err(Error::DeviceLost("simulated unplug".into()));
        }
        if self.dev.faults.usb_stall.load(Ordering::SeqCst) {
            return Err(Error::UsbStall("simulated pipe stall".into()));
        }
        let latency = self.dev.faults.control_latency_ms.load(Ordering::SeqCst);
        if latency > 0 {
            thread::sleep(Duration::from_millis(latency));
        }
        Ok(())
    }

    fn control_table(&self) -> Vec<ControlInfo> {
        let colour = self.dev.info.pixel_format.is_colour();
        let mut table = vec![
            ControlInfo::new(ControlId::ExposureUs, "Exposure", 32, 60_000_000, 20_000)
                .unit("us")
                .logarithmic(true)
                .auto(true),
            ControlInfo::new(ControlId::Gain, "Gain", 100, 5_000, 100).unit("%"),
            ControlInfo::new(ControlId::Offset, "Offset", 0, 255, 10).unit("ADU"),
            ControlInfo::new(ControlId::UsbBandwidth, "USB bandwidth", 10, 100, 100).unit("%"),
            // Real cameras report things you cannot set. Having one here
            // keeps the tests honest about that.
            ControlInfo::new(ControlId::Vendor(16), "Sensor temperature", -500, 1000, 215)
                .unit("0.1C")
                .read_only(true),
        ];
        if colour {
            table.push(ControlInfo::new(ControlId::WbRed, "WB red", 25, 400, 100).unit("%"));
            table.push(ControlInfo::new(ControlId::WbGreen, "WB green", 25, 400, 100).unit("%"));
            table.push(ControlInfo::new(ControlId::WbBlue, "WB blue", 25, 400, 100).unit("%"));
        }
        table
    }

    fn spawn_producer(&mut self) {
        self.ring.reset();
        self.reported_drops = 0;
        self.stop_flag.store(false, Ordering::SeqCst);

        let ring = self.ring.clone();
        let stop_flag = self.stop_flag.clone();
        let settings = self.settings.clone();
        let dev = self.dev.clone();
        let events = self.events_tx.clone();

        self.producer = Some(thread::spawn(move || {
            let mut sequence: u64 = 0;
            let mut reported_drops = 0u64;
            let mut rng = Rng::new(dev.seed ^ 0x9E37_79B9_7F4A_7C15);
            let mut buffer: Vec<u8> = Vec::new();
            let stars = make_stars(dev.seed, 220);

            loop {
                if stop_flag.load(Ordering::SeqCst) {
                    ring.stop(StreamStop::Stopped);
                    return;
                }
                if dev.faults.device_lost.load(Ordering::SeqCst)
                    || dev.faults.detached.load(Ordering::SeqCst)
                {
                    let _ = events.send(CameraEvent::DeviceLost {
                        reason: "simulated unplug".into(),
                    });
                    ring.stop(StreamStop::DeviceLost("simulated unplug".into()));
                    return;
                }
                if dev.faults.usb_stall.load(Ordering::SeqCst) {
                    let _ = events.send(CameraEvent::UsbStall {
                        detail: "simulated pipe stall".into(),
                    });
                    ring.stop(StreamStop::UsbStall("simulated pipe stall".into()));
                    return;
                }

                let snapshot = settings.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let interval = snapshot.frame_interval();

                // Sleep in slices so a stop request, an unplug or a stall is
                // noticed within a tick even during a long exposure.
                let deadline = Instant::now() + interval;
                loop {
                    let now = Instant::now();
                    if now >= deadline {
                        break;
                    }
                    thread::sleep(TICK.min(deadline - now));
                    if stop_flag.load(Ordering::SeqCst)
                        || dev.faults.device_lost.load(Ordering::SeqCst)
                        || dev.faults.usb_stall.load(Ordering::SeqCst)
                    {
                        break;
                    }
                }
                if stop_flag.load(Ordering::SeqCst) {
                    ring.stop(StreamStop::Stopped);
                    return;
                }
                if dev.faults.freeze.load(Ordering::SeqCst) {
                    // Camera alive but delivering nothing: consumers time out.
                    continue;
                }

                render(&snapshot, &stars, sequence, &mut rng, &mut buffer);
                let meta = FrameMeta {
                    sequence,
                    timestamp: SystemTime::now(),
                    width: snapshot.roi.width,
                    height: snapshot.roi.height,
                    format: snapshot.format,
                    bit_depth: snapshot.bit_depth,
                    exposure_us: snapshot.exposure_us,
                    gain: snapshot.gain,
                    offset: snapshot.offset,
                    binning: snapshot.binning,
                    roi: snapshot.roi,
                    dropped: ring.dropped(),
                    temperature_c: Some(21.5 + (sequence % 7) as f32 * 0.05),
                };
                match Frame::new(meta, buffer.as_slice()) {
                    Ok(frame) => {
                        let dropped = ring.push(frame);
                        if dropped != reported_drops {
                            reported_drops = dropped;
                            let _ = events.send(CameraEvent::FramesDropped { total: dropped });
                        }
                    }
                    Err(e) => {
                        let _ = events.send(CameraEvent::Warning {
                            message: e.to_string(),
                        });
                    }
                }
                sequence += 1;
            }
        }));
    }

    /// Real SDKs raise a disconnect callback whether or not a stream is
    /// running, so an idle camera notices an unplug too. Mirror that with a
    /// cheap watcher thread rather than making callers poll.
    fn spawn_watchdog(&mut self) {
        self.watchdog_stop.store(false, Ordering::SeqCst);
        let stop = self.watchdog_stop.clone();
        let dev = self.dev.clone();
        let events = self.events_tx.clone();
        self.watchdog = Some(thread::spawn(move || {
            let mut announced = false;
            while !stop.load(Ordering::SeqCst) {
                let lost = dev.faults.device_lost.load(Ordering::SeqCst)
                    || dev.faults.detached.load(Ordering::SeqCst);
                if lost && !announced {
                    announced = true;
                    let _ = events.send(CameraEvent::DeviceLost {
                        reason: "simulated unplug".into(),
                    });
                }
                thread::sleep(Duration::from_millis(50));
            }
        }));
    }

    fn join_watchdog(&mut self) {
        self.watchdog_stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.watchdog.take() {
            let _ = handle.join();
        }
    }

    fn join_producer(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        self.ring.stop(StreamStop::Stopped);
        if let Some(handle) = self.producer.take() {
            let _ = handle.join();
        }
    }

    /// Geometry changes need a stream restart, exactly as on real hardware.
    fn restart_if_streaming(&mut self, change: impl FnOnce(&mut Self) -> Result<()>) -> Result<()> {
        let was_streaming = self.streaming;
        if was_streaming {
            self.stop_streaming()?;
        }
        let result = change(self);
        if was_streaming && result.is_ok() {
            self.start_streaming()?;
        }
        result
    }
}

impl Drop for SimulatorCamera {
    fn drop(&mut self) {
        self.join_producer();
        self.join_watchdog();
        self.dev.open.store(false, Ordering::SeqCst);
    }
}

impl Camera for SimulatorCamera {
    fn info(&self) -> &CameraInfo {
        &self.dev.info
    }

    fn is_connected(&self) -> bool {
        self.connected && !self.dev.faults.device_lost.load(Ordering::SeqCst)
    }

    fn connect(&mut self) -> Result<()> {
        if self.connected {
            return Ok(());
        }
        if self.dev.faults.detached.load(Ordering::SeqCst) {
            return Err(Error::NotFound(format!(
                "{} is not attached",
                self.dev.info.id
            )));
        }
        if self
            .dev
            .open
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(Error::Busy(format!("{} is already open", self.dev.info.id)));
        }
        self.connected = true;
        self.spawn_watchdog();
        let _ = self.events_tx.send(CameraEvent::Connected);
        Ok(())
    }

    fn disconnect(&mut self) -> Result<()> {
        if !self.connected {
            return Ok(());
        }
        self.join_producer();
        self.join_watchdog();
        self.streaming = false;
        self.connected = false;
        self.dev.open.store(false, Ordering::SeqCst);
        let _ = self.events_tx.send(CameraEvent::Disconnected);
        Ok(())
    }

    fn controls(&self) -> Result<Vec<ControlInfo>> {
        Ok(self.control_table())
    }

    fn control(&self, id: ControlId) -> Result<i64> {
        self.check_alive()?;
        self.values
            .get(&id)
            .copied()
            .ok_or_else(|| Error::UnknownControl(id.to_string()))
    }

    fn set_control(&mut self, id: ControlId, value: i64) -> Result<()> {
        self.check_alive()?;
        let info = self.control_info(id)?;
        if info.read_only {
            return Err(Error::Unsupported(format!("{id} is read-only")));
        }
        let value = info.validate(value)?;
        self.values.insert(id, value);
        self.edit_settings(|s| match id {
            ControlId::ExposureUs => s.exposure_us = value as u64,
            ControlId::Gain => s.gain = value,
            ControlId::Offset => s.offset = value,
            ControlId::WbRed => s.wb.red = value,
            ControlId::WbGreen => s.wb.green = value,
            ControlId::WbBlue => s.wb.blue = value,
            _ => {}
        });
        Ok(())
    }

    fn roi(&self) -> Result<Roi> {
        Ok(self.settings().roi)
    }

    fn set_roi(&mut self, roi: Roi) -> Result<()> {
        self.check_alive()?;
        let bin = self.settings().binning.factor();
        let (max_w, max_h) = (
            self.dev.info.max_width / bin,
            self.dev.info.max_height / bin,
        );
        roi.validate(max_w, max_h, self.dev.info.pixel_format.bayer().is_some())?;
        self.restart_if_streaming(|cam| {
            cam.edit_settings(|s| s.roi = roi);
            Ok(())
        })
    }

    fn binning(&self) -> Result<Binning> {
        Ok(self.settings().binning)
    }

    fn set_binning(&mut self, binning: Binning) -> Result<()> {
        self.check_alive()?;
        if !self.dev.info.binnings.contains(&binning) {
            return Err(Error::InvalidGeometry(format!(
                "binning {binning} not supported"
            )));
        }
        self.restart_if_streaming(|cam| {
            let (w, h) = (cam.dev.info.max_width, cam.dev.info.max_height);
            let factor = binning.factor();
            cam.edit_settings(|s| {
                s.binning = binning;
                // Binning changes the coordinate system the ROI lives in, so
                // the only safe thing is to go back to full frame.
                s.roi = Roi::full(w / factor, h / factor);
                // Binned pixels lose the colour filter mosaic.
                s.format = if factor > 1 {
                    PixelFormat::Mono
                } else {
                    cam.dev.info.pixel_format
                };
            });
            Ok(())
        })
    }

    fn bit_depth(&self) -> Result<BitDepth> {
        Ok(self.settings().bit_depth)
    }

    fn set_bit_depth(&mut self, depth: BitDepth) -> Result<()> {
        self.check_alive()?;
        if !self.dev.info.bit_depths.contains(&depth) {
            return Err(Error::Unsupported(format!("{depth} output")));
        }
        self.restart_if_streaming(|cam| {
            cam.edit_settings(|s| s.bit_depth = depth);
            Ok(())
        })
    }

    fn pixel_format(&self) -> Result<PixelFormat> {
        Ok(self.settings().format)
    }

    fn white_balance(&self) -> Result<WhiteBalance> {
        self.check_alive()?;
        Ok(self.settings().wb)
    }

    fn set_white_balance(&mut self, wb: WhiteBalance) -> Result<()> {
        self.set_control(ControlId::WbRed, wb.red)?;
        self.set_control(ControlId::WbGreen, wb.green)?;
        self.set_control(ControlId::WbBlue, wb.blue)
    }

    fn is_streaming(&self) -> bool {
        self.streaming
    }

    fn start_streaming(&mut self) -> Result<()> {
        self.check_alive()?;
        if self.streaming {
            return Ok(());
        }
        self.spawn_producer();
        self.streaming = true;
        let _ = self.events_tx.send(CameraEvent::StreamStarted);
        Ok(())
    }

    fn stop_streaming(&mut self) -> Result<()> {
        if !self.streaming {
            return Ok(());
        }
        // Deliberately not gated on `check_alive`: stopping a stream on a
        // camera that just vanished has to succeed, or cleanup deadlocks.
        self.join_producer();
        self.streaming = false;
        let _ = self.events_tx.send(CameraEvent::StreamStopped);
        Ok(())
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Frame> {
        if !self.connected {
            return Err(Error::NotConnected);
        }
        if !self.streaming {
            return Err(Error::NotStreaming);
        }
        let frame = self.ring.recv_timeout(timeout)?;
        let dropped = self.ring.dropped();
        if dropped != self.reported_drops {
            self.reported_drops = dropped;
        }
        Ok(frame)
    }

    fn dropped_frames(&self) -> u64 {
        self.ring.dropped()
    }

    fn temperature_c(&self) -> Result<f32> {
        self.check_alive()?;
        Ok(21.5)
    }

    fn events(&self) -> Receiver<CameraEvent> {
        self.events_rx.clone()
    }
}

// --- image synthesis ----------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct Star {
    /// Position in unbinned full-sensor pixels.
    x: f32,
    y: f32,
    flux: f32,
    /// Relative R, G, B response, so a colour sensor shows colour.
    colour: [f32; 3],
}

fn make_stars(seed: u64, count: usize) -> Vec<Star> {
    let mut rng = Rng::new(seed);
    (0..count)
        .map(|_| {
            let temp = rng.next_f32();
            Star {
                x: rng.next_f32(),
                y: rng.next_f32(),
                // Heavily skewed: a few bright stars, many faint ones.
                flux: 0.02 + rng.next_f32().powi(4) * 0.98,
                colour: [
                    0.7 + temp * 0.6,
                    0.85 + rng.next_f32() * 0.2,
                    1.3 - temp * 0.6,
                ],
            }
        })
        .collect()
}

/// Draw one frame into `buffer`, sized and laid out per `s`.
fn render(s: &SimSettings, stars: &[Star], sequence: u64, rng: &mut Rng, buffer: &mut Vec<u8>) {
    let (w, h) = (s.roi.width as usize, s.roi.height as usize);
    let spp = s.format.samples_per_pixel();
    let bps = s.bit_depth.bytes_per_sample();
    let max = s.bit_depth.max_value() as f32;
    buffer.clear();
    buffer.resize(w * h * spp * bps, 0);

    // Signal scales with exposure and gain, the way a real sensor behaves.
    let exposure_scale = (s.exposure_us as f32 / 20_000.0).clamp(0.001, 50.0);
    let gain_scale = s.gain as f32 / 100.0;
    let pedestal = (s.offset as f32 / 255.0) * 0.04;
    let bin = s.binning.factor() as f32;
    // Binning sums photons from bin^2 wells.
    let bin_gain = bin * bin;

    let mut plane = vec![0.0f32; w * h * spp];

    // Sky gradient plus read noise. Cheap, but enough that an auto-stretch
    // has something to chew on.
    let sky = 0.015 * exposure_scale;
    for y in 0..h {
        for x in 0..w {
            let grad =
                1.0 + 0.4 * (y as f32 / h.max(1) as f32) + 0.2 * (x as f32 / w.max(1) as f32);
            let noise = rng.next_f32() * 0.006 + rng.next_f32() * 0.006;
            let base = pedestal + sky * grad * bin_gain + noise;
            for c in 0..spp {
                plane[(y * w + x) * spp + c] = base;
            }
        }
    }

    // A slow drift so successive frames differ, plus per-frame seeing jitter.
    let drift = sequence as f32 * 0.02;
    let jitter_x = (rng.next_f32() - 0.5) * 1.5;
    let jitter_y = (rng.next_f32() - 0.5) * 1.5;
    let sigma = 1.1_f32;
    let two_sigma_sq = 2.0 * sigma * sigma;

    let wb = [
        s.wb.red as f32 / 100.0,
        s.wb.green as f32 / 100.0,
        s.wb.blue as f32 / 100.0,
    ];
    let bayer = s.format.bayer().map(|p| p.shifted(s.roi.x, s.roi.y));

    for star in stars {
        // Full-sensor position -> binned pixels -> ROI-relative.
        let sx = star.x * s.sensor_width as f32 + drift;
        let sy = star.y * s.sensor_height as f32 + drift * 0.6;
        let bx = sx / bin - s.roi.x as f32 + jitter_x;
        let by = sy / bin - s.roi.y as f32 + jitter_y;
        if bx < -4.0 || by < -4.0 || bx > w as f32 + 4.0 || by > h as f32 + 4.0 {
            continue;
        }
        let amplitude = star.flux * exposure_scale * bin_gain * 0.9;
        let x0 = (bx - 4.0).floor().max(0.0) as usize;
        let x1 = ((bx + 4.0).ceil() as usize).min(w.saturating_sub(1));
        let y0 = (by - 4.0).floor().max(0.0) as usize;
        let y1 = ((by + 4.0).ceil() as usize).min(h.saturating_sub(1));
        for y in y0..=y1 {
            for x in x0..=x1 {
                let dx = x as f32 + 0.5 - bx;
                let dy = y as f32 + 0.5 - by;
                let psf = (-(dx * dx + dy * dy) / two_sigma_sq).exp();
                if psf < 0.001 {
                    continue;
                }
                let idx = (y * w + x) * spp;
                match (spp, bayer) {
                    (1, Some(pattern)) => {
                        let c = pattern.channel_at(x as u32, y as u32);
                        plane[idx] += amplitude * psf * star.colour[c] * wb[c];
                    }
                    (1, None) => {
                        let lum = (star.colour[0] + star.colour[1] + star.colour[2]) / 3.0;
                        plane[idx] += amplitude * psf * lum;
                    }
                    _ => {
                        for c in 0..spp.min(3) {
                            plane[idx + c] += amplitude * psf * star.colour[c] * wb[c];
                        }
                    }
                }
            }
        }
    }

    for (i, value) in plane.iter().enumerate() {
        let scaled = (value * gain_scale * max).clamp(0.0, max) as u32;
        match bps {
            1 => buffer[i] = scaled as u8,
            _ => {
                let bytes = (scaled as u16).to_le_bytes();
                buffer[i * 2] = bytes[0];
                buffer[i * 2 + 1] = bytes[1];
            }
        }
    }
}

/// xorshift64*, deterministic and fast enough to fill a 2 MPix frame.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(if seed == 0 {
            0x1234_5678_9ABC_DEF0
        } else {
            seed
        })
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in [0, 1).
    fn next_f32(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u32 << 24) as f32
    }
}
