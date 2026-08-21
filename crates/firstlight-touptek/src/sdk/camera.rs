//! The [`Camera`] implementation over the Touptek SDK.
//!
//! Frame flow, and why it is shaped this way:
//!
//! ```text
//!   SDK thread ──callback(event)──▶ channel ──▶ pump thread ──▶ FrameRing ──▶ next_frame
//! ```
//!
//! The vendor callback runs on the SDK's own thread. Doing anything slow in
//! it — pulling the image, allocating, taking a lock the UI might hold —
//! stalls the USB pipe and costs frames, so the callback does exactly one
//! thing: push an event code into an unbounded channel. A pump thread of ours
//! does the pulling and the bookkeeping, and pushes finished frames into a
//! bounded ring that drops the oldest frame when the consumer falls behind.
//!
//! `next_frame` only ever touches the ring, never the SDK handle, so a slow
//! or wedged SDK call can never block the caller past its timeout.
// The `as u32`/`as u64` casts on the vendor's constants are deliberate and
// stay even where a given header makes them redundant: bindgen picks the type
// of a `#define` from its literal form, and that has differed between SDK
// releases (`u32` in some, `i32` in others). The casts make this code compile
// against either.
#![allow(clippy::unnecessary_cast)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::{Receiver, Sender, unbounded};

use firstlight_core::camera::{Camera, CameraInfo};
use firstlight_core::control::{Binning, BitDepth, ControlId, ControlInfo, Roi, WhiteBalance};
use firstlight_core::error::{Error, Result};
use firstlight_core::event::CameraEvent;
use firstlight_core::frame::{Frame, FrameMeta, PixelFormat};
use firstlight_core::ring::{FrameRing, StreamStop};
use firstlight_core::settle::SettingsClock;

use super::ffi;
use super::sys::{self, Handle};
use crate::events::{self, Action, Fatal};
use crate::tuning;

/// How long the pump thread waits for an SDK event before checking whether it
/// has been asked to stop.
const PUMP_POLL: Duration = Duration::from_millis(200);

/// Geometry of the frames currently being produced. Updated whenever the
/// stream is (re)started, and read by the pump thread for every frame.
#[derive(Debug, Clone)]
struct Geometry {
    format: PixelFormat,
    bit_depth: BitDepth,
    binning: Binning,
    roi: Roi,
    exposure_us: u64,
    gain: i64,
    offset: i64,
}

/// State shared between the camera, the SDK callback and the pump thread.
struct Shared {
    handle: Mutex<Handle>,
    /// When the camera's configuration last changed, so frames already
    /// integrating are marked as not describing themselves.
    clock: SettingsClock,
    ring: Arc<FrameRing>,
    /// Event codes, straight from the vendor callback. Unbounded on purpose:
    /// the callback must never block or fail.
    sdk_events: Sender<u32>,
    camera_events: Sender<CameraEvent>,
    geometry: Mutex<Geometry>,
    stop: AtomicBool,
    /// Set once the device is gone; every entry point checks it so calls fail
    /// immediately instead of waiting on a handle that will never answer.
    lost: AtomicBool,
    sequence: AtomicU64,
}

impl Shared {
    fn handle(&self) -> MutexGuard<'_, Handle> {
        self.handle.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn geometry(&self) -> Geometry {
        self.geometry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// The vendor callback. Runs on the SDK's thread; must not block.
///
/// # Safety
/// `context` is the pointer handed to `Toupcam_StartPullModeWithCallback`,
/// which is a `&Shared` kept alive until after `Toupcam_Close` returns.
#[cfg(not(windows))]
unsafe extern "C" fn event_callback(event: u32, context: *mut c_void) {
    unsafe { dispatch_event(event, context) }
}

#[cfg(windows)]
unsafe extern "stdcall" fn event_callback(event: u32, context: *mut c_void) {
    unsafe { dispatch_event(event, context) }
}

/// # Safety
/// See [`event_callback`].
unsafe fn dispatch_event(event: u32, context: *mut c_void) {
    if context.is_null() {
        return;
    }
    let shared = unsafe { &*(context as *const Shared) };
    // `send` on an unbounded channel never blocks and never allocates beyond
    // one node; a closed channel just means we are shutting down.
    let _ = shared.sdk_events.send(event);
}

pub struct TouptekCamera {
    info: CameraInfo,
    shared: Arc<Shared>,
    events_rx: Receiver<CameraEvent>,
    sdk_events_rx: Option<Receiver<u32>>,
    pump: Option<JoinHandle<()>>,
    connected: bool,
    streaming: bool,
    controls: Vec<ControlInfo>,
}

impl TouptekCamera {
    pub fn new(info: CameraInfo) -> TouptekCamera {
        let (sdk_tx, sdk_rx) = unbounded();
        let (event_tx, event_rx) = unbounded();
        let geometry = Geometry {
            format: info.pixel_format,
            bit_depth: BitDepth::SIXTEEN,
            binning: Binning::ONE,
            roi: Roi::full(info.max_width, info.max_height),
            exposure_us: 0,
            gain: 100,
            offset: 0,
        };
        TouptekCamera {
            info,
            shared: Arc::new(Shared {
                handle: Mutex::new(Handle::null()),
                clock: SettingsClock::new(),
                ring: Arc::new(FrameRing::new(3)),
                sdk_events: sdk_tx,
                camera_events: event_tx,
                geometry: Mutex::new(geometry),
                stop: AtomicBool::new(false),
                lost: AtomicBool::new(false),
                sequence: AtomicU64::new(0),
            }),
            events_rx: event_rx,
            sdk_events_rx: Some(sdk_rx),
            pump: None,
            connected: false,
            streaming: false,
            controls: Vec::new(),
        }
    }

    /// Fail fast on a handle that is already known to be dead.
    fn check_alive(&self) -> Result<()> {
        if !self.connected {
            return Err(Error::NotConnected);
        }
        if self.shared.lost.load(Ordering::SeqCst) {
            return Err(Error::DeviceLost(
                "the camera was disconnected; re-open it to continue".into(),
            ));
        }
        Ok(())
    }

    /// Read the geometry back from the SDK rather than trusting what we asked
    /// for: cameras round ROIs, and binning changes the reported size.
    /// Re-read geometry from the camera. Always follows a change, so it is
    /// also where frames still in flight are marked as stale.
    fn refresh_geometry(&mut self) -> Result<()> {
        self.shared.clock.changed();
        let handle = self.shared.handle();
        let (width, height) = handle.size()?;
        let (x, y, roi_w, roi_h) = handle.roi().unwrap_or((0, 0, width, height));
        let (fourcc, bits) = handle.raw_format().unwrap_or((0, 16));
        let bit_depth = BitDepth(bits.clamp(8, 16) as u8);
        let mono = self.info.pixel_format == PixelFormat::Mono;
        let mut format = sys::pixel_format_from_fourcc(fourcc, mono);
        // Binned output is no longer mosaiced, whatever the sensor is.
        let binning = self.shared.geometry().binning;
        if binning.factor() > 1 {
            format = PixelFormat::Mono;
        }
        let exposure_us = u64::from(handle.exposure_us().unwrap_or(0));
        let gain = i64::from(handle.gain().unwrap_or(100));
        let offset = i64::from(
            handle
                .get_option(ffi::TOUPCAM_OPTION_BLACKLEVEL as u32)
                .unwrap_or(0),
        );
        drop(handle);

        let roi = if roi_w == 0 || roi_h == 0 {
            Roi::full(width, height)
        } else {
            Roi::new(x, y, roi_w, roi_h)
        };
        let mut geometry = self
            .shared
            .geometry
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *geometry = Geometry {
            format,
            bit_depth,
            binning,
            roi,
            exposure_us,
            gain,
            offset,
        };
        Ok(())
    }

    /// Build the control table from the ranges the camera reports.
    fn build_controls(&mut self) -> Result<()> {
        let handle = self.shared.handle();
        let (exp_min, exp_max, exp_def) =
            handle.exposure_range().unwrap_or((32, 60_000_000, 20_000));
        let (gain_min, gain_max, gain_def) = handle.gain_range().unwrap_or((100, 1000, 100));
        drop(handle);

        let depth = self.shared.geometry().bit_depth;
        let mut controls = vec![
            ControlInfo::new(
                ControlId::ExposureUs,
                "Exposure",
                i64::from(exp_min),
                i64::from(exp_max),
                i64::from(exp_def),
            )
            .unit("us")
            .logarithmic(true)
            .auto(true),
            ControlInfo::new(
                ControlId::Gain,
                "Gain",
                i64::from(gain_min),
                i64::from(gain_max),
                i64::from(gain_def),
            )
            .unit("%"),
            ControlInfo::new(
                ControlId::Offset,
                "Offset",
                0,
                tuning::black_level_max(depth),
                0,
            )
            .unit("ADU"),
            ControlInfo::new(ControlId::UsbBandwidth, "USB bandwidth", 1, 100, 100).unit("%"),
        ];
        if self.info.pixel_format.is_colour() {
            for (id, label) in [
                (ControlId::WbRed, "WB red"),
                (ControlId::WbGreen, "WB green"),
                (ControlId::WbBlue, "WB blue"),
            ] {
                controls.push(ControlInfo::new(id, label, 0, 227, 100).unit("%"));
            }
        }
        self.controls = controls;
        Ok(())
    }

    fn spawn_pump(&mut self) -> Result<()> {
        let Some(sdk_events) = self.sdk_events_rx.clone() else {
            return Err(Error::other("the SDK event channel is gone"));
        };
        let shared = self.shared.clone();
        // Size the buffer for the *whole sensor*, not the current ROI: a
        // geometry change racing with a pull then cannot overflow it.
        let capacity =
            (self.info.max_width.max(1) as usize) * (self.info.max_height.max(1) as usize) * 2 + 64;

        self.pump = Some(
            thread::Builder::new()
                .name("touptek-pump".into())
                .spawn(move || pump(shared, sdk_events, capacity))
                .map_err(|e| Error::other(format!("spawning the pull thread: {e}")))?,
        );
        Ok(())
    }

    fn join_pump(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        // Wake the pump if it is sitting on the event channel.
        let _ = self.shared.sdk_events.send(u32::MAX);
        if let Some(pump) = self.pump.take() {
            let _ = pump.join();
        }
    }

    /// Geometry and mode changes need the stream stopped; restart it after if
    /// it was running, so callers do not have to sequence this themselves.
    fn with_stream_stopped<T>(&mut self, change: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
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

/// Pull frames until told to stop or the device dies.
fn pump(shared: Arc<Shared>, sdk_events: Receiver<u32>, capacity: usize) {
    let mut buffer = vec![0u8; capacity];
    // When the frame now being pulled began integrating. A free-running
    // sensor starts the next exposure as it hands the last one over, so the
    // previous arrival is a safe lower bound.
    let mut exposure_began = Instant::now();

    loop {
        if shared.stop.load(Ordering::SeqCst) {
            shared.ring.stop(StreamStop::Stopped);
            return;
        }
        let event = match sdk_events.recv_timeout(PUMP_POLL) {
            Ok(event) => event,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                shared.ring.stop(StreamStop::Stopped);
                return;
            }
        };
        if event == u32::MAX {
            continue; // wake-up nudge from `join_pump`
        }

        if let Some(camera_event) = events::to_camera_event(event) {
            let _ = shared.camera_events.send(camera_event);
        }

        match events::action(event) {
            Action::Ignore | Action::Notify => continue,
            Action::Fatal(fatal) => {
                shared.lost.store(true, Ordering::SeqCst);
                let reason = format!("TOUPCAM_EVENT_{}", events::name(event));
                shared.ring.stop(match fatal {
                    Fatal::Stalled => StreamStop::UsbStall(reason),
                    Fatal::Disconnected | Fatal::HardwareError => StreamStop::DeviceLost(reason),
                });
                return;
            }
            Action::PullFrame => {}
        }

        let geometry = shared.geometry();
        let bits = tuning::pull_bits(geometry.bit_depth);
        // SAFETY: `buffer` is sized for the full sensor at 16 bits, which is
        // an upper bound on anything the SDK can write here.
        let pulled = {
            let handle = shared.handle();
            unsafe { handle.pull_image(&mut buffer, bits) }
        };

        match pulled {
            Ok(None) => continue,
            Ok(Some(info)) => {
                let (width, height) = (info.width, info.height);
                let bytes_per_sample = geometry.bit_depth.bytes_per_sample();
                let expected = width as usize * height as usize * bytes_per_sample;
                if expected == 0 || expected > buffer.len() {
                    let _ = shared.camera_events.send(CameraEvent::Warning {
                        message: format!("SDK reported an implausible frame: {width}x{height}"),
                    });
                    continue;
                }
                let meta = FrameMeta {
                    sequence: shared.sequence.fetch_add(1, Ordering::Relaxed),
                    timestamp: SystemTime::now(),
                    width,
                    height,
                    format: geometry.format,
                    bit_depth: geometry.bit_depth,
                    exposure_us: geometry.exposure_us,
                    gain: geometry.gain,
                    offset: geometry.offset,
                    binning: geometry.binning,
                    roi: geometry.roi,
                    dropped: shared.ring.dropped(),
                    temperature_c: None,
                    // This frame began integrating one exposure ago; if
                    // anything changed since, its pixels predate the metadata.
                    settings_settled: shared.clock.settled_since(exposure_began),
                };
                // The next exposure begins as this one is handed over.
                exposure_began = Instant::now();
                match Frame::new(meta, &buffer[..expected]) {
                    Ok(frame) => {
                        let before = shared.ring.dropped();
                        let after = shared.ring.push(frame);
                        if after != before {
                            let _ = shared
                                .camera_events
                                .send(CameraEvent::FramesDropped { total: after });
                        }
                    }
                    Err(e) => {
                        let _ = shared.camera_events.send(CameraEvent::Warning {
                            message: e.to_string(),
                        });
                    }
                }
            }
            Err(e) => {
                // A failed pull is either a stall or a dead device; either
                // way the stream is over and the caller must re-open.
                let fatal = e.is_fatal();
                let reason = e.to_string();
                let _ = shared.camera_events.send(match &e {
                    Error::UsbStall(detail) => CameraEvent::UsbStall {
                        detail: detail.clone(),
                    },
                    _ => CameraEvent::DeviceLost {
                        reason: reason.clone(),
                    },
                });
                if fatal {
                    shared.lost.store(true, Ordering::SeqCst);
                }
                shared.ring.stop(match e {
                    Error::UsbStall(detail) => StreamStop::UsbStall(detail),
                    Error::DeviceLost(detail) => StreamStop::DeviceLost(detail),
                    other => StreamStop::Failed(other.to_string()),
                });
                return;
            }
        }
    }
}

impl Camera for TouptekCamera {
    fn info(&self) -> &CameraInfo {
        &self.info
    }

    fn is_connected(&self) -> bool {
        self.connected && !self.shared.lost.load(Ordering::SeqCst)
    }

    fn connect(&mut self) -> Result<()> {
        if self.connected {
            return Ok(());
        }
        let handle = Handle::open(self.info.id.as_str())?;
        *self.shared.handle() = handle;
        self.shared.lost.store(false, Ordering::SeqCst);
        self.connected = true;

        {
            let handle = self.shared.handle();
            // Raw Bayer output, free-running. Both must be set before the
            // stream starts, and both are silently wrong if skipped: RGB
            // output would give us debayered data we cannot un-debayer, and
            // an armed trigger would produce no frames at all.
            handle.put_option(ffi::TOUPCAM_OPTION_RAW as u32, 1)?;
            handle.put_option(ffi::TOUPCAM_OPTION_TRIGGER as u32, 0)?;
            // The SDK's own watchdogs; without them a dead pipe looks exactly
            // like a very long exposure.
            let _ = handle.put_option(ffi::TOUPCAM_OPTION_NOFRAME_TIMEOUT as u32, 1);
            let _ = handle.put_option(ffi::TOUPCAM_OPTION_NOPACKET_TIMEOUT as u32, 1);
            if let Ok(serial) = handle.serial_number()
                && !serial.is_empty()
            {
                self.info.serial = serial;
            }
        }

        self.refresh_geometry()?;
        self.build_controls()?;
        // The Bayer phase the SDK reports beats whatever enumeration guessed.
        self.info.pixel_format = self.shared.geometry().format;
        let _ = self.shared.camera_events.send(CameraEvent::Connected);
        Ok(())
    }

    fn disconnect(&mut self) -> Result<()> {
        if !self.connected {
            return Ok(());
        }
        // Deliberately ignores failures: this path runs when the device has
        // already gone, and cleanup must still complete.
        let _ = self.stop_streaming();
        self.shared.handle().close();
        self.connected = false;
        let _ = self.shared.camera_events.send(CameraEvent::Disconnected);
        Ok(())
    }

    fn controls(&self) -> Result<Vec<ControlInfo>> {
        Ok(self.controls.clone())
    }

    fn control(&self, id: ControlId) -> Result<i64> {
        self.check_alive()?;
        let handle = self.shared.handle();
        Ok(match id {
            ControlId::ExposureUs => i64::from(handle.exposure_us()?),
            ControlId::Gain => i64::from(handle.gain()?),
            ControlId::Offset => {
                i64::from(handle.get_option(ffi::TOUPCAM_OPTION_BLACKLEVEL as u32)?)
            }
            ControlId::UsbBandwidth => {
                i64::from(handle.get_option(ffi::TOUPCAM_OPTION_BANDWIDTH as u32)?)
            }
            ControlId::WbRed => tuning::wb_gain_to_percent(handle.white_balance_gain()?[0]),
            ControlId::WbGreen => tuning::wb_gain_to_percent(handle.white_balance_gain()?[1]),
            ControlId::WbBlue => tuning::wb_gain_to_percent(handle.white_balance_gain()?[2]),
            ControlId::Vendor(option) => i64::from(handle.get_option(option)?),
            other => return Err(Error::UnknownControl(other.to_string())),
        })
    }

    fn set_control(&mut self, id: ControlId, value: i64) -> Result<()> {
        self.check_alive()?;
        // Vendor options are pass-through by definition, so they skip the
        // table; everything else is range-checked before it reaches USB.
        let value = match id {
            ControlId::Vendor(_) => value,
            _ => self.control_info(id)?.validate(value)?,
        };
        let handle = self.shared.handle();
        match id {
            ControlId::ExposureUs => {
                handle.put_exposure_us(value.clamp(0, u32::MAX as i64) as u32)?
            }
            ControlId::Gain => handle.put_gain(value.clamp(0, u16::MAX as i64) as u16)?,
            ControlId::Offset => {
                handle.put_option(ffi::TOUPCAM_OPTION_BLACKLEVEL as u32, value as i32)?
            }
            ControlId::UsbBandwidth => {
                handle.put_option(ffi::TOUPCAM_OPTION_BANDWIDTH as u32, value as i32)?
            }
            ControlId::WbRed | ControlId::WbGreen | ControlId::WbBlue => {
                let mut gains = handle.white_balance_gain()?;
                let index = match id {
                    ControlId::WbRed => 0,
                    ControlId::WbGreen => 1,
                    _ => 2,
                };
                gains[index] = tuning::wb_percent_to_gain(value);
                handle.put_white_balance_gain(gains)?;
            }
            ControlId::Vendor(option) => handle.put_option(option, value as i32)?,
            other => return Err(Error::UnknownControl(other.to_string())),
        }
        drop(handle);
        self.shared.clock.changed();

        // Keep the metadata stamped onto frames in step with reality.
        let mut geometry = self
            .shared
            .geometry
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match id {
            ControlId::ExposureUs => geometry.exposure_us = value.max(0) as u64,
            ControlId::Gain => geometry.gain = value,
            ControlId::Offset => geometry.offset = value,
            _ => {}
        }
        Ok(())
    }

    fn roi(&self) -> Result<Roi> {
        Ok(self.shared.geometry().roi)
    }

    fn set_roi(&mut self, roi: Roi) -> Result<()> {
        self.check_alive()?;
        let bin = self.shared.geometry().binning.factor();
        roi.validate(
            self.info.max_width / bin,
            self.info.max_height / bin,
            self.info.pixel_format.bayer().is_some(),
        )?;
        self.with_stream_stopped(|camera| {
            camera
                .shared
                .handle()
                .put_roi(roi.x, roi.y, roi.width, roi.height)?;
            camera.refresh_geometry()
        })
    }

    fn binning(&self) -> Result<Binning> {
        Ok(self.shared.geometry().binning)
    }

    fn set_binning(&mut self, binning: Binning) -> Result<()> {
        self.check_alive()?;
        if !self.info.binnings.contains(&binning) {
            return Err(Error::InvalidGeometry(format!(
                "binning {binning} is not supported by {}",
                self.info.display_name
            )));
        }
        self.with_stream_stopped(|camera| {
            {
                let handle = camera.shared.handle();
                // Binning must be applied with the ROI cleared, or the SDK
                // rejects a window that no longer fits the binned sensor.
                let _ = handle.put_roi(0, 0, 0, 0);
                handle.put_option(ffi::TOUPCAM_OPTION_BINNING as u32, binning.factor() as i32)?;
            }
            camera
                .shared
                .geometry
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .binning = binning;
            camera.refresh_geometry()
        })
    }

    fn bit_depth(&self) -> Result<BitDepth> {
        Ok(self.shared.geometry().bit_depth)
    }

    fn set_bit_depth(&mut self, depth: BitDepth) -> Result<()> {
        self.check_alive()?;
        if !self.info.bit_depths.contains(&depth) {
            return Err(Error::Unsupported(format!(
                "{depth} output on {}",
                self.info.display_name
            )));
        }
        self.with_stream_stopped(|camera| {
            camera.shared.handle().put_option(
                ffi::TOUPCAM_OPTION_BITDEPTH as u32,
                tuning::bitdepth_option(depth),
            )?;
            camera.refresh_geometry()?;
            // The offset range scales with bit depth, so the table changes.
            camera.build_controls()
        })
    }

    fn pixel_format(&self) -> Result<PixelFormat> {
        Ok(self.shared.geometry().format)
    }

    fn white_balance(&self) -> Result<WhiteBalance> {
        self.check_alive()?;
        let gains = self.shared.handle().white_balance_gain()?;
        Ok(WhiteBalance {
            red: tuning::wb_gain_to_percent(gains[0]),
            green: tuning::wb_gain_to_percent(gains[1]),
            blue: tuning::wb_gain_to_percent(gains[2]),
        })
    }

    fn set_white_balance(&mut self, wb: WhiteBalance) -> Result<()> {
        self.check_alive()?;
        self.shared.handle().put_white_balance_gain([
            tuning::wb_percent_to_gain(wb.red),
            tuning::wb_percent_to_gain(wb.green),
            tuning::wb_percent_to_gain(wb.blue),
        ])?;
        self.shared.clock.changed();
        Ok(())
    }

    fn is_streaming(&self) -> bool {
        self.streaming
    }

    fn start_streaming(&mut self) -> Result<()> {
        self.check_alive()?;
        if self.streaming {
            return Ok(());
        }
        self.refresh_geometry()?;
        self.shared.ring.reset();
        self.shared.stop.store(false, Ordering::SeqCst);
        self.shared.sequence.store(0, Ordering::SeqCst);
        // Drain stale event codes so a frame signalled before the last stop
        // does not trigger a pull against the new geometry.
        if let Some(rx) = &self.sdk_events_rx {
            while rx.try_recv().is_ok() {}
        }

        // The context pointer must stay valid until `Toupcam_Close` returns.
        // `self.shared` is an `Arc` this camera owns and only releases in
        // `Drop`, after `disconnect` has closed the handle.
        let context = Arc::as_ptr(&self.shared) as *mut c_void;
        // SAFETY: see above for the lifetime of `context`.
        unsafe {
            self.shared
                .handle()
                .start_pull_mode(Some(event_callback), context)?
        };

        if let Err(e) = self.spawn_pump() {
            let _ = self.shared.handle().stop();
            return Err(e);
        }
        self.streaming = true;
        let _ = self.shared.camera_events.send(CameraEvent::StreamStarted);
        Ok(())
    }

    fn stop_streaming(&mut self) -> Result<()> {
        if !self.streaming {
            return Ok(());
        }
        self.streaming = false;
        // Stop the SDK first so no further callbacks arrive, then join.
        let stop_result = self.shared.handle().stop();
        self.join_pump();
        self.shared.ring.stop(StreamStop::Stopped);
        let _ = self.shared.camera_events.send(CameraEvent::StreamStopped);
        // A stop that fails because the device already vanished is not worth
        // propagating: the stream is stopped either way.
        match stop_result {
            Err(e) if e.is_fatal() => Ok(()),
            other => other,
        }
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Frame> {
        if !self.connected {
            return Err(Error::NotConnected);
        }
        if !self.streaming {
            return Err(Error::NotStreaming);
        }
        self.shared.ring.recv_timeout(timeout)
    }

    fn dropped_frames(&self) -> u64 {
        self.shared.ring.dropped()
    }

    fn temperature_c(&self) -> Result<f32> {
        self.check_alive()?;
        self.shared.handle().temperature_c()
    }

    fn events(&self) -> Receiver<CameraEvent> {
        self.events_rx.clone()
    }
}

impl Drop for TouptekCamera {
    fn drop(&mut self) {
        // Order matters: stop the stream and close the handle (which
        // guarantees no callback is in flight) before the `Arc<Shared>` the
        // callback context points at can be released.
        let _ = self.disconnect();
        self.join_pump();
    }
}
