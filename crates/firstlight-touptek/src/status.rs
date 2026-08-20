//! Turning the SDK's `HRESULT` codes into [`firstlight_core::Error`].
//!
//! Toupcam returns COM-style HRESULTs on every platform, including macOS and
//! Linux. Getting this mapping right is most of what "robust error handling"
//! means for this backend: the difference between a stalled USB pipe, a
//! camera someone else already opened, and a value the sensor cannot accept
//! is entirely in these codes, and collapsing them into one generic failure
//! makes the whole application unable to recover.
//!
//! This module deliberately has no SDK dependency so it compiles and is
//! tested without the vendor headers present.

use firstlight_core::{Error, Result};

pub const S_OK: i32 = 0x0000_0000u32 as i32;
pub const S_FALSE: i32 = 0x0000_0001u32 as i32;

pub const E_UNEXPECTED: i32 = 0x8000_FFFFu32 as i32;
pub const E_NOTIMPL: i32 = 0x8000_4001u32 as i32;
pub const E_NOINTERFACE: i32 = 0x8000_4002u32 as i32;
pub const E_POINTER: i32 = 0x8000_4003u32 as i32;
pub const E_FAIL: i32 = 0x8000_4005u32 as i32;
/// Returned when a call is made too early, or no image is ready yet.
pub const E_PENDING: i32 = 0x8000_000Au32 as i32;
pub const E_ACCESSDENIED: i32 = 0x8007_0005u32 as i32;
pub const E_OUTOFMEMORY: i32 = 0x8007_000Eu32 as i32;
pub const E_INVALIDARG: i32 = 0x8007_0057u32 as i32;
pub const RPC_E_WRONG_THREAD: i32 = 0x8001_010Eu32 as i32;

/// `ERROR_BUSY`: another process holds the camera.
pub const ERROR_BUSY: i32 = 0x8007_00AAu32 as i32;
/// `ERROR_GEN_FAILURE`: "a device attached to the system is not functioning".
/// In practice on USB this is a stalled or halted endpoint.
pub const ERROR_GEN_FAILURE: i32 = 0x8007_001Fu32 as i32;
/// `ERROR_SEM_TIMEOUT`: a USB transfer did not complete in time.
pub const ERROR_SEM_TIMEOUT: i32 = 0x8007_0079u32 as i32;
/// `ERROR_DEVICE_NOT_CONNECTED`.
pub const ERROR_DEVICE_NOT_CONNECTED: i32 = 0x8007_048Fu32 as i32;
/// `ERROR_OPERATION_ABORTED`, seen when the device is removed mid-transfer.
pub const ERROR_OPERATION_ABORTED: i32 = 0x8007_03E3u32 as i32;
/// `ERROR_FILE_NOT_FOUND`, which `Toupcam_Open` uses for "no such camera".
pub const ERROR_FILE_NOT_FOUND: i32 = 0x8007_0002u32 as i32;

pub fn succeeded(hr: i32) -> bool {
    hr >= 0
}

/// True when the call worked but produced nothing — no frame is ready yet.
/// Not an error: the caller should wait and try again.
pub fn is_no_data(hr: i32) -> bool {
    hr == S_FALSE || hr == E_PENDING
}

/// Check a return code, naming the call that produced it.
pub fn check(call: &'static str, hr: i32) -> Result<()> {
    if succeeded(hr) {
        Ok(())
    } else {
        Err(to_error(call, hr))
    }
}

/// Map a failing HRESULT onto the error taxonomy, preserving the raw code.
pub fn to_error(call: &'static str, hr: i32) -> Error {
    let code = hr as u32;
    match hr {
        // The pipe or the device itself is broken. Recovery means closing the
        // handle and re-opening, not retrying the call.
        ERROR_GEN_FAILURE | ERROR_SEM_TIMEOUT => Error::UsbStall(format!(
            "{call} failed with {code:#010x} ({})",
            describe(hr)
        )),
        ERROR_DEVICE_NOT_CONNECTED | ERROR_OPERATION_ABORTED => Error::DeviceLost(format!(
            "{call} failed with {code:#010x} ({})",
            describe(hr)
        )),
        ERROR_FILE_NOT_FOUND => Error::NotFound(format!("{call}: no such camera")),
        ERROR_BUSY | E_ACCESSDENIED => Error::Busy(format!(
            "{call}: {} (already open, or missing permissions)",
            describe(hr)
        )),
        E_NOTIMPL | E_NOINTERFACE => {
            Error::Unsupported(format!("{call} is not implemented by this camera"))
        }
        E_INVALIDARG => Error::InvalidGeometry(format!("{call} rejected its arguments")),
        _ => Error::Sdk {
            backend: crate::BACKEND_NAME,
            call,
            code,
            message: describe(hr).to_string(),
        },
    }
}

/// Human-readable name for the codes the SDK documents.
pub fn describe(hr: i32) -> &'static str {
    match hr {
        S_OK => "S_OK",
        S_FALSE => "S_FALSE",
        E_UNEXPECTED => "E_UNEXPECTED",
        E_NOTIMPL => "E_NOTIMPL",
        E_NOINTERFACE => "E_NOINTERFACE",
        E_POINTER => "E_POINTER",
        E_FAIL => "E_FAIL",
        E_PENDING => "E_PENDING",
        E_ACCESSDENIED => "E_ACCESSDENIED",
        E_OUTOFMEMORY => "E_OUTOFMEMORY",
        E_INVALIDARG => "E_INVALIDARG",
        RPC_E_WRONG_THREAD => "RPC_E_WRONG_THREAD",
        ERROR_BUSY => "ERROR_BUSY",
        ERROR_GEN_FAILURE => "ERROR_GEN_FAILURE",
        ERROR_SEM_TIMEOUT => "ERROR_SEM_TIMEOUT",
        ERROR_DEVICE_NOT_CONNECTED => "ERROR_DEVICE_NOT_CONNECTED",
        ERROR_OPERATION_ABORTED => "ERROR_OPERATION_ABORTED",
        ERROR_FILE_NOT_FOUND => "ERROR_FILE_NOT_FOUND",
        _ => "unrecognised HRESULT",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_codes_are_not_errors() {
        assert!(check("Toupcam_Open", S_OK).is_ok());
        assert!(check("Toupcam_PullImageV3", S_FALSE).is_ok());
        assert!(is_no_data(S_FALSE));
        assert!(is_no_data(E_PENDING));
        assert!(!is_no_data(E_FAIL));
    }

    #[test]
    fn a_stalled_pipe_is_distinguishable_from_a_lost_device() {
        let stall = to_error("Toupcam_PullImageV3", ERROR_GEN_FAILURE);
        assert!(matches!(stall, Error::UsbStall(_)), "got {stall:?}");
        assert!(stall.is_fatal(), "a stall needs a re-open, not a retry");

        let lost = to_error("Toupcam_PullImageV3", ERROR_DEVICE_NOT_CONNECTED);
        assert!(matches!(lost, Error::DeviceLost(_)), "got {lost:?}");
    }

    #[test]
    fn a_camera_held_by_another_process_is_reported_as_busy() {
        assert!(matches!(
            to_error("Toupcam_Open", ERROR_BUSY),
            Error::Busy(_)
        ));
        // On Linux this is what a missing udev rule looks like, so the
        // message has to mention permissions.
        let denied = to_error("Toupcam_Open", E_ACCESSDENIED);
        assert!(matches!(denied, Error::Busy(_)));
        assert!(denied.to_string().contains("permissions"));
    }

    #[test]
    fn unknown_codes_keep_the_raw_hresult_for_the_bug_report() {
        let error = to_error("Toupcam_put_Option", 0x8000_1234u32 as i32);
        match error {
            Error::Sdk {
                backend,
                call,
                code,
                ..
            } => {
                assert_eq!(backend, "touptek");
                assert_eq!(call, "Toupcam_put_Option");
                assert_eq!(code, 0x8000_1234);
            }
            other => panic!("expected Error::Sdk, got {other:?}"),
        }
        assert!(
            error.to_string().contains("0x80001234"),
            "the message must carry the code: {error}"
        );
    }

    #[test]
    fn out_of_range_arguments_are_not_reported_as_device_failures() {
        let error = to_error("Toupcam_put_Roi", E_INVALIDARG);
        assert!(matches!(error, Error::InvalidGeometry(_)));
        assert!(!error.is_fatal(), "a bad argument must not kill the handle");
    }
}
