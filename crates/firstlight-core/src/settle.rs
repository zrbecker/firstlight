//! Knowing whether a frame was exposed under the settings it claims.
//!
//! A camera does not apply a setting the instant it is asked. A frame that is
//! already integrating when the exposure, gain or offset changes finishes
//! under the *old* value, and the SDK hands it over without comment. Stamp it
//! with the settings read back afterwards and the file says one thing while
//! its pixels say another — which is worse than no metadata at all, because
//! nothing about the file looks wrong.
//!
//! Measured on an SV305C Pro: three frames captured in one run after setting
//! the offset to 50 came back with black levels of 0, 3200 and 3200. Only the
//! first was still in flight when the offset landed, and all three were
//! labelled `OFFSET=50`.
//!
//! The rule this module provides: a frame describes itself only if nothing
//! changed after it began integrating. Backends know when that was and say
//! so; frames carry the answer in
//! [`crate::frame::FrameMeta::settings_settled`]; each consumer decides what
//! to do about it. The live view shows unsettled frames anyway, because a
//! briefly stale picture beats a frozen one. Anything that writes a file
//! skips them, because a file outlives the session that made it.

use std::sync::Mutex;
use std::time::Instant;

/// Tracks when a camera's settings last changed.
///
/// Cheap to share: backends keep one alongside the device handle, mark it
/// after every successful write, and ask it about each frame they build.
#[derive(Debug)]
pub struct SettingsClock {
    changed_at: Mutex<Instant>,
}

impl Default for SettingsClock {
    fn default() -> Self {
        SettingsClock::new()
    }
}

impl SettingsClock {
    pub fn new() -> SettingsClock {
        SettingsClock {
            changed_at: Mutex::new(Instant::now()),
        }
    }

    /// Record that something about the camera's configuration just changed.
    ///
    /// Call this after *any* successful write that a frame's metadata
    /// describes — exposure, gain, offset, white balance, ROI, binning, bit
    /// depth — and after opening or reconfiguring the device. Calling it too
    /// often costs at most one frame; calling it too rarely lets a
    /// mislabelled frame reach a file.
    pub fn changed(&self) {
        *self.changed_at.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
    }

    /// Whether a frame that began integrating at `began` was exposed
    /// entirely under the current settings.
    ///
    /// The caller passes when the frame *started*, rather than this working
    /// it out from the arrival time and the exposure. That was the first
    /// attempt and it is wrong: the arrival is later than exposure alone
    /// predicts — readout, transfer and scheduling all add to it — so
    /// `arrival - exposure` lands after the change and a stale frame is
    /// waved through. Measured on the simulator, a frame carrying the old
    /// gain reported itself settled.
    ///
    /// Backends know the honest answer. A free-running camera starts the next
    /// exposure as it hands over the last frame, so the previous arrival is a
    /// safe lower bound; the simulator knows exactly.
    pub fn settled_since(&self, began: Instant) -> bool {
        began >= *self.changed_at.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_frame_that_began_before_the_change_is_not_settled() {
        let clock = SettingsClock::new();
        let before = Instant::now();
        // Nothing has changed since the clock was made.
        assert!(clock.settled_since(before));

        clock.changed();
        // A frame that began before the change carries the old settings
        // however it ends up labelled.
        assert!(!clock.settled_since(before));
        // One that began afterwards is fine.
        assert!(clock.settled_since(Instant::now()));
    }

    #[test]
    fn the_start_time_is_what_counts_not_the_arrival() {
        // The bug this guards: working the start out from the arrival time
        // and the exposure lets scheduling slop wave a stale frame through,
        // because a frame always arrives later than the exposure alone
        // predicts.
        let clock = SettingsClock::new();
        let began = Instant::now();
        clock.changed();
        let arrived_much_later = began + Duration::from_secs(10);

        assert!(
            !clock.settled_since(began),
            "the frame began before the change, however long it took to arrive"
        );
        // And the late arrival does not change that.
        assert!(clock.settled_since(arrived_much_later));
    }
}
