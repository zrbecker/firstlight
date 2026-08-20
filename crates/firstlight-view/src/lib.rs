//! FirstLight — live view and capture for astronomy cameras.
//!
//! The crate is a library as well as a binary so the application state can be
//! driven headlessly in tests: egui runs perfectly well with no window, so
//! `tests/headless.rs` exercises the real widgets against a simulated camera,
//! including what the UI does while the device is unplugged.
//!
//! The UI thread never touches a camera. It sends commands to
//! [`firstlight_core::worker::WorkerHandle`] and reads status back, so a
//! camera that stalls or vanishes cannot stall the window.

pub mod app;
pub mod ui;

pub use app::FirstLightApp;

use firstlight_core::registry::Registry;

/// Run the desktop application.
pub fn run(registry: Registry) -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([900.0, 600.0])
            .with_title("FirstLight"),
        ..Default::default()
    };

    eframe::run_native(
        "FirstLight",
        options,
        Box::new(move |cc| Ok(Box::new(FirstLightApp::new(&cc.egui_ctx, registry)))),
    )
}

/// Every backend this build was compiled with.
pub fn default_registry() -> Registry {
    use std::sync::Arc;
    let mut registry = Registry::new();
    #[cfg(feature = "simulator")]
    registry.push(Arc::new(firstlight_core::simulator::SimulatorBackend::new()));
    registry.push(Arc::new(firstlight_svbony::SvbonyBackend::new()));
    registry.push(Arc::new(firstlight_touptek::TouptekBackend::new()));
    registry
}
