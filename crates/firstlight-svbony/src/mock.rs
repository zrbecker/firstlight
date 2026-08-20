//! Safe wrappers over the mock camera's test hooks.
//!
//! Only compiled with `mock-sdk`. These have no counterpart in the vendor
//! SDK; they are how a test tells the fake camera to be unplugged, to stop
//! delivering, or to reject the next control write.

unsafe extern "C" {
    fn SVB_mock_auto_save() -> i32;
    fn SVB_mock_reset();
    fn SVB_mock_unplug();
    fn SVB_mock_replug();
    fn SVB_mock_freeze(frozen: i32);
    fn SVB_mock_fail_next_control();
    fn SVB_mock_set_dropped(dropped: i32);
}

/// Put the mock camera back to its initial state.
pub fn reset() {
    // SAFETY: the hooks take no pointers and guard their own state.
    unsafe { SVB_mock_reset() }
}

/// The camera disappears: enumeration empties and calls fail with
/// `SVB_ERROR_CAMERA_REMOVED`.
pub fn unplug() {
    unsafe { SVB_mock_unplug() }
}

pub fn replug() {
    unsafe { SVB_mock_replug() }
}

/// Accept calls but never deliver a frame, so reads time out.
pub fn freeze(frozen: bool) {
    unsafe { SVB_mock_freeze(i32::from(frozen)) }
}

/// Make the next control write fail with `SVB_ERROR_GENERAL_ERROR`.
pub fn fail_next_control() {
    unsafe { SVB_mock_fail_next_control() }
}

/// Pretend the camera has dropped this many frames on the bus.
pub fn set_dropped(dropped: i32) {
    unsafe { SVB_mock_set_dropped(dropped) }
}

/// Whether the SDK would be writing parameter files to the working directory.
pub fn auto_save_enabled() -> bool {
    unsafe { SVB_mock_auto_save() != 0 }
}
