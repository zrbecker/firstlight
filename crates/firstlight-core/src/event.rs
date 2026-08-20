//! Asynchronous notifications a camera raises outside the call/return path.

use std::time::SystemTime;

/// Something happened to the device that no in-flight call was going to tell
/// you about. Delivered on the camera's event channel.
#[derive(Debug, Clone, PartialEq)]
pub enum CameraEvent {
    /// The device was opened successfully.
    Connected,
    /// The device was closed on request.
    Disconnected,
    /// The device vanished (unplug, bus reset, firmware crash). Every
    /// subsequent call on this handle fails with [`crate::Error::DeviceLost`].
    DeviceLost {
        reason: String,
    },
    /// A handle to the same physical camera was re-opened after a loss.
    Reconnected,
    StreamStarted,
    StreamStopped,
    /// The backend produced frames faster than they were consumed. Carries the
    /// cumulative count since the stream started, not a delta.
    FramesDropped {
        total: u64,
    },
    /// The USB pipe stalled. The stream is dead; the handle usually is too.
    UsbStall {
        detail: String,
    },
    /// A frame did not arrive within the SDK's own watchdog window.
    FrameTimeout,
    /// Sensor temperature reading, when the camera reports one.
    Temperature {
        celsius: f32,
        at: SystemTime,
    },
    /// A non-fatal backend complaint worth surfacing in the UI log.
    Warning {
        message: String,
    },
}

impl CameraEvent {
    /// True for events after which the handle must be re-opened.
    pub fn is_fatal(&self) -> bool {
        matches!(
            self,
            CameraEvent::DeviceLost { .. } | CameraEvent::UsbStall { .. }
        )
    }
}

impl std::fmt::Display for CameraEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CameraEvent::Connected => f.write_str("connected"),
            CameraEvent::Disconnected => f.write_str("disconnected"),
            CameraEvent::DeviceLost { reason } => write!(f, "device lost: {reason}"),
            CameraEvent::Reconnected => f.write_str("reconnected"),
            CameraEvent::StreamStarted => f.write_str("stream started"),
            CameraEvent::StreamStopped => f.write_str("stream stopped"),
            CameraEvent::FramesDropped { total } => write!(f, "{total} frame(s) dropped"),
            CameraEvent::UsbStall { detail } => write!(f, "USB stall: {detail}"),
            CameraEvent::FrameTimeout => f.write_str("frame timeout"),
            CameraEvent::Temperature { celsius, .. } => write!(f, "sensor at {celsius:.1} C"),
            CameraEvent::Warning { message } => write!(f, "warning: {message}"),
        }
    }
}
