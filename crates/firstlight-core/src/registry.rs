//! A place to hang every compiled-in backend so the apps do not have to
//! know which ones exist.

use std::sync::Arc;

use crate::camera::{Backend, Camera, CameraId, CameraInfo};
use crate::error::{Error, Result};

/// Set of backends the binary was built with.
#[derive(Clone, Default)]
pub struct Registry {
    backends: Vec<Arc<dyn Backend>>,
}

impl Registry {
    pub fn new() -> Self {
        Registry::default()
    }

    /// The set an application gets by default: whatever features are on.
    pub fn with_defaults() -> Self {
        let reg = Registry::new();
        #[cfg(feature = "simulator")]
        let reg = reg.with(Arc::new(crate::simulator::SimulatorBackend::new()));
        reg
    }

    pub fn with(mut self, backend: Arc<dyn Backend>) -> Self {
        self.backends.push(backend);
        self
    }

    pub fn push(&mut self, backend: Arc<dyn Backend>) {
        self.backends.push(backend);
    }

    pub fn backends(&self) -> &[Arc<dyn Backend>] {
        &self.backends
    }

    pub fn backend(&self, name: &str) -> Option<Arc<dyn Backend>> {
        self.backends.iter().find(|b| b.name() == name).cloned()
    }

    /// Enumerate every backend.
    ///
    /// One backend failing (SDK missing, permissions, a wedged driver) must
    /// not hide the cameras the others found, so failures are returned
    /// alongside the results instead of short-circuiting.
    pub fn enumerate(&self) -> (Vec<CameraInfo>, Vec<(&'static str, Error)>) {
        let mut cameras = Vec::new();
        let mut errors = Vec::new();
        for backend in &self.backends {
            match backend.enumerate() {
                Ok(mut list) => cameras.append(&mut list),
                Err(e) => errors.push((backend.name(), e)),
            }
        }
        (cameras, errors)
    }

    /// Enumerate, discarding per-backend failures.
    pub fn enumerate_ok(&self) -> Vec<CameraInfo> {
        self.enumerate().0
    }

    pub fn open(&self, backend: &str, id: &CameraId) -> Result<Box<dyn Camera>> {
        self.backend(backend)
            .ok_or_else(|| Error::NotFound(format!("backend {backend}")))?
            .open(id)
    }

    /// Open by camera id alone, searching every backend for a match.
    pub fn open_any(&self, id: &CameraId) -> Result<Box<dyn Camera>> {
        for backend in &self.backends {
            if let Ok(list) = backend.enumerate()
                && list.iter().any(|c| &c.id == id)
            {
                return backend.open(id);
            }
        }
        Err(Error::NotFound(id.to_string()))
    }
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field(
                "backends",
                &self.backends.iter().map(|b| b.name()).collect::<Vec<_>>(),
            )
            .finish()
    }
}
