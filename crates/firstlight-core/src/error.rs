//! Error taxonomy shared by every backend.
//!
//! The rule for backends: never turn a failure into a silent retry loop and
//! never return a generic error where a specific one exists. Callers make
//! recovery decisions (retry, reconnect, abort) purely from these variants.

use std::time::Duration;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A blocking call hit its deadline. Never fatal on its own: for
    /// [`crate::Camera::next_frame`] it just means no frame arrived yet.
    #[error("timed out after {0:?}")]
    Timeout(Duration),

    /// The USB pipe stalled (vendor SDK reported a transfer/pipe error).
    /// Recovery requires stopping the stream and usually re-opening the device.
    #[error("USB transfer stalled: {0}")]
    UsbStall(String),

    /// The device went away while it was open (unplugged, bus reset, powered
    /// down). The handle is dead; a new [`crate::Backend::open`] is required.
    #[error("device lost: {0}")]
    DeviceLost(String),

    /// No camera with this identifier is currently attached.
    #[error("camera not found: {0}")]
    NotFound(String),

    /// The operation needs an open device.
    #[error("camera is not connected")]
    NotConnected,

    /// The operation needs a running stream.
    #[error("camera is not streaming")]
    NotStreaming,

    /// The camera is already open, possibly by another process.
    #[error("camera is busy or already open: {0}")]
    Busy(String),

    /// The backend or the model does not implement this feature.
    #[error("unsupported by this backend: {0}")]
    Unsupported(String),

    /// A control id the camera does not expose.
    #[error("unknown control: {0}")]
    UnknownControl(String),

    /// A value outside the range the camera advertises.
    #[error("value {value} out of range for {control} ({min}..={max})")]
    OutOfRange {
        control: &'static str,
        value: i64,
        min: i64,
        max: i64,
    },

    /// A geometry request the camera cannot satisfy (odd ROI on a Bayer
    /// sensor, ROI outside the array, unsupported bin factor, ...).
    #[error("invalid geometry: {0}")]
    InvalidGeometry(String),

    /// Raw failure straight from the vendor SDK, kept verbatim so the numeric
    /// code survives into logs and bug reports.
    #[error("{backend} SDK error {code:#010x} in {call}: {message}")]
    Sdk {
        backend: &'static str,
        call: &'static str,
        code: u32,
        message: String,
    },

    /// The camera worker/IO thread died or its channel was dropped.
    #[error("camera thread disconnected")]
    ChannelClosed,

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Other(String),
}

impl Error {
    /// True when the handle is unusable and the caller should re-open the
    /// device instead of retrying the call.
    pub fn is_fatal(&self) -> bool {
        matches!(
            self,
            Error::DeviceLost(_) | Error::UsbStall(_) | Error::NotConnected | Error::ChannelClosed
        )
    }

    /// True when simply trying again later is reasonable.
    pub fn is_transient(&self) -> bool {
        matches!(self, Error::Timeout(_) | Error::Busy(_))
    }

    pub fn other(msg: impl Into<String>) -> Self {
        Error::Other(msg.into())
    }
}

impl<T> From<crossbeam_channel::SendError<T>> for Error {
    fn from(_: crossbeam_channel::SendError<T>) -> Self {
        Error::ChannelClosed
    }
}

impl From<crossbeam_channel::RecvError> for Error {
    fn from(_: crossbeam_channel::RecvError) -> Self {
        Error::ChannelClosed
    }
}
