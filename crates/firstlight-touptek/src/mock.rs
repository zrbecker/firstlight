//! Safe wrappers over the mock camera's test hooks.
//!
//! Only compiled with `mock-sdk`. These functions have no counterpart in the
//! vendor SDK; they are how a test tells the fake camera to be unplugged, to
//! stall its pipe, or to stop delivering frames.

use crate::sdk::ffi;

/// Put the mock camera back to its initial state.
pub fn reset() {
    // SAFETY: the mock's hooks take no arguments and guard their own state
    // with a mutex.
    unsafe { ffi::Toupcam_mock_reset() }
}

/// The camera disappears: enumeration goes empty and the open handle starts
/// reporting `TOUPCAM_EVENT_DISCONNECTED`.
pub fn unplug() {
    unsafe { ffi::Toupcam_mock_unplug() }
}

pub fn replug() {
    unsafe { ffi::Toupcam_mock_replug() }
}

/// The USB pipe stalls: events report `NOPACKETTIMEOUT` and pulls fail with
/// `ERROR_GEN_FAILURE`.
pub fn stall() {
    unsafe { ffi::Toupcam_mock_stall() }
}

/// The camera stops delivering frames without reporting anything.
pub fn freeze(frozen: bool) {
    unsafe { ffi::Toupcam_mock_freeze(i32::from(frozen)) }
}

/// Make the next `Toupcam_put_Option` fail with the given HRESULT.
pub fn fail_next_option(hresult: i32) {
    unsafe { ffi::Toupcam_mock_fail_next_option(hresult) }
}

/// How many times the mock has been opened, for checking reconnect logic.
pub fn open_count() -> i32 {
    unsafe { ffi::Toupcam_mock_open_count() }
}
