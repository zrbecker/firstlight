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

    // And a capture run, which writes one FITS file per frame.
    let run = dir.join("run");
    harness.app.send(WorkerCommand::StartRecording(
        firstlight_core::worker::RecordRequest::new(run.join("light_0001.fits"))
            .limit(Some(firstlight_core::RecordLimit::frames(4))),
    ));
    harness.run_until("the capture run to finish", |app| {
        app.last_saved.as_ref().is_some_and(|p| p == &run)
    });

    let mut written: Vec<String> = std::fs::read_dir(&run)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    written.sort();
    assert_eq!(written.len(), 4, "wrote {written:?}");
    assert_eq!(written[0], "light_0001.fits");
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

#[test]
fn sliders_show_what_the_camera_holds_rather_than_the_defaults() {
    // The bug this guards: every control except exposure, gain and offset
    // displayed its default, so a camera holding a white balance left by
    // other software looked neutral in the UI while its frames were not.
    let mut harness = Harness::new();
    harness.connect();
    harness.run_until("the control table", |app| !app.controls.is_empty());

    let wb_red = firstlight_core::ControlId::WbRed;
    let default = harness
        .app
        .controls
        .iter()
        .find(|c| c.id == wb_red)
        .expect("white balance control")
        .default;

    // Change it *at the worker*, the way another application or a previous
    // session would have: the UI never touches its own copy.
    harness.app.send(WorkerCommand::SetControl {
        id: wb_red,
        value: 250,
    });
    harness.run_until("the slider to follow the camera", |app| {
        app.values.get(&wb_red) == Some(&250)
    });
    assert_ne!(default, 250, "the test would prove nothing otherwise");
}

#[test]
fn a_control_can_be_put_back_to_its_default() {
    let mut harness = Harness::new();
    harness.connect();
    harness.run_until("the control table", |app| !app.controls.is_empty());

    let wb_red = firstlight_core::ControlId::WbRed;
    harness.app.send(WorkerCommand::SetControl {
        id: wb_red,
        value: 250,
    });
    harness.run_until("the change", |app| app.values.get(&wb_red) == Some(&250));

    harness.app.reset_control(wb_red);
    harness.run_until("the camera to report the default again", |app| {
        app.status.control_values.get(&wb_red)
            == app
                .controls
                .iter()
                .find(|c| c.id == wb_red)
                .map(|c| &c.default)
    });
}

#[test]
fn resetting_everything_leaves_read_only_controls_alone() {
    let mut harness = Harness::new();
    harness.connect();
    harness.run_until("the control table", |app| !app.controls.is_empty());

    let temperature = firstlight_core::ControlId::Vendor(16);
    assert!(
        harness
            .app
            .controls
            .iter()
            .any(|c| c.id == temperature && c.read_only),
        "the simulator should expose a read-only control"
    );

    for (id, value) in [
        (firstlight_core::ControlId::WbRed, 250),
        (firstlight_core::ControlId::Gain, 300),
    ] {
        harness.app.send(WorkerCommand::SetControl { id, value });
        harness.run_until("the change", |app| app.values.get(&id) == Some(&value));
    }

    let from = harness.app.log.len();
    harness.app.reset_all_controls();
    harness.run_until("everything back at its default", |app| {
        app.controls
            .iter()
            .filter(|c| !c.read_only)
            .all(|c| app.status.control_values.get(&c.id) == Some(&c.default))
    });

    // Writing a read-only control would have been reported as a failure.
    let complaints: Vec<&str> = harness
        .app
        .log
        .iter()
        .skip(from)
        .map(|l| l.text.as_str())
        .collect();
    assert!(
        !complaints.iter().any(|t| t.contains("read-only")),
        "reset-all tried to write a read-only control: {complaints:?}"
    );
}

#[test]
fn the_preview_white_balance_keeps_responding_while_settings_change() {
    // Reported: after toggling the preview balance while adjusting exposure
    // and gain, the toggle stopped having any effect until the app was
    // restarted — which points at the renderer thread rather than the toggle.
    let mut harness = Harness::new();
    harness.connect();
    harness.run_until("the control table", |app| !app.controls.is_empty());

    // Give the camera a strong cast, so "is the correction applied" is
    // visible in the levels regardless of what the scene looks like.
    harness.app.send(WorkerCommand::SetControl {
        id: firstlight_core::ControlId::WbRed,
        value: 300,
    });
    harness.app.send(WorkerCommand::SetControl {
        id: firstlight_core::ControlId::WbBlue,
        value: 40,
    });
    harness.app.send(WorkerCommand::StartStream);
    harness.run_until("a frame on screen", |app| app.texture.is_some());

    let mut exposure = 4_000i64;
    for round in 0..12 {
        // Toggle, and change exposure and gain the way a user fiddling with
        // the sliders would.
        harness.app.white_balance_preview = round % 2 == 0;
        exposure = if exposure > 20_000 {
            4_000
        } else {
            exposure + 3_000
        };
        harness.app.send(WorkerCommand::SetControl {
            id: firstlight_core::ControlId::ExposureUs,
            value: exposure,
        });
        harness.app.send(WorkerCommand::SetControl {
            id: firstlight_core::ControlId::Gain,
            value: 100 + (round as i64 % 5) * 40,
        });

        // Let the renderer catch up with the new setting.
        let wanted = harness.app.white_balance_preview;
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut matched = false;
        while Instant::now() < deadline {
            harness.frame();
            let gains = harness.app.last_channel_gains;
            let correcting = gains != [1.0; 3];
            if correcting == wanted {
                matched = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            matched,
            "round {round}: preview balance {} but the gains are {:?}",
            if wanted { "on" } else { "off" },
            harness.app.last_channel_gains
        );
    }
}

#[test]
fn a_stopped_renderer_is_noticed_reported_and_replaced() {
    // The failure this guards: when the render thread stops, the last image
    // stays on screen looking live, the display controls quietly stop having
    // any effect, and only restarting the application clears it.
    let mut harness = Harness::new();
    harness.connect();
    harness.app.send(WorkerCommand::StartStream);
    harness.run_until("a frame on screen", |app| app.texture.is_some());

    // Stop the renderer the way a panic would: the handle's Drop joins it.
    let ring = harness.app.worker.frame_ring();
    let options = harness.app.display;
    harness.app.renderer = firstlight_view::render::Renderer::spawn(ring, options);
    // Replacing it is the recovery path; prove the app also *notices* one
    // that has gone, by handing it a renderer that is already stopped.
    harness.app.renderer.stop_for_test();
    assert!(!harness.app.renderer.is_alive());

    let before = harness.app.log.len();
    harness.run_until("the renderer to be restarted", |app| {
        app.renderer.is_alive()
    });

    let complaint: Vec<&str> = harness
        .app
        .log
        .iter()
        .skip(before)
        .map(|line| line.text.as_str())
        .collect();
    assert!(
        complaint.iter().any(|t| t.contains("renderer stopped")),
        "the restart should be reported, log said {complaint:?}"
    );

    // And it works again afterwards.
    harness.run_until("frames again", |app| app.texture.is_some());
}

#[test]
fn stacking_averages_the_live_view_without_touching_recordings() {
    let mut harness = Harness::new();
    let dir = std::env::temp_dir().join(format!("firstlight-stack-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    harness.connect();
    harness.app.send(WorkerCommand::SetControl {
        id: firstlight_core::ControlId::ExposureUs,
        value: 3_000,
    });
    harness.app.send(WorkerCommand::StartStream);
    harness.run_until("a frame on screen", |app| app.texture.is_some());

    // Off by default: one frame in, one frame shown.
    assert_eq!(harness.app.stack_depth, 1);
    harness.run_until("an unstacked frame", |app| app.stacked_frames == 1);

    harness.app.stack_depth = 6;
    harness.run_until("the stack to fill", |app| app.stacked_frames == 6);
    assert!(
        harness.app.stacked_span > Duration::ZERO,
        "a filled stack spans some wall-clock time"
    );

    // A capture run taken while stacking must still write single frames.
    let run = dir.join("run");
    harness.app.send(WorkerCommand::StartRecording(
        firstlight_core::worker::RecordRequest::new(run.join("light_0001.fits"))
            .limit(Some(firstlight_core::RecordLimit::frames(3))),
    ));
    harness.run_until("the run to finish", |app| {
        app.last_saved.as_ref().is_some_and(|p| p == &run)
    });

    for entry in std::fs::read_dir(&run).unwrap() {
        let path = entry.unwrap().path();
        let bytes = std::fs::read(&path).unwrap();
        // 2880-byte header, then 64x48 samples at 16 bits. A stacked frame
        // would be the same size, so check the exposure card instead: the
        // stack reports its total integration, a single frame does not.
        let header = String::from_utf8_lossy(&bytes[..2880]).to_string();
        let exposure: f64 = header
            .as_bytes()
            .chunks(80)
            .find(|card| card.starts_with(b"EXPTIME ="))
            .map(|card| String::from_utf8_lossy(&card[10..]).to_string())
            .and_then(|value| value.split('/').next().map(|v| v.trim().to_string()))
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| panic!("no EXPTIME in {path:?}"));
        assert!(
            exposure < 0.01,
            "a recorded frame should carry its own ~3ms exposure, not the \
             stack's total; {path:?} says {exposure}s"
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_geometry_change_restarts_the_stack_rather_than_mixing_shapes() {
    let mut harness = Harness::new();
    harness.connect();
    harness.app.send(WorkerCommand::SetControl {
        id: firstlight_core::ControlId::ExposureUs,
        value: 3_000,
    });
    harness.app.stack_depth = 8;
    harness.app.send(WorkerCommand::StartStream);
    harness.run_until("the stack to fill", |app| app.stacked_frames >= 4);

    harness
        .app
        .send(WorkerCommand::SetRoi(firstlight_core::Roi::new(
            0, 0, 32, 32,
        )));
    harness.run_until("the smaller frames to arrive", |app| {
        app.last_meta.as_ref().is_some_and(|m| m.width == 32)
    });
    // Frames of different sizes cannot be averaged, so it starts over and
    // fills again rather than showing a mixture or falling over.
    harness.run_until("the stack to refill at the new size", |app| {
        app.stacked_frames >= 4 && app.last_meta.as_ref().is_some_and(|m| m.width == 32)
    });
}
