//! The backend-agnostic camera interface.
//!
//! The shape here is deliberately narrow so a ZWO (ASI) or QHY backend can be
//! dropped in beside Touptek without changing a line of the CLI or the GUI:
//!
//! * geometry (ROI, binning, bit depth) is separate from the numeric control
//!   table, because every SDK models those two things differently;
//! * every blocking operation takes an explicit deadline;
//! * asynchronous device trouble arrives on an event channel rather than
//!   being discovered by a call that mysteriously never returns.

use std::time::Duration;

use crossbeam_channel::Receiver;

use crate::control::{Binning, BitDepth, ControlId, ControlInfo, Roi, WhiteBalance};
use crate::error::{Error, Result};
use crate::event::CameraEvent;
use crate::frame::{BayerPattern, Frame, PixelFormat};

/// Stable identifier for a camera. Backends prefer the serial number so the
/// same physical device keeps its id across a replug; where the SDK cannot
/// supply one they fall back to the enumeration id and say so.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CameraId(pub String);

impl CameraId {
    pub fn new(s: impl Into<String>) -> Self {
        CameraId(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CameraId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What enumeration can tell you before the device is open.
#[derive(Debug, Clone, PartialEq)]
pub struct CameraInfo {
    pub id: CameraId,
    /// Name to show a human, e.g. "SVBONY SV305C Pro".
    pub display_name: String,
    pub model: String,
    /// Serial number if the SDK exposes one; empty when it does not.
    pub serial: String,
    /// Which backend produced this entry ("touptek", "simulator", ...).
    pub backend: &'static str,
    pub max_width: u32,
    pub max_height: u32,
    pub pixel_size_um: f32,
    /// Native sensor layout at bin 1.
    pub pixel_format: PixelFormat,
    /// Bit depths the camera can be switched to.
    pub bit_depths: Vec<BitDepth>,
    /// Binning factors the camera supports (always contains 1).
    pub binnings: Vec<Binning>,
    pub has_cooler: bool,
}

impl CameraInfo {
    pub fn bayer(&self) -> Option<BayerPattern> {
        self.pixel_format.bayer()
    }

    /// A stable-ish key for reconnect matching: the serial when we have one,
    /// otherwise the enumeration id.
    pub fn reconnect_key(&self) -> &str {
        if self.serial.is_empty() {
            self.id.as_str()
        } else {
            &self.serial
        }
    }
}

/// Discovery and opening. Implemented once per SDK.
///
/// `enumerate` must be safe to call while another camera from the same
/// backend is open and streaming; that is what the GUI does when it polls for
/// a device to come back after an unplug.
pub trait Backend: Send + Sync {
    /// Short lowercase name, used in ids and CLI flags.
    fn name(&self) -> &'static str;

    fn enumerate(&self) -> Result<Vec<CameraInfo>>;

    /// Open a camera by id. Returns [`Error::NotFound`] if it is not attached
    /// and [`Error::Busy`] if something else already holds it.
    fn open(&self, id: &CameraId) -> Result<Box<dyn Camera>>;

    /// Open whichever camera enumerates first; convenience for the CLI.
    fn open_first(&self) -> Result<Box<dyn Camera>> {
        let list = self.enumerate()?;
        let first = list
            .into_iter()
            .next()
            .ok_or_else(|| Error::NotFound(format!("no {} camera attached", self.name())))?;
        self.open(&first.id)
    }

    /// Why this backend cannot see any camera in this build, when that is a
    /// property of the build rather than of what is plugged in: a vendor SDK
    /// that was not compiled in, a driver that is not installed.
    ///
    /// Returning `None` means "nothing to explain". This exists so a user
    /// staring at an empty camera list can tell the difference between "no
    /// camera is attached" and "this build cannot see your camera at all",
    /// which otherwise look identical and waste an evening.
    fn unavailable_reason(&self) -> Option<String> {
        None
    }

    /// Re-find a camera after a replug using [`CameraInfo::reconnect_key`].
    fn find_by_key(&self, key: &str) -> Result<Option<CameraInfo>> {
        Ok(self
            .enumerate()?
            .into_iter()
            .find(|c| c.reconnect_key() == key))
    }
}

/// An open (or openable) camera.
///
/// Implementations are `Send` but not `Sync`: one thread owns the device.
/// Cross-thread use goes through [`crate::worker::CameraWorker`].
pub trait Camera: Send {
    fn info(&self) -> &CameraInfo;

    fn is_connected(&self) -> bool;

    /// Open the device. Idempotent: connecting an already connected camera is
    /// a no-op, not an error.
    fn connect(&mut self) -> Result<()>;

    /// Close the device, stopping the stream first if needed. Idempotent, and
    /// must not fail just because the device already vanished.
    fn disconnect(&mut self) -> Result<()>;

    // --- controls -------------------------------------------------------

    /// Everything this camera exposes, with ranges. The GUI builds its
    /// sliders from exactly this list.
    fn controls(&self) -> Result<Vec<ControlInfo>>;

    fn control(&self, id: ControlId) -> Result<i64>;

    fn set_control(&mut self, id: ControlId, value: i64) -> Result<()>;

    /// Whether the camera is driving this control itself.
    fn control_auto(&self, _id: ControlId) -> Result<bool> {
        Ok(false)
    }

    fn set_control_auto(&mut self, id: ControlId, _on: bool) -> Result<()> {
        Err(Error::Unsupported(format!("auto mode for {id}")))
    }

    // --- geometry -------------------------------------------------------

    fn roi(&self) -> Result<Roi>;

    /// Set the region of interest, in binned pixels. Changing geometry while
    /// streaming is not portable; implementations stop and restart the stream
    /// around the change, or return [`Error::Unsupported`].
    fn set_roi(&mut self, roi: Roi) -> Result<()>;

    fn binning(&self) -> Result<Binning>;

    fn set_binning(&mut self, binning: Binning) -> Result<()>;

    fn bit_depth(&self) -> Result<BitDepth>;

    fn set_bit_depth(&mut self, depth: BitDepth) -> Result<()>;

    /// The layout frames will actually arrive in, given the current geometry.
    fn pixel_format(&self) -> Result<PixelFormat>;

    fn white_balance(&self) -> Result<WhiteBalance> {
        Err(Error::Unsupported("white balance".into()))
    }

    fn set_white_balance(&mut self, _wb: WhiteBalance) -> Result<()> {
        Err(Error::Unsupported("white balance".into()))
    }

    /// Balance the colour channels by measuring frames and adjusting the
    /// camera's white balance gains.
    ///
    /// This changes the camera, not the display: frames captured afterwards
    /// are balanced, which is the difference between fixing a colour cast and
    /// hiding one. Point the camera at something neutral first, or it will
    /// faithfully balance for whatever it can see.
    ///
    /// Implemented by measuring rather than by asking the vendor SDK, because
    /// vendor implementations cannot be relied on: SVBONY's writes the same
    /// fixed triple whatever the camera is looking at, which is a factory
    /// calibration rather than a measurement. Closing the loop on the picture
    /// works on any camera that exposes the gains.
    fn auto_white_balance(&mut self) -> Result<()> {
        const PASSES: u32 = 3;
        const TOLERANCE: f64 = 0.02;
        const TIMEOUT: Duration = Duration::from_secs(5);

        let controls = self.controls()?;
        let channels = [ControlId::WbRed, ControlId::WbGreen, ControlId::WbBlue];
        if !channels
            .iter()
            .all(|id| controls.iter().any(|control| control.id == *id))
        {
            return Err(Error::Unsupported(
                "this camera has no white balance gains".into(),
            ));
        }

        let was_streaming = self.is_streaming();
        if !was_streaming {
            self.start_streaming()?;
        }

        let outcome = (|| -> Result<()> {
            for _ in 0..PASSES {
                // Measure a frame that was actually exposed under the gains
                // just written, rather than one still in flight from before.
                let frame = self.next_settled_frame(TIMEOUT)?;
                let means = frame.channel_means().ok_or_else(|| {
                    Error::Unsupported("a mono camera has nothing to balance".into())
                })?;
                // Green is the reference: a Bayer sensor has most of it, and
                // leaving it alone keeps the overall level where it was.
                let green = means[1].max(1.0);
                let mut changed = false;
                for (index, id) in [(0usize, ControlId::WbRed), (2, ControlId::WbBlue)] {
                    let correction = green / means[index].max(1.0);
                    if (correction - 1.0).abs() < TOLERANCE {
                        continue;
                    }
                    let Some(info) = controls.iter().find(|c| c.id == id) else {
                        continue;
                    };
                    let current = self.control(id)?;
                    let target = ((current.max(1) as f64) * correction).round() as i64;
                    let target = target.clamp(info.min.max(1), info.max);
                    if target != current {
                        self.set_control(id, target)?;
                        changed = true;
                    }
                }
                if !changed {
                    break;
                }
            }
            Ok(())
        })();

        if !was_streaming {
            let _ = self.stop_streaming();
        }
        outcome
    }

    // --- streaming ------------------------------------------------------

    fn is_streaming(&self) -> bool;

    fn start_streaming(&mut self) -> Result<()>;

    /// Stop the stream. Idempotent, and safe to call on a lost device.
    fn stop_streaming(&mut self) -> Result<()>;

    /// Wait for the next frame.
    ///
    /// Returns [`Error::Timeout`] when nothing arrived within `timeout` — a
    /// normal outcome during long exposures, not a failure. Any other error
    /// means the stream is broken. This call must never block longer than
    /// `timeout`, whatever the SDK is doing underneath.
    fn next_frame(&mut self, timeout: Duration) -> Result<Frame>;

    /// Frames the backend produced but nobody consumed, since stream start.
    fn dropped_frames(&self) -> u64;

    /// Sensor temperature, when the camera has a sensor for it.
    fn temperature_c(&self) -> Result<f32> {
        Err(Error::Unsupported("temperature readout".into()))
    }

    /// Receiver for asynchronous device events.
    ///
    /// Single-consumer: the returned receiver shares one queue, so clones
    /// steal from each other. In practice [`crate::worker::CameraWorker`] is
    /// the only consumer and re-publishes what the UI needs.
    fn events(&self) -> Receiver<CameraEvent>;

    // --- provided conveniences -----------------------------------------

    fn exposure_us(&self) -> Result<u64> {
        Ok(self.control(ControlId::ExposureUs)?.max(0) as u64)
    }

    fn set_exposure_us(&mut self, us: u64) -> Result<()> {
        self.set_control(ControlId::ExposureUs, us as i64)
    }

    fn gain(&self) -> Result<i64> {
        self.control(ControlId::Gain)
    }

    fn set_gain(&mut self, gain: i64) -> Result<()> {
        self.set_control(ControlId::Gain, gain)
    }

    fn offset(&self) -> Result<i64> {
        self.control(ControlId::Offset)
    }

    fn set_offset(&mut self, offset: i64) -> Result<()> {
        self.set_control(ControlId::Offset, offset)
    }

    fn control_info(&self, id: ControlId) -> Result<ControlInfo> {
        self.controls()?
            .into_iter()
            .find(|c| c.id == id)
            .ok_or_else(|| Error::UnknownControl(id.to_string()))
    }

    /// Wait for a frame that was exposed under the current settings.
    ///
    /// A frame already integrating when a setting changed finishes under the
    /// old value, so anything that measures or saves a frame wants this
    /// rather than [`Camera::next_frame`]. The live view deliberately does
    /// not: a briefly stale picture is better than a frozen one.
    ///
    /// `timeout` applies to each wait, so the total is bounded by roughly one
    /// exposure beyond it.
    fn next_settled_frame(&mut self, timeout: Duration) -> Result<Frame> {
        loop {
            let frame = self.next_frame(timeout)?;
            if frame.meta.settings_settled {
                return Ok(frame);
            }
        }
    }

    /// Grab a single frame: start the stream if needed, wait, stop again.
    ///
    /// The deadline covers the whole operation, so a caller asking for one
    /// 60 s sub can pass 70 s and be sure the call returns.
    fn snap(&mut self, timeout: Duration) -> Result<Frame> {
        let was_streaming = self.is_streaming();
        if !was_streaming {
            self.start_streaming()?;
        }
        // Settled, because a snapshot gets written to a file and a file
        // that misdescribes itself outlives the session that made it.
        let result = self.next_settled_frame(timeout);
        if !was_streaming {
            // Stop even when the grab failed; leaving the sensor running after
            // a failed snap is how you end up with a device you cannot reopen.
            let _ = self.stop_streaming();
        }
        result
    }
}
