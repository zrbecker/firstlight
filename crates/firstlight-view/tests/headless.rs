//! Drives the real widgets with no window.
//!
//! egui does not need a GPU or a display to run a frame, so these tests
//! exercise the same code the application does: the panels, the worker
//! commands they send, and above all what the UI does while a camera is
//! misbehaving. The central claim of this application — the window stays
//! responsive when the hardware does not — is checked here by timing frames
//! while the camera is unplugged.

use std::sync::Arc;
use std::time::{Duration, Instant};

use firstlight_core::camera::Backend;
use firstlight_core::frame::{BayerPattern, PixelFormat};
use firstlight_core::registry::Registry;
use firstlight_core::simulator::{SimHandle, SimulatorBackend};
use firstlight_core::worker::{ConnectionState, WorkerCommand};
use firstlight_view::FirstLightApp;
use firstlight_view::app::LogKind;

struct Harness {
    ctx: egui::Context,
    app: FirstLightApp,
    sim: SimHandle,
}

impl Harness {
    fn new() -> Harness {
        let backend = Arc::new(SimulatorBackend::single(
            64,
            48,
            PixelFormat::Bayer(BayerPattern::Rggb),
        ));
        let sim = backend.handle(0).unwrap();
        let registry = Registry::new().with(backend as Arc<dyn Backend>);
        let ctx = egui::Context::default();
        let app = FirstLightApp::new(&ctx, registry);
        Harness { ctx, app, sim }
    }

    /// Run one full frame: bookkeeping, then the panels, exactly as eframe
    /// does. Returns how long it took.
    fn frame(&mut self) -> Duration {
        let started = Instant::now();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 820.0),
            )),
            ..Default::default()
        };
        let app = &mut self.app;
        let mut output = self.ctx.run_ui(input, |ui| {
            app.tick(ui.ctx());
            firstlight_view::ui::left_panel(app, ui);
            firstlight_view::ui::log_panel(app, ui);
            firstlight_view::ui::central_panel(app, ui);
        });
        // A real backend uploads these to the GPU; here there is nothing to
        // upload to, and epaint insists they be acknowledged either way.
        output.textures_delta.clear();
        started.elapsed()
    }

    /// Run frames until `predicate` holds, or fail with the log.
    fn run_until(&mut self, what: &str, mut predicate: impl FnMut(&FirstLightApp) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            self.frame();
            if predicate(&self.app) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "timed out waiting for {what}. State: {:?}. Log:\n{}",
            self.app.status.state,
            self.log_text()
        );
    }

    fn log_text(&self) -> String {
        self.app
            .log
            .iter()
            .map(|l| format!("  [{:?}] {}", l.kind, l.text))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn errors(&self) -> Vec<String> {
        self.app
            .log
            .iter()
            .filter(|l| l.kind == LogKind::Error)
            .map(|l| l.text.clone())
            .collect()
    }

    fn connect(&mut self) {
        self.run_until("a camera to enumerate", |app| !app.cameras.is_empty());
        let id = self.app.selected.clone().expect("a camera to be selected");
        self.app.send(WorkerCommand::Connect(id));
        self.run_until("the camera to connect", |app| app.connected());
    }
}

#[test]
fn the_app_starts_and_finds_the_simulated_camera() {
    let mut harness = Harness::new();
    harness.run_until("enumeration", |app| !app.cameras.is_empty());
    assert_eq!(harness.app.cameras.len(), 1);
    assert!(harness.app.selected.is_some(), "a camera is preselected");
    assert!(harness.errors().is_empty(), "{}", harness.log_text());
}

#[test]
fn connecting_populates_the_control_sliders() {
    let mut harness = Harness::new();
    harness.connect();
    harness.run_until("the control table", |app| !app.controls.is_empty());

    let controls = &harness.app.controls;
    assert!(
        controls
            .iter()
            .any(|c| c.id == firstlight_core::ControlId::ExposureUs)
    );
    // Every slider must have a value to sit at, or the first frame panics.
    for control in controls {
        assert!(
            harness.app.values.contains_key(&control.id),
            "no value for {}",
            control.id
        );
    }
}

#[test]
fn starting_the_live_view_produces_a_texture() {
    let mut harness = Harness::new();
    harness.connect();
    harness.app.send(WorkerCommand::StartStream);
    harness.run_until("a frame on screen", |app| app.texture.is_some());

    let meta = harness.app.last_meta.clone().expect("frame metadata");
    assert_eq!((meta.width, meta.height), (64, 48));
    // Bayer data is debayered to half size for display.
    let texture = harness.app.texture.as_ref().unwrap();
    assert_eq!(texture.size(), [32, 24]);
    assert!(harness.app.display_fps() > 0.0);
}

#[test]
fn auto_stretch_changes_the_levels_but_not_the_frame() {
    let mut harness = Harness::new();
    harness.connect();
    harness.app.send(WorkerCommand::StartStream);
    harness.run_until("a frame on screen", |app| app.texture.is_some());

    harness.app.auto_stretch = false;
    harness.run_until("a linear render", |app| app.last_levels.0 == 0);
    let linear = harness.app.last_levels;
    assert_eq!(linear, (0, 65535), "linear uses the full bit depth");

    harness.app.auto_stretch = true;
    harness.run_until("a stretched render", |app| app.last_levels != linear);
    let stretched = harness.app.last_levels;
    assert!(
        stretched.1 < 65535 && stretched.1 > stretched.0,
        "auto stretch should narrow the range, got {stretched:?}"
    );
}

#[test]
fn the_window_keeps_rendering_while_the_camera_is_unplugged() {
    let mut harness = Harness::new();
    harness.connect();
    harness.app.send(WorkerCommand::StartStream);
    harness.run_until("a frame on screen", |app| app.texture.is_some());

    harness.sim.unplug();
    harness.run_until("the loss to reach the UI", |app| {
        matches!(
            app.status.state,
            ConnectionState::Lost { .. } | ConnectionState::Reconnecting { .. }
        )
    });

    // The whole point: frames still draw, and quickly, with no camera there.
    let mut slowest = Duration::ZERO;
    for _ in 0..30 {
        slowest = slowest.max(harness.frame());
    }
    assert!(
        slowest < Duration::from_millis(100),
        "a UI frame took {slowest:?} with the camera gone"
    );

    // And the reconnect happens by itself, restoring the live view.
    harness.sim.replug();
    harness.run_until("the reconnection", |app| app.connected());
    harness.run_until("frames after the reconnection", |app| app.status.streaming);
}

#[test]
fn a_frozen_camera_is_flagged_without_freezing_the_window() {
    let mut harness = Harness::new();
    harness.connect();
    harness.app.send(WorkerCommand::StartStream);
    harness.run_until("a frame on screen", |app| app.texture.is_some());

    harness.sim.freeze_frames(true);
    harness.run_until("the stall to be reported", |app| app.status.stalled);
    let slowest = (0..20).map(|_| harness.frame()).max().unwrap();
    assert!(
        slowest < Duration::from_millis(100),
        "a UI frame took {slowest:?} while the camera was stalled"
    );

    harness.sim.freeze_frames(false);
    harness.run_until("recovery", |app| !app.status.stalled);
}

#[test]
fn a_slow_control_call_does_not_block_the_ui_thread() {
    let mut harness = Harness::new();
    harness.connect();
    // Every control write now takes 300 ms inside the camera thread.
    harness.sim.set_control_latency(Duration::from_millis(300));

    harness
        .app
        .set_control(firstlight_core::ControlId::Gain, 250, true);
    let slowest = (0..10).map(|_| harness.frame()).max().unwrap();
    assert!(
        slowest < Duration::from_millis(100),
        "a UI frame took {slowest:?} while a control write was in flight"
    );
    harness.run_until("the control to be applied", |app| {
        app.status.settings.gain == 250
    });
}

#[test]
fn recording_and_snapshots_write_files_from_the_ui() {
    let mut harness = Harness::new();
    let dir = std::env::temp_dir().join(format!("firstlight-view-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    harness.app.output_dir = dir.display().to_string();

    harness.connect();
    harness.app.send(WorkerCommand::StartStream);
    harness.run_until("a frame on screen", |app| app.texture.is_some());

    let snap_path = harness.app.output_path("fits");
    harness.app.send(WorkerCommand::Snap { path: snap_path });
    harness.run_until("the snapshot to be saved", |app| {
        app.last_saved
            .as_ref()
            .is_some_and(|p| p.extension().is_some_and(|e| e == "fits"))
    });

    let ser_path = harness.app.output_path("ser");
    harness.app.send(WorkerCommand::StartRecording {
        path: ser_path.clone(),
        limit: Some(firstlight_core::RecordLimit::frames(4)),
    });
    harness.run_until("the recording to finish", |app| {
        app.last_saved.as_ref().is_some_and(|p| p == &ser_path)
    });

    let bytes = std::fs::read(&ser_path).unwrap();
    assert_eq!(&bytes[0..14], b"LUCAM-RECORDER");
    // 178 byte header + 4 frames of 64x48x2 + 4 timestamps.
    assert_eq!(bytes.len(), 178 + 4 * 64 * 48 * 2 + 4 * 8);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn geometry_changes_from_the_ui_reach_the_camera() {
    let mut harness = Harness::new();
    harness.connect();
    harness.app.send(WorkerCommand::StartStream);
    harness.run_until("a frame on screen", |app| app.texture.is_some());

    harness
        .app
        .send(WorkerCommand::SetRoi(firstlight_core::Roi::new(
            0, 0, 32, 32,
        )));
    harness.run_until("the new ROI to take effect", |app| {
        app.last_meta.as_ref().is_some_and(|m| m.width == 32)
    });
    assert_eq!(harness.app.texture.as_ref().unwrap().size(), [16, 16]);

    harness
        .app
        .send(WorkerCommand::SetBinning(firstlight_core::Binning(2)));
    harness.run_until("binning to take effect", |app| {
        app.status.settings.binning == firstlight_core::Binning(2)
    });
    // Binned output is mono, so the display no longer halves the size.
    harness.run_until("a binned frame", |app| {
        app.last_meta
            .as_ref()
            .is_some_and(|m| m.format == PixelFormat::Mono)
    });
}

#[test]
fn a_build_without_a_vendor_sdk_says_so_instead_of_showing_an_empty_list() {
    // The failure this prevents: a user with a camera plugged in sees only
    // the simulator, with nothing on screen to suggest the build simply
    // cannot see their hardware.
    let backend = Arc::new(SimulatorBackend::single(
        64,
        48,
        PixelFormat::Bayer(BayerPattern::Rggb),
    ));
    let registry = Registry::new()
        .with(backend as Arc<dyn Backend>)
        .with(Arc::new(firstlight_touptek::TouptekBackend::new()) as Arc<dyn Backend>);
    let ctx = egui::Context::default();
    let app = FirstLightApp::new(&ctx, registry);
    let sim = SimulatorBackend::single(1, 1, PixelFormat::Mono)
        .handle(0)
        .unwrap();
    let mut harness = Harness { ctx, app, sim };

    harness.run_until("the backend note", |app| !app.backend_notes.is_empty());
    let note = harness.app.backend_notes.join(" ");
    assert!(note.contains("--features touptek"), "note was {note:?}");
}

#[test]
fn a_status_update_does_not_yank_a_slider_the_user_is_holding() {
    let mut harness = Harness::new();
    harness.connect();

    // Make the camera slow to apply and report the change, which is when the
    // stale value in the next status snapshot used to overwrite the slider.
    harness.sim.set_control_latency(Duration::from_millis(300));
    harness
        .app
        .set_control(firstlight_core::ControlId::Gain, 250, true);

    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        harness.frame();
        assert_eq!(
            harness.app.values.get(&firstlight_core::ControlId::Gain),
            Some(&250),
            "the slider value was overwritten by a status update mid-edit"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    // Once the camera has caught up, its value is adopted again.
    harness.run_until("the camera to report the new gain", |app| {
        app.status.settings.gain == 250
    });
}

#[test]
fn a_full_size_sensor_does_not_slow_the_window_down() {
    // The regression this exists for: frame conversion used to happen on the
    // UI thread, so a 1920x1080 camera made every repaint take over a
    // hundred milliseconds. The earlier tests all used a 64x48 simulator and
    // sailed straight past it.
    let backend = Arc::new(SimulatorBackend::single(
        1920,
        1080,
        PixelFormat::Bayer(BayerPattern::Rggb),
    ));
    let sim = backend.handle(0).unwrap();
    let registry = Registry::new().with(backend as Arc<dyn Backend>);
    let ctx = egui::Context::default();
    let app = FirstLightApp::new(&ctx, registry);
    let mut harness = Harness { ctx, app, sim };

    harness.connect();
    harness.app.send(WorkerCommand::SetControl {
        id: firstlight_core::ControlId::ExposureUs,
        value: 5_000,
    });
    harness.app.send(WorkerCommand::StartStream);
    harness.run_until("a frame on screen", |app| app.texture.is_some());

    let mut slowest = Duration::ZERO;
    for _ in 0..60 {
        slowest = slowest.max(harness.frame());
    }
    assert!(
        slowest < Duration::from_millis(50),
        "a UI frame took {slowest:?} with a 1920x1080 camera streaming"
    );

    // And the texture is sized to the window rather than to the sensor.
    let texture = harness.app.texture.as_ref().unwrap();
    assert!(
        texture.size()[0] <= 1400,
        "texture is {:?} for a window about 1280 wide",
        texture.size()
    );
}
