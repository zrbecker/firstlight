//! Stand-in used when the crate is built without the `sdk` feature.
//!
//! It exists so applications can link the backend unconditionally and decide
//! at runtime what to do, instead of every caller needing `#[cfg]` blocks.
//! Enumeration returns nothing — an application with a simulator backend
//! registered alongside carries on perfectly well — while an explicit `open`
//! explains what is missing rather than failing obscurely.

use firstlight_core::camera::{Backend, Camera, CameraId, CameraInfo};
use firstlight_core::error::{Error, Result};

use crate::BACKEND_NAME;

/// The Touptek backend, compiled without the vendor SDK.
#[derive(Debug, Default, Clone, Copy)]
pub struct TouptekBackend;

impl TouptekBackend {
    pub fn new() -> TouptekBackend {
        TouptekBackend
    }

    /// Why no cameras are being reported, in words a user can act on.
    pub fn unavailable_reason() -> String {
        "this build has no Touptek SDK: rebuild with --features touptek after \
         installing the vendor SDK (see crates/firstlight-touptek/vendor/README.md)"
            .to_string()
    }
}

impl Backend for TouptekBackend {
    fn name(&self) -> &'static str {
        BACKEND_NAME
    }

    fn unavailable_reason(&self) -> Option<String> {
        Some(TouptekBackend::unavailable_reason())
    }

    fn enumerate(&self) -> Result<Vec<CameraInfo>> {
        // Not an error: a build without the SDK legitimately has no Touptek
        // cameras, and the registry should not treat that as a failure.
        Ok(Vec::new())
    }

    fn open(&self, id: &CameraId) -> Result<Box<dyn Camera>> {
        Err(Error::Unsupported(format!(
            "cannot open {id}: {}",
            TouptekBackend::unavailable_reason()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumeration_is_empty_rather_than_failing() {
        assert!(TouptekBackend::new().enumerate().unwrap().is_empty());
    }

    #[test]
    fn the_backend_explains_why_it_sees_nothing() {
        // Without this, an empty camera list in the GUI is indistinguishable
        // from a camera that is simply unplugged.
        let reason = TouptekBackend::new().unavailable_reason().unwrap();
        assert!(reason.contains("--features touptek"), "{reason}");
    }

    #[test]
    fn opening_explains_how_to_get_a_working_build() {
        let error = TouptekBackend::new()
            .open(&CameraId::new("anything"))
            .map(|_| ())
            .unwrap_err();
        assert!(matches!(error, Error::Unsupported(_)));
        assert!(error.to_string().contains("--features touptek"));
    }
}
