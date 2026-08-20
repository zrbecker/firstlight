//! The SDK's event codes and what they mean for us.
//!
//! Compiled with or without the vendor SDK. When the `sdk` feature is on, the
//! constants below are checked against the header at compile time (see
//! `sdk::ffi`), so a vendor renumbering breaks the build instead of silently
//! misreading a disconnect as a white-balance change.

use firstlight_core::CameraEvent;

pub const EVENT_EXPOSURE: u32 = 0x0001;
pub const EVENT_TEMPTINT: u32 = 0x0002;
pub const EVENT_CHROME: u32 = 0x0003;
/// A video frame is ready to be pulled.
pub const EVENT_IMAGE: u32 = 0x0004;
/// A still frame is ready to be pulled.
pub const EVENT_STILLIMAGE: u32 = 0x0005;
pub const EVENT_WBGAIN: u32 = 0x0006;
pub const EVENT_TRIGGERFAIL: u32 = 0x0007;
pub const EVENT_BLACKBALANCE: u32 = 0x0008;
pub const EVENT_FFC: u32 = 0x0009;
pub const EVENT_DFC: u32 = 0x000a;
pub const EVENT_ROI: u32 = 0x000b;
pub const EVENT_LEVELRANGE: u32 = 0x000c;
pub const EVENT_AUTOEXPO_CONV: u32 = 0x000d;
pub const EVENT_AUTOEXPO_CONVFAIL: u32 = 0x000e;
/// Generic hardware error; the stream is finished.
pub const EVENT_ERROR: u32 = 0x0080;
/// The camera was unplugged.
pub const EVENT_DISCONNECTED: u32 = 0x0081;
/// No frame arrived within the SDK's own watchdog window.
pub const EVENT_NOFRAMETIMEOUT: u32 = 0x0082;
pub const EVENT_AFFEEDBACK: u32 = 0x0083;
pub const EVENT_FOCUSPOS: u32 = 0x0084;
/// USB packets stopped arriving: a stalled pipe or a bandwidth collapse.
pub const EVENT_NOPACKETTIMEOUT: u32 = 0x0085;
pub const EVENT_EXPO_START: u32 = 0x4000;
pub const EVENT_EXPO_STOP: u32 = 0x4001;
pub const EVENT_TRIGGER_ALLOW: u32 = 0x4002;
pub const EVENT_HEARTBEAT: u32 = 0x4003;
pub const EVENT_FACTORY: u32 = 0x8001;

/// What the pump thread should do about an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Pull a frame out of the SDK.
    PullFrame,
    /// The stream is over and the handle is dead.
    Fatal(Fatal),
    /// Worth telling the user, but the stream continues.
    Notify,
    /// Nothing to do.
    Ignore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fatal {
    Disconnected,
    Stalled,
    HardwareError,
}

/// Decide what an event code means.
pub fn action(event: u32) -> Action {
    match event {
        EVENT_IMAGE | EVENT_STILLIMAGE => Action::PullFrame,
        EVENT_DISCONNECTED => Action::Fatal(Fatal::Disconnected),
        EVENT_NOPACKETTIMEOUT => Action::Fatal(Fatal::Stalled),
        EVENT_ERROR => Action::Fatal(Fatal::HardwareError),
        // A frame watchdog is recoverable: a very long exposure or a paused
        // trigger looks exactly like this.
        EVENT_NOFRAMETIMEOUT | EVENT_TRIGGERFAIL | EVENT_AUTOEXPO_CONVFAIL => Action::Notify,
        _ => Action::Ignore,
    }
}

/// The event to publish for a code, if any.
pub fn to_camera_event(event: u32) -> Option<CameraEvent> {
    Some(match event {
        EVENT_DISCONNECTED => CameraEvent::DeviceLost {
            reason: "the SDK reported the camera was disconnected".into(),
        },
        EVENT_NOPACKETTIMEOUT => CameraEvent::UsbStall {
            detail: "no USB packets received (TOUPCAM_EVENT_NOPACKETTIMEOUT)".into(),
        },
        EVENT_ERROR => CameraEvent::DeviceLost {
            reason: "the SDK reported a hardware error (TOUPCAM_EVENT_ERROR)".into(),
        },
        EVENT_NOFRAMETIMEOUT => CameraEvent::FrameTimeout,
        EVENT_TRIGGERFAIL => CameraEvent::Warning {
            message: "trigger failed (TOUPCAM_EVENT_TRIGGERFAIL)".into(),
        },
        EVENT_AUTOEXPO_CONVFAIL => CameraEvent::Warning {
            message: "auto-exposure failed to converge".into(),
        },
        _ => return None,
    })
}

/// Name for logs.
pub fn name(event: u32) -> &'static str {
    match event {
        EVENT_EXPOSURE => "EXPOSURE",
        EVENT_TEMPTINT => "TEMPTINT",
        EVENT_CHROME => "CHROME",
        EVENT_IMAGE => "IMAGE",
        EVENT_STILLIMAGE => "STILLIMAGE",
        EVENT_WBGAIN => "WBGAIN",
        EVENT_TRIGGERFAIL => "TRIGGERFAIL",
        EVENT_BLACKBALANCE => "BLACKBALANCE",
        EVENT_FFC => "FFC",
        EVENT_DFC => "DFC",
        EVENT_ROI => "ROI",
        EVENT_LEVELRANGE => "LEVELRANGE",
        EVENT_AUTOEXPO_CONV => "AUTOEXPO_CONV",
        EVENT_AUTOEXPO_CONVFAIL => "AUTOEXPO_CONVFAIL",
        EVENT_ERROR => "ERROR",
        EVENT_DISCONNECTED => "DISCONNECTED",
        EVENT_NOFRAMETIMEOUT => "NOFRAMETIMEOUT",
        EVENT_AFFEEDBACK => "AFFEEDBACK",
        EVENT_FOCUSPOS => "FOCUSPOS",
        EVENT_NOPACKETTIMEOUT => "NOPACKETTIMEOUT",
        EVENT_EXPO_START => "EXPO_START",
        EVENT_EXPO_STOP => "EXPO_STOP",
        EVENT_TRIGGER_ALLOW => "TRIGGER_ALLOW",
        EVENT_HEARTBEAT => "HEARTBEAT",
        EVENT_FACTORY => "FACTORY",
        _ => "UNKNOWN",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_image_event_asks_for_a_pull() {
        assert_eq!(action(EVENT_IMAGE), Action::PullFrame);
        assert_eq!(action(EVENT_STILLIMAGE), Action::PullFrame);
    }

    #[test]
    fn unplug_and_stall_are_fatal_but_distinguishable() {
        assert_eq!(
            action(EVENT_DISCONNECTED),
            Action::Fatal(Fatal::Disconnected)
        );
        assert_eq!(action(EVENT_NOPACKETTIMEOUT), Action::Fatal(Fatal::Stalled));
        assert_eq!(action(EVENT_ERROR), Action::Fatal(Fatal::HardwareError));

        assert!(matches!(
            to_camera_event(EVENT_DISCONNECTED),
            Some(CameraEvent::DeviceLost { .. })
        ));
        assert!(matches!(
            to_camera_event(EVENT_NOPACKETTIMEOUT),
            Some(CameraEvent::UsbStall { .. })
        ));
    }

    #[test]
    fn a_frame_watchdog_does_not_kill_the_stream() {
        // A 300 second sub-exposure trips this on some models; treating it as
        // fatal would make long exposures impossible.
        assert_eq!(action(EVENT_NOFRAMETIMEOUT), Action::Notify);
        assert_eq!(
            to_camera_event(EVENT_NOFRAMETIMEOUT),
            Some(CameraEvent::FrameTimeout)
        );
    }

    #[test]
    fn routine_events_are_ignored_without_being_mistaken_for_frames() {
        for event in [
            EVENT_EXPOSURE,
            EVENT_TEMPTINT,
            EVENT_WBGAIN,
            EVENT_HEARTBEAT,
            EVENT_EXPO_START,
            0xDEAD_BEEF,
        ] {
            assert_eq!(action(event), Action::Ignore, "event {event:#x}");
            assert!(to_camera_event(event).is_none(), "event {event:#x}");
        }
    }
}
