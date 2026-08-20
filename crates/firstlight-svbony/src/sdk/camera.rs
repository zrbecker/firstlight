//! The [`Camera`] implementation over the SVBONY SDK.
//!
//! ```text
//!   pump thread ── SVBGetVideoData(wait) ──▶ FrameRing ──▶ next_frame
//! ```
//!
//! This SDK polls rather than calling back, which suits the trait: a pump
//! thread blocks in `SVBGetVideoData` for a bounded wait and files whatever
//! it gets into a drop-oldest ring. `next_frame` only ever touches the ring,
//! so a camera that stops answering cannot hold up its caller past the
//! caller's own deadline.
//!
//! Calls into the SDK are serialised by a mutex. The vendor documents no
//! thread-safety guarantees at all, so the conservative reading is the only
//! defensible one; the cost is that a control write can queue behind an
//! in-flight frame read, which is why the read waits in short slices.

// The `as i64` casts on control values are deliberate and stay even where a
// given target makes them redundant: the SDK types those fields as C `long`,
// which is 64-bit on 64-bit Unix but 32-bit on Windows and on armhf. Without
// the cast this file compiles on a Mac and fails on a Raspberry Pi.
#![allow(clippy::unnecessary_cast)]

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

use super::ffi;
use super::sys::{self, Device};
use crate::controls;

/// How long one `SVBGetVideoData` call waits. Short slices keep the stop
/// flag and queued control writes responsive during long exposures.
const READ_SLICE_MS: i32 = 100;

/// How long to keep trying to open a camera that was recently closed.
const OPEN_RETRY_WINDOW: Duration = Duration::from_secs(5);
const OPEN_RETRY_INTERVAL: Duration = Duration::from_millis(200);

/// Pause after stopping video before anything else touches the camera.
const RESTART_SETTLE: Duration = Duration::from_millis(150);

/// How long a started stream may stay silent before the pump assumes the
/// camera did not really start and tells it again.
const SILENCE_BEFORE_KICK: Duration = Duration::from_millis(1500);

/// How many times to try that before concluding the silence is real.
const MAX_KICKS: u32 = 3;

/// How often the pump asks the SDK how many frames the camera itself lost.
const DROP_POLL_EVERY: u32 = 20;

#[derive(Debug, Clone)]
struct Geometry {
    width: u32,
    height: u32,
    format: PixelFormat,
    bit_depth: BitDepth,
    binning: Binning,
    roi: Roi,
    exposure_us: u64,
    gain: i64,
    offset: i64,
}

impl Geometry {
    fn frame_bytes(&self) -> usize {
        self.width as usize * self.height as usize * self.bit_depth.bytes_per_sample()
    }
}

struct Shared {
    device: Mutex<Device>,
    ring: Arc<FrameRing>,
    events: Sender<CameraEvent>,
    geometry: Mutex<Geometry>,
    stop: AtomicBool,
    lost: AtomicBool,
    sequence: AtomicU64,
    /// Cumulative count the SDK reports, mirrored so the UI can see it.
    sdk_dropped: AtomicU64,
}

impl Shared {
    fn device(&self) -> MutexGuard<'_, Device> {
        self.device.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn geometry(&self) -> Geometry {
        self.geometry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// How many of the top bits of a 16-bit sample carry signal, for the log.
/// The SDK scales the sensor's own depth up to fill the word.
pub struct SvbonyCamera {
    info: CameraInfo,
    shared: Arc<Shared>,
    events_rx: Receiver<CameraEvent>,
    pump: Option<JoinHandle<()>>,
    connected: bool,
    streaming: bool,
    controls: Vec<ControlInfo>,
    colour: bool,
    /// Significant bits the sensor produces in its deepest mode.
    max_bit_depth: u8,
}

impl SvbonyCamera {
    pub fn new(info: CameraInfo, device_id: i32) -> SvbonyCamera {
        let (events_tx, events_rx) = unbounded();
        let geometry = Geometry {
            width: 0,
            height: 0,
            format: PixelFormat::Mono,
            bit_depth: BitDepth::SIXTEEN,
            binning: Binning::ONE,
            roi: Roi::default(),
            exposure_us: 0,
            gain: 0,
            offset: 0,
        };
        SvbonyCamera {
            info,
            shared: Arc::new(Shared {
                device: Mutex::new(Device::new(device_id)),
                ring: Arc::new(FrameRing::new(3)),
                events: events_tx,
                geometry: Mutex::new(geometry),
                stop: AtomicBool::new(false),
                lost: AtomicBool::new(false),
                sequence: AtomicU64::new(0),
                sdk_dropped: AtomicU64::new(0),
            }),
            events_rx,
            pump: None,
            connected: false,
            streaming: false,
            controls: Vec::new(),
            colour: false,
            max_bit_depth: 16,
        }
    }

    fn check_alive(&self) -> Result<()> {
        if !self.connected {
            return Err(Error::NotConnected);
        }
        if self.shared.lost.load(Ordering::SeqCst) {
            return Err(Error::DeviceLost(
                "the camera was removed; re-open it to continue".into(),
            ));
        }
        Ok(())
    }

    /// Read the sensor's fixed properties, once, at connect.
    fn read_properties(&mut self) -> Result<()> {
        let device = self.shared.device();
        let property = device.property()?;
        self.colour = property.IsColorCam == ffi::SVB_BOOL_SVB_TRUE;
        self.max_bit_depth = property.MaxBitDepth.clamp(8, 16) as u8;

        self.info.max_width = property.MaxWidth.max(0) as u32;
        self.info.max_height = property.MaxHeight.max(0) as u32;
        self.info.pixel_format = if self.colour {
            PixelFormat::Bayer(sys::bayer_from_sdk(property.BayerPattern))
        } else {
            PixelFormat::Mono
        };
        // Only a colour sensor has anything to balance.
        self.info.has_auto_white_balance = self.colour;
        self.info.binnings = property
            .SupportedBins
            .iter()
            .take_while(|&&bin| bin != 0)
            .map(|&bin| Binning(bin.max(1) as u32))
            .collect();
        if self.info.binnings.is_empty() {
            self.info.binnings.push(Binning::ONE);
        }

        // The camera lists the containers it can output; translate those into
        // the significant-bit depths a caller can ask for.
        let mut depths = Vec::new();
        for &format in property.SupportedVideoFormat.iter() {
            if format < 0 {
                break;
            }
            let depth = if sys::bytes_per_sample(format) == 1 {
                BitDepth::EIGHT
            } else {
                // 16, not the sensor's 12: this SDK left-aligns its samples,
                // so the delivered values fill the whole 16-bit range and the
                // bottom four bits are always zero. Verified on an SV305C
                // Pro, where a neutral-white-balance frame contains only
                // multiples of 16. Reporting 12 here would make every display
                // stretch clip to white above a sixteenth of the range.
                BitDepth::SIXTEEN
            };
            if !depths.contains(&depth) {
                depths.push(depth);
            }
        }
        depths.sort();
        if depths.is_empty() {
            depths.push(BitDepth::EIGHT);
        }
        self.info.bit_depths = depths;

        if let Ok(size) = device.pixel_size_um() {
            self.info.pixel_size_um = size;
        }
        if let Ok(serial) = device.serial_number()
            && !serial.is_empty()
        {
            self.info.serial = serial;
        }
        // Cooled models expose a set-point; that is the honest test for one.
        let count = device.control_count().unwrap_or(0);
        for index in 0..count {
            if let Ok(caps) = device.control_caps(index)
                && caps.ControlType as i32 == controls::SVB_COOLER_ENABLE
            {
                self.info.has_cooler = true;
            }
        }
        Ok(())
    }

    /// Build the control table from whatever this camera reports.
    fn read_controls(&mut self) -> Result<()> {
        let device = self.shared.device();
        let count = device.control_count()?;
        let mut table = Vec::new();
        for index in 0..count {
            let caps = device.control_caps(index)?;
            let control_type = caps.ControlType as i32;
            // The camera's own label beats anything hardcoded here: it is why
            // "Frame speed" shows up correctly without this crate knowing the
            // control exists.
            let label = ffi::c_string(&caps.Name);
            let info = ControlInfo::new(
                controls::to_control_id(control_type),
                if label.is_empty() {
                    format!("control {control_type}")
                } else {
                    label
                },
                caps.MinValue as i64,
                caps.MaxValue as i64,
                caps.DefaultValue as i64,
            )
            .unit(controls::unit_for(control_type))
            .logarithmic(controls::is_logarithmic(control_type))
            .auto(caps.IsAutoSupported == ffi::SVB_BOOL_SVB_TRUE)
            .read_only(caps.IsWritable != ffi::SVB_BOOL_SVB_TRUE);
            table.push(info);
        }
        drop(device);
        self.controls = table;
        Ok(())
    }

    /// Re-read geometry from the camera rather than trusting what we asked
    /// for: ROIs get rounded and binning rescales everything.
    fn refresh_geometry(&mut self) -> Result<()> {
        let device = self.shared.device();
        let (x, y, width, height, bin) = device.roi()?;
        let image_type = device.image_type()?;
        // See `read_properties`: 16-bit output is left-aligned, so the sample
        // range is the full 16 bits whatever the sensor's own depth is.
        let bit_depth = if sys::bytes_per_sample(image_type) == 1 {
            BitDepth::EIGHT
        } else {
            BitDepth::SIXTEEN
        };
        let exposure_us = device.control(controls::SVB_EXPOSURE).unwrap_or(0).max(0) as u64;
        let gain = device.control(controls::SVB_GAIN).unwrap_or(0);
        let offset = device.control(controls::SVB_BLACK_LEVEL).unwrap_or(0);
        drop(device);

        // Binned output has no colour mosaic left in it.
        let format = if bin > 1 || !self.colour {
            PixelFormat::Mono
        } else {
            self.info.pixel_format
        };
        let mut geometry = self
            .shared
            .geometry
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *geometry = Geometry {
            width,
            height,
            format,
            bit_depth,
            binning: Binning(bin),
            roi: Roi::new(x, y, width, height),
            exposure_us,
            gain,
            offset,
        };
        Ok(())
    }

    /// This SDK wants a width that is a multiple of 8 and an even height, and
    /// reports a bare "invalid size" when it does not get one.
    fn validate_roi(&self, roi: Roi, bin: u32) -> Result<()> {
        let bin = bin.max(1);
        roi.validate(
            self.info.max_width / bin,
            self.info.max_height / bin,
            self.info.pixel_format.bayer().is_some() && bin == 1,
        )?;
        if roi.width % 8 != 0 || roi.height % 2 != 0 {
            return Err(Error::InvalidGeometry(format!(
                "{roi}: this SDK needs a width that is a multiple of 8 and an \
                 even height"
            )));
        }
        Ok(())
    }

    fn spawn_pump(&mut self) -> Result<()> {
        let shared = self.shared.clone();
        // Sized for the whole sensor at 16 bits, an upper bound on anything
        // the SDK can produce in the formats we ask for.
        let capacity =
            (self.info.max_width.max(1) as usize) * (self.info.max_height.max(1) as usize) * 2 + 64;
        self.pump = Some(
            thread::Builder::new()
                .name("svbony-pump".into())
                .spawn(move || pump(shared, capacity))
                .map_err(|e| Error::other(format!("spawning the pump thread: {e}")))?,
        );
        Ok(())
    }

    fn join_pump(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(pump) = self.pump.take() {
            let _ = pump.join();
        }
    }

    /// Geometry and format changes are rejected while video is running.
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

fn pump(shared: Arc<Shared>, capacity: usize) {
    let mut buffer = vec![0u8; capacity];
    let mut since_drop_check = 0u32;
    let mut last_delivery = Instant::now();
    let mut kicks = 0u32;

    loop {
        if shared.stop.load(Ordering::SeqCst) {
            shared.ring.stop(StreamStop::Stopped);
            return;
        }

        let geometry = shared.geometry();
        let expected = geometry.frame_bytes();
        if expected == 0 || expected > buffer.len() {
            let _ = shared.events.send(CameraEvent::Warning {
                message: format!("implausible frame geometry: {expected} bytes"),
            });
            thread::sleep(Duration::from_millis(100));
            continue;
        }

        let read = {
            let device = shared.device();
            device.read_frame(&mut buffer[..expected], READ_SLICE_MS)
        };

        match read {
            Ok(false) => {
                // Nothing ready is normal during a long exposure — but this
                // SDK also accepts SVBStartVideoCapture and then delivers
                // nothing at all, about half the time, after the stream has
                // been stopped and started. Measured on an SV305C Pro. When
                // that happens the camera is healthy and simply needs telling
                // again, so kick it rather than leaving a frozen live view.
                let quiet = last_delivery.elapsed();
                let expected = Duration::from_micros(geometry.exposure_us.saturating_mul(3))
                    .max(SILENCE_BEFORE_KICK);
                if quiet > expected && kicks < MAX_KICKS {
                    kicks += 1;
                    {
                        let device = shared.device();
                        let _ = device.stop_video();
                    }
                    thread::sleep(RESTART_SETTLE);
                    {
                        let device = shared.device();
                        if let Err(e) = device.start_video() {
                            shared.ring.stop(StreamStop::Failed(e.to_string()));
                            return;
                        }
                    }
                    last_delivery = Instant::now();
                    let _ = shared.events.send(CameraEvent::Warning {
                        message: format!(
                            "no frames for {:.1}s after starting the stream; \
                             restarted the camera's video (attempt {kicks})",
                            quiet.as_secs_f32()
                        ),
                    });
                }
                continue;
            }
            Ok(true) => {
                last_delivery = Instant::now();
                kicks = 0;
                let meta = FrameMeta {
                    sequence: shared.sequence.fetch_add(1, Ordering::Relaxed),
                    timestamp: SystemTime::now(),
                    width: geometry.width,
                    height: geometry.height,
                    format: geometry.format,
                    bit_depth: geometry.bit_depth,
                    exposure_us: geometry.exposure_us,
                    gain: geometry.gain,
                    offset: geometry.offset,
                    binning: geometry.binning,
                    roi: geometry.roi,
                    dropped: shared.ring.dropped(),
                    temperature_c: None,
                };
                match Frame::new(meta, &buffer[..expected]) {
                    Ok(frame) => {
                        let before = shared.ring.dropped();
                        let after = shared.ring.push(frame);
                        if after != before {
                            let _ = shared
                                .events
                                .send(CameraEvent::FramesDropped { total: after });
                        }
                    }
                    Err(e) => {
                        let _ = shared.events.send(CameraEvent::Warning {
                            message: e.to_string(),
                        });
                    }
                }

                // Frames the camera lost on the bus are a different failure
                // from frames we failed to consume, and worth saying so.
                since_drop_check += 1;
                if since_drop_check >= DROP_POLL_EVERY {
                    since_drop_check = 0;
                    if let Ok(dropped) = shared.device().dropped_frames() {
                        let previous = shared.sdk_dropped.swap(dropped, Ordering::SeqCst);
                        if dropped > previous {
                            let _ = shared.events.send(CameraEvent::Warning {
                                message: format!(
                                    "the camera dropped {dropped} frame(s) on the USB bus \
                                     (try a lower frame speed, a shorter cable, or a \
                                     port that is not behind a hub)"
                                ),
                            });
                        }
                    }
                }
            }
            Err(e) => {
                let fatal = e.is_fatal();
                if fatal {
                    shared.lost.store(true, Ordering::SeqCst);
                }
                let _ = shared.events.send(if fatal {
                    CameraEvent::DeviceLost {
                        reason: e.to_string(),
                    }
                } else {
                    CameraEvent::Warning {
                        message: e.to_string(),
                    }
                });
                shared.ring.stop(match &e {
                    Error::DeviceLost(reason) => StreamStop::DeviceLost(reason.clone()),
                    other => StreamStop::Failed(other.to_string()),
                });
                return;
            }
        }
    }
}

impl Camera for SvbonyCamera {
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
        self.open_with_retry()?;
        self.shared.lost.store(false, Ordering::SeqCst);
        self.connected = true;

        self.read_properties()?;
        tracing::info!(
            camera = %self.info.display_name,
            sensor_bits = self.max_bit_depth,
            "16-bit output from this SDK is left-aligned: samples fill the \
             16-bit range and carry this many significant bits"
        );
        // Free-running video rather than one of the trigger modes; a camera
        // left armed for a trigger delivers nothing and explains nothing.
        let _ = self.shared.device().set_normal_mode();

        // Stop the SDK writing a parameter file into whatever directory the
        // application happens to have been launched from, and reloading it
        // next time. That mechanism makes the camera's settings depend on the
        // working directory — two launches from different places see
        // different cameras — and leaves stray .bin files behind. What the
        // camera holds should be the only state there is.
        let _ = self.shared.device().set_auto_save(false);

        // Deepest mode the sensor offers, which is what an imager wants by
        // default; 8 bit is a deliberate choice for frame rate, not a default.
        let depth = self
            .info
            .bit_depths
            .iter()
            .copied()
            .max()
            .unwrap_or(BitDepth::EIGHT);
        let image_type = sys::image_type_for(depth, self.colour);
        self.shared.device().set_image_type(image_type)?;
        let (width, height) = (self.info.max_width, self.info.max_height);
        self.shared.device().set_roi(0, 0, width, height, 1)?;

        self.read_controls()?;
        self.refresh_geometry()?;
        self.warn_about_stored_white_balance();
        let _ = self.shared.events.send(CameraEvent::Connected);
        Ok(())
    }

    fn disconnect(&mut self) -> Result<()> {
        if !self.connected {
            return Ok(());
        }
        let _ = self.stop_streaming();
        self.shared.device().close();
        self.connected = false;
        let _ = self.shared.events.send(CameraEvent::Disconnected);
        Ok(())
    }

    fn controls(&self) -> Result<Vec<ControlInfo>> {
        Ok(self.controls.clone())
    }

    fn control(&self, id: ControlId) -> Result<i64> {
        self.check_alive()?;
        let control_type =
            controls::to_control_type(id).ok_or_else(|| Error::UnknownControl(id.to_string()))?;
        let raw = self.shared.device().control(control_type)?;
        Ok(match id {
            ControlId::TargetTemperatureMilliC => controls::temperature_to_milli_c(raw),
            _ => raw,
        })
    }

    fn set_control(&mut self, id: ControlId, value: i64) -> Result<()> {
        self.check_alive()?;
        let control_type = controls::to_control_type(id).ok_or_else(|| {
            Error::Unsupported(format!(
                "{id}: SVBONY cameras have no bandwidth limit; the equivalent \
                 is the camera's own \"Frame speed\" control"
            ))
        })?;
        // Range-check against what the camera reported, so a rejected value
        // comes back with the range rather than a bare SDK error.
        let value = match self.controls.iter().find(|c| c.id == id) {
            Some(info) => info.validate(value)?,
            None => value,
        };
        let raw = match id {
            ControlId::TargetTemperatureMilliC => controls::milli_c_to_temperature(value),
            _ => value,
        };
        self.shared.device().set_control(control_type, raw, false)?;

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

    fn control_auto(&self, id: ControlId) -> Result<bool> {
        self.check_alive()?;
        let control_type =
            controls::to_control_type(id).ok_or_else(|| Error::UnknownControl(id.to_string()))?;
        self.shared.device().control_auto(control_type)
    }

    fn set_control_auto(&mut self, id: ControlId, on: bool) -> Result<()> {
        self.check_alive()?;
        let control_type =
            controls::to_control_type(id).ok_or_else(|| Error::UnknownControl(id.to_string()))?;
        let value = self.shared.device().control(control_type)?;
        self.shared.device().set_control(control_type, value, on)
    }

    fn roi(&self) -> Result<Roi> {
        Ok(self.shared.geometry().roi)
    }

    fn set_roi(&mut self, roi: Roi) -> Result<()> {
        self.check_alive()?;
        let bin = self.shared.geometry().binning.factor();
        self.validate_roi(roi, bin)?;
        self.with_stream_stopped(|camera| {
            camera
                .shared
                .device()
                .set_roi(roi.x, roi.y, roi.width, roi.height, bin)?;
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
            // Binning rescales the coordinate system, so the only safe ROI to
            // pair it with is the full frame.
            let factor = binning.factor();
            let width = (camera.info.max_width / factor) & !7;
            let height = (camera.info.max_height / factor) & !1;
            camera
                .shared
                .device()
                .set_roi(0, 0, width, height, factor)?;
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
        let colour = self.colour && self.shared.geometry().binning.factor() == 1;
        self.with_stream_stopped(|camera| {
            camera
                .shared
                .device()
                .set_image_type(sys::image_type_for(depth, colour))?;
            camera.refresh_geometry()
        })
    }

    fn pixel_format(&self) -> Result<PixelFormat> {
        Ok(self.shared.geometry().format)
    }

    fn white_balance(&self) -> Result<WhiteBalance> {
        Ok(WhiteBalance {
            red: self.control(ControlId::WbRed)?,
            green: self.control(ControlId::WbGreen)?,
            blue: self.control(ControlId::WbBlue)?,
        })
    }

    fn auto_white_balance(&mut self) -> Result<()> {
        self.check_alive()?;
        if !self.colour {
            return Err(Error::Unsupported(
                "automatic white balance on a mono camera".into(),
            ));
        }
        // Measured on an SV305C Pro: called while video is running, this
        // either returns success without changing the gains, or leaves the
        // stream dead so every later frame times out — it takes frames of its
        // own to measure with and does not give the pipeline back. Called
        // with the stream stopped it works every time, so stop and restart
        // around it exactly as the geometry changes do.
        self.with_stream_stopped(|camera| camera.shared.device().white_balance_once())?;
        let _ = self.shared.events.send(CameraEvent::Warning {
            message: "white balance measured from the current scene and stored \
                      in the camera"
                .into(),
        });
        Ok(())
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
        self.refresh_geometry()?;
        self.shared.ring.reset();
        self.shared.stop.store(false, Ordering::SeqCst);
        self.shared.sequence.store(0, Ordering::SeqCst);
        self.shared.device().start_video()?;
        if let Err(e) = self.spawn_pump() {
            let _ = self.shared.device().stop_video();
            return Err(e);
        }
        self.streaming = true;
        let _ = self.shared.events.send(CameraEvent::StreamStarted);
        Ok(())
    }

    fn stop_streaming(&mut self) -> Result<()> {
        if !self.streaming {
            return Ok(());
        }
        self.streaming = false;
        // Stop the pump first so it is not mid-read when video stops.
        self.join_pump();
        let result = self.shared.device().stop_video();
        // Measured: restarting immediately after stopping leaves the camera
        // accepting the start and never delivering a frame, about half the
        // time. Giving it a moment to settle is the difference between a
        // geometry change that works and a live view that freezes.
        thread::sleep(RESTART_SETTLE);
        self.shared.ring.stop(StreamStop::Stopped);
        let _ = self.shared.events.send(CameraEvent::StreamStopped);
        match result {
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
        let tenths = self
            .shared
            .device()
            .control(controls::SVB_CURRENT_TEMPERATURE)?;
        Ok(tenths as f32 / 10.0)
    }

    fn events(&self) -> Receiver<CameraEvent> {
        self.events_rx.clone()
    }
}

impl SvbonyCamera {
    /// Open the camera, re-enumerating if the SDK says the id is unknown.
    ///
    /// Measured on an SV305C Pro: for about a second after the camera has
    /// been closed, opening it either blocks or fails with "no camera with
    /// that id" — and the id really has gone stale, so retrying the same one
    /// never succeeds. Re-enumerating picks up the current id and the open
    /// then works. Without this, disconnecting and reconnecting in the GUI,
    /// or reconnecting after an unplug, fails on a camera that is sitting
    /// there working.
    fn open_with_retry(&mut self) -> Result<()> {
        let deadline = Instant::now() + OPEN_RETRY_WINDOW;
        loop {
            let result = self.shared.device().open();
            let Err(error) = result else {
                return Ok(());
            };
            if Instant::now() >= deadline {
                return Err(error);
            }
            thread::sleep(OPEN_RETRY_INTERVAL);
            if let Some(handle) = sys::enumerate_handles()
                .into_iter()
                .find(|handle| handle.camera_id_matches(&self.info.id))
            {
                self.shared.device().set_id(handle.device_id);
            }
        }
    }

    /// These cameras keep white-balance gains in non-volatile memory and the
    /// SDK applies them to the raw frames, so a setting left behind by
    /// another application shows up as a colour cast in recordings made here.
    /// Worth saying out loud, because nothing else would explain it.
    fn warn_about_stored_white_balance(&self) {
        let mut stored = Vec::new();
        for id in [ControlId::WbRed, ControlId::WbGreen, ControlId::WbBlue] {
            let Some(info) = self.controls.iter().find(|c| c.id == id) else {
                continue;
            };
            let Ok(value) = self.control(id) else {
                continue;
            };
            if value != info.default {
                stored.push(format!("{} {value} (default {})", info.label, info.default));
            }
        }
        if stored.is_empty() {
            return;
        }
        let _ = self.shared.events.send(CameraEvent::Warning {
            message: format!(
                "the camera has non-default white balance stored in it: {}. \
                 The SDK applies these to the raw frames, so captures will \
                 carry the cast; set them back to the defaults for neutral \
                 raw data.",
                stored.join(", ")
            ),
        });
    }
}

impl Drop for SvbonyCamera {
    fn drop(&mut self) {
        let _ = self.disconnect();
        self.join_pump();
    }
}
