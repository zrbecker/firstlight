//! # firstlight-core
//!
//! Backend-agnostic camera capture for astronomy imaging.
//!
//! The crate is built around one trait, [`Camera`], and one rule: nothing
//! blocks forever and nothing fails silently. Every waiting call takes a
//! deadline, every failure mode a device can produce has its own [`Error`]
//! variant, and anything that happens outside a call — an unplug, a stalled
//! pipe, a dropped frame — arrives as a [`CameraEvent`].
//!
//! ```no_run
//! use std::time::Duration;
//! use firstlight_core::{Backend, Registry, simulator::SimulatorBackend};
//!
//! let backend = SimulatorBackend::new();
//! let mut camera = backend.open_first()?;
//! camera.set_exposure_us(10_000)?;
//! camera.start_streaming()?;
//! let frame = camera.next_frame(Duration::from_secs(2))?;
//! println!("{}x{} at {}", frame.width(), frame.height(), frame.meta.bit_depth);
//! camera.stop_streaming()?;
//! # Ok::<(), firstlight_core::Error>(())
//! ```
//!
//! Layout:
//!
//! * [`camera`] — the [`Camera`] and [`Backend`] traits;
//! * [`control`], [`frame`], [`event`] — the vocabulary they speak;
//! * [`ring`] — the bounded, drop-oldest frame queue every backend uses;
//! * [`worker`] — a thread that owns a camera and talks over channels, which
//!   is how the GUI stays responsive while a device misbehaves;
//! * [`format`] — SER and FITS writers;
//! * [`display`] — debayer and stretch, for screens only, never for files;
//! * [`simulator`] — a synthetic camera with fault injection.

pub mod camera;
pub mod control;
pub mod display;
pub mod error;
pub mod event;
pub mod format;
pub mod frame;
pub mod registry;
pub mod ring;
pub mod settle;
pub mod stack;
pub mod time_util;
pub mod worker;

#[cfg(feature = "simulator")]
pub mod simulator;

pub use camera::{Backend, Camera, CameraId, CameraInfo};
pub use control::{Binning, BitDepth, ControlId, ControlInfo, Roi, WhiteBalance};
pub use display::{DisplayImage, DisplayOptions, Stretch};
pub use error::{Error, Result};
pub use event::CameraEvent;
pub use format::{FitsMetadata, SerMetadata, SerWriter, write_fits};
pub use frame::{BayerPattern, Frame, FrameMeta, PixelFormat};
pub use registry::Registry;
pub use ring::{FrameRing, StreamStop};
pub use settle::SettingsClock;
pub use stack::RollingStack;
pub use worker::{
    CameraSettings, ConnectionState, RecordLimit, WorkerCommand, WorkerHandle, WorkerStatus,
    WorkerUpdate,
};
