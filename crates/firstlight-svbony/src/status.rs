//! Turning `SVB_ERROR_CODE` into [`firstlight_core::Error`].
//!
//! The SDK returns a small enum from every call. Which of those codes mean
//! "the device is gone", "you asked for something impossible" and "nothing
//! happened yet" is the difference between an application that recovers and
//! one that hangs, so the mapping lives here, gets tested, and compiles
//! without the vendor SDK present.

use std::time::Duration;

use firstlight_core::{Error, Result};

pub const SVB_SUCCESS: i32 = 0;
pub const SVB_ERROR_INVALID_INDEX: i32 = 1;
pub const SVB_ERROR_INVALID_ID: i32 = 2;
pub const SVB_ERROR_INVALID_CONTROL_TYPE: i32 = 3;
pub const SVB_ERROR_CAMERA_CLOSED: i32 = 4;
pub const SVB_ERROR_CAMERA_REMOVED: i32 = 5;
pub const SVB_ERROR_INVALID_PATH: i32 = 6;
pub const SVB_ERROR_INVALID_FILEFORMAT: i32 = 7;
pub const SVB_ERROR_INVALID_SIZE: i32 = 8;
pub const SVB_ERROR_INVALID_IMGTYPE: i32 = 9;
pub const SVB_ERROR_OUTOF_BOUNDARY: i32 = 10;
pub const SVB_ERROR_TIMEOUT: i32 = 11;
pub const SVB_ERROR_INVALID_SEQUENCE: i32 = 12;
pub const SVB_ERROR_BUFFER_TOO_SMALL: i32 = 13;
pub const SVB_ERROR_VIDEO_MODE_ACTIVE: i32 = 14;
pub const SVB_ERROR_EXPOSURE_IN_PROGRESS: i32 = 15;
pub const SVB_ERROR_GENERAL_ERROR: i32 = 16;
pub const SVB_ERROR_INVALID_MODE: i32 = 17;
pub const SVB_ERROR_INVALID_DIRECTION: i32 = 18;
pub const SVB_ERROR_UNKNOW_SENSOR_TYPE: i32 = 19;

/// "No frame yet" — the only failure that is part of normal operation.
pub fn is_timeout(code: i32) -> bool {
    code == SVB_ERROR_TIMEOUT
}

/// True when the handle is dead and must be re-opened.
pub fn is_fatal(code: i32) -> bool {
    code == SVB_ERROR_CAMERA_REMOVED
}

pub fn check(call: &'static str, code: i32) -> Result<()> {
    if code == SVB_SUCCESS {
        Ok(())
    } else {
        Err(to_error(call, code))
    }
}

/// Map a failing code, keeping the numeric value for the log.
pub fn to_error(call: &'static str, code: i32) -> Error {
    match code {
        // The camera was unplugged, or the driver lost it. Nothing on this
        // handle will work again.
        SVB_ERROR_CAMERA_REMOVED => Error::DeviceLost(format!(
            "{call}: the camera was removed (SVB_ERROR_CAMERA_REMOVED)"
        )),
        // The caller has to wait, not recover. The duration is filled in by
        // the frame path, which is the only place it is meaningful.
        SVB_ERROR_TIMEOUT => Error::Timeout(Duration::ZERO),
        SVB_ERROR_INVALID_INDEX | SVB_ERROR_INVALID_ID => {
            Error::NotFound(format!("{call}: no camera with that id"))
        }
        SVB_ERROR_CAMERA_CLOSED => Error::NotConnected,
        SVB_ERROR_INVALID_CONTROL_TYPE => {
            Error::UnknownControl(format!("{call}: the camera has no such control"))
        }
        SVB_ERROR_INVALID_SIZE | SVB_ERROR_OUTOF_BOUNDARY => Error::InvalidGeometry(format!(
            "{call}: the ROI is not a size this camera accepts \
             (width must be a multiple of 8, height a multiple of 2, and it \
             must fit the sensor)"
        )),
        SVB_ERROR_INVALID_IMGTYPE | SVB_ERROR_INVALID_MODE | SVB_ERROR_INVALID_DIRECTION => {
            Error::Unsupported(format!("{call}: not supported by this camera"))
        }
        SVB_ERROR_EXPOSURE_IN_PROGRESS => {
            Error::Busy(format!("{call}: an exposure is already in progress"))
        }
        // Both mean "you called this in the wrong order"; ours to fix, so say
        // so rather than dressing it up as a device problem.
        SVB_ERROR_INVALID_SEQUENCE | SVB_ERROR_VIDEO_MODE_ACTIVE => Error::Other(format!(
            "{call}: rejected because the video stream is running (stop it first)"
        )),
        SVB_ERROR_BUFFER_TOO_SMALL => Error::Other(format!(
            "{call}: the frame buffer was too small for the current ROI"
        )),
        SVB_ERROR_GENERAL_ERROR => Error::Other(format!(
            "{call}: rejected by the camera (SVB_ERROR_GENERAL_ERROR, usually \
             a value outside the range the camera reports)"
        )),
        other => Error::Sdk {
            backend: crate::BACKEND_NAME,
            call,
            code: other as u32,
            message: describe(other).to_string(),
        },
    }
}

pub fn describe(code: i32) -> &'static str {
    match code {
        SVB_SUCCESS => "SVB_SUCCESS",
        SVB_ERROR_INVALID_INDEX => "SVB_ERROR_INVALID_INDEX",
        SVB_ERROR_INVALID_ID => "SVB_ERROR_INVALID_ID",
        SVB_ERROR_INVALID_CONTROL_TYPE => "SVB_ERROR_INVALID_CONTROL_TYPE",
        SVB_ERROR_CAMERA_CLOSED => "SVB_ERROR_CAMERA_CLOSED",
        SVB_ERROR_CAMERA_REMOVED => "SVB_ERROR_CAMERA_REMOVED",
        SVB_ERROR_INVALID_PATH => "SVB_ERROR_INVALID_PATH",
        SVB_ERROR_INVALID_FILEFORMAT => "SVB_ERROR_INVALID_FILEFORMAT",
        SVB_ERROR_INVALID_SIZE => "SVB_ERROR_INVALID_SIZE",
        SVB_ERROR_INVALID_IMGTYPE => "SVB_ERROR_INVALID_IMGTYPE",
        SVB_ERROR_OUTOF_BOUNDARY => "SVB_ERROR_OUTOF_BOUNDARY",
        SVB_ERROR_TIMEOUT => "SVB_ERROR_TIMEOUT",
        SVB_ERROR_INVALID_SEQUENCE => "SVB_ERROR_INVALID_SEQUENCE",
        SVB_ERROR_BUFFER_TOO_SMALL => "SVB_ERROR_BUFFER_TOO_SMALL",
        SVB_ERROR_VIDEO_MODE_ACTIVE => "SVB_ERROR_VIDEO_MODE_ACTIVE",
        SVB_ERROR_EXPOSURE_IN_PROGRESS => "SVB_ERROR_EXPOSURE_IN_PROGRESS",
        SVB_ERROR_GENERAL_ERROR => "SVB_ERROR_GENERAL_ERROR",
        SVB_ERROR_INVALID_MODE => "SVB_ERROR_INVALID_MODE",
        SVB_ERROR_INVALID_DIRECTION => "SVB_ERROR_INVALID_DIRECTION",
        SVB_ERROR_UNKNOW_SENSOR_TYPE => "SVB_ERROR_UNKNOW_SENSOR_TYPE",
        _ => "unrecognised SVB_ERROR_CODE",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_is_not_an_error() {
        assert!(check("SVBOpenCamera", SVB_SUCCESS).is_ok());
    }

    #[test]
    fn an_unplugged_camera_is_fatal_and_says_why() {
        let error = to_error("SVBGetVideoData", SVB_ERROR_CAMERA_REMOVED);
        assert!(matches!(error, Error::DeviceLost(_)), "got {error:?}");
        assert!(error.is_fatal());
        assert!(is_fatal(SVB_ERROR_CAMERA_REMOVED));
    }

    #[test]
    fn a_timeout_is_transient_not_fatal() {
        let error = to_error("SVBGetVideoData", SVB_ERROR_TIMEOUT);
        assert!(matches!(error, Error::Timeout(_)), "got {error:?}");
        assert!(error.is_transient());
        assert!(!error.is_fatal());
        assert!(is_timeout(SVB_ERROR_TIMEOUT));
        assert!(!is_timeout(SVB_ERROR_GENERAL_ERROR));
    }

    #[test]
    fn a_bad_roi_explains_the_alignment_rule() {
        let error = to_error("SVBSetROIFormat", SVB_ERROR_INVALID_SIZE);
        assert!(matches!(error, Error::InvalidGeometry(_)));
        // The rule is not guessable from the SDK's own error name.
        assert!(error.to_string().contains("multiple of 8"), "{error}");
        assert!(!error.is_fatal());
    }

    #[test]
    fn calling_things_in_the_wrong_order_is_reported_as_ours_to_fix() {
        for code in [SVB_ERROR_INVALID_SEQUENCE, SVB_ERROR_VIDEO_MODE_ACTIVE] {
            let error = to_error("SVBSetROIFormat", code);
            assert!(error.to_string().contains("stop it first"), "{error}");
            assert!(!error.is_fatal());
        }
    }

    #[test]
    fn unknown_codes_keep_their_number() {
        let error = to_error("SVBOpenCamera", 99);
        match error {
            Error::Sdk { backend, code, .. } => {
                assert_eq!(backend, "svbony");
                assert_eq!(code, 99);
            }
            other => panic!("expected Error::Sdk, got {other:?}"),
        }
    }
}
