//! Stand-in used when the crate is built without a vendor SDK.
//!
//! Applications can link the backend unconditionally and decide at runtime;
//! enumeration returns nothing, and says why when asked.

use firstlight_core::camera::{Backend, Camera, CameraId, CameraInfo};
use firstlight_core::error::{Error, Result};

use crate::BACKEND_NAME;

#[derive(Debug, Default, Clone, Copy)]
pub struct SvbonyBackend;

impl SvbonyBackend {
    pub fn new() -> SvbonyBackend {
        SvbonyBackend
    }

    pub fn unavailable_reason() -> String {
        "this build has no SVBONY SDK: rebuild with --features svbony (the SDK \
         is downloaded automatically)"
            .to_string()
    }
}

impl Backend for SvbonyBackend {
    fn name(&self) -> &'static str {
        BACKEND_NAME
    }

    fn unavailable_reason(&self) -> Option<String> {
        Some(SvbonyBackend::unavailable_reason())
    }

    fn enumerate(&self) -> Result<Vec<CameraInfo>> {
        Ok(Vec::new())
    }

    fn open(&self, id: &CameraId) -> Result<Box<dyn Camera>> {
        Err(Error::Unsupported(format!(
            "cannot open {id}: {}",
            SvbonyBackend::unavailable_reason()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_backend_explains_itself_instead_of_erroring() {
        let backend = SvbonyBackend::new();
        assert!(backend.enumerate().unwrap().is_empty());
        let reason = backend.unavailable_reason().unwrap();
        assert!(reason.contains("--features svbony"), "{reason}");
    }
}
