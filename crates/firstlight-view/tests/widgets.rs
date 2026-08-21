//! Checks that widgets actually reach the screen.
//!
//! The behavioural tests drive the app's methods directly, which says nothing
//! about whether a button was ever painted — a missing font glyph or a layout
//! that squeezes a control out of the panel would sail past them. These walk
//! the shapes egui produced and look for the text that should be visible.

use std::sync::Arc;

use firstlight_core::camera::Backend;
use firstlight_core::frame::{BayerPattern, PixelFormat};
use firstlight_core::registry::Registry;
use firstlight_core::simulator::SimulatorBackend;
use firstlight_core::worker::WorkerCommand;
use firstlight_view::FirstLightApp;

/// Every string egui painted this frame.
fn painted_text(shapes: &[egui::epaint::ClippedShape]) -> Vec<String> {
    fn walk(shape: &egui::Shape, out: &mut Vec<String>) {
        match shape {
            egui::Shape::Text(text) => out.push(text.galley.text().to_string()),
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    walk(shape, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for clipped in shapes {
        walk(&clipped.shape, &mut out);
    }
    out
}

struct Ui {
    ctx: egui::Context,
    app: FirstLightApp,
    painted: Vec<String>,
    last_shapes: Vec<egui::epaint::ClippedShape>,
    size: (f32, f32),
}

impl Ui {
    fn new() -> Ui {
        Ui::with_registry(Registry::new().with(Arc::new(SimulatorBackend::single(
            64,
            48,
            PixelFormat::Bayer(BayerPattern::Rggb),
        )) as Arc<dyn Backend>))
    }

    fn with_registry(registry: Registry) -> Ui {
        let ctx = egui::Context::default();
        let app = FirstLightApp::with_dark_path(&ctx, registry, Some(scratch_dark_path()));
        Ui {
            ctx,
            app,
            painted: Vec::new(),
            last_shapes: Vec::new(),
            size: (1280.0, 820.0),
        }
    }

    fn frame(&mut self) {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(self.size.0, self.size.1),
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
        output.textures_delta.clear();
        self.painted = painted_text(&output.shapes);
        self.last_shapes = output.shapes;
    }

    fn run_until(&mut self, what: &str, mut done: impl FnMut(&Ui) -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            self.frame();
            if done(self) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("timed out waiting for {what}. Painted: {:#?}", self.painted);
    }

    fn shows(&self, text: &str) -> bool {
        self.painted.iter().any(|painted| painted.contains(text))
    }

    fn connect(&mut self) {
        self.run_until("a camera", |ui| !ui.app.cameras.is_empty());
        let id = self.app.selected.clone().expect("a camera");
        self.app.send(WorkerCommand::Connect(id));
        self.run_until("the connection", |ui| ui.app.connected());
    }
}

#[test]
fn the_reset_buttons_are_actually_on_screen() {
    let mut ui = Ui::new();
    ui.connect();
    ui.run_until("the control table", |ui| !ui.app.controls.is_empty());
    ui.frame();

    assert!(ui.shows("Reset all"), "painted: {:#?}", ui.painted);
    assert!(
        ui.shows("Exposure"),
        "the controls themselves should be there: {:#?}",
        ui.painted
    );

    // The per-control button. A label whose glyph the bundled fonts cannot
    // draw would leave an empty square, so check the font can draw it.
    let glyph = "\u{21ba}";
    let renderable = ui
        .ctx
        .fonts_mut(|fonts| fonts.has_glyphs(&egui::FontId::proportional(14.0), glyph));
    assert!(
        renderable,
        "egui's bundled fonts cannot draw {glyph:?}, so the reset button would \
         be an empty box"
    );
    assert!(ui.shows(glyph), "painted: {:#?}", ui.painted);
}

#[test]
fn the_auto_white_balance_button_is_offered() {
    let mut ui = Ui::new();
    ui.connect();
    ui.run_until("the control table", |ui| !ui.app.controls.is_empty());
    ui.frame();
    assert!(ui.shows("Auto WB"), "painted: {:#?}", ui.painted);
}

#[test]
fn a_backend_that_cannot_see_anything_says_so_on_screen() {
    // The CLI printed this note; the GUI was supposed to and did not, so an
    // empty camera list looked the same as a build with no SDK compiled in.
    let registry = Registry::new()
        .with(Arc::new(SimulatorBackend::single(
            64,
            48,
            PixelFormat::Bayer(BayerPattern::Rggb),
        )) as Arc<dyn Backend>)
        .with(Arc::new(firstlight_touptek::TouptekBackend::new()) as Arc<dyn Backend>);
    let mut ui = Ui::with_registry(registry);

    ui.run_until("the backend note to be painted", |ui| {
        ui.shows("--features touptek")
    });
}

/// How far right anything that starts inside the left panel reaches.
fn left_panel_extent(shapes: &[egui::epaint::ClippedShape]) -> f32 {
    let mut extent: f32 = 0.0;
    for clipped in shapes {
        let rect = clipped.shape.visual_bounding_rect();
        if rect.is_finite() && rect.min.x < 300.0 {
            extent = extent.max(rect.max.x);
        }
    }
    extent
}

/// True if the live-view texture was drawn.
fn shows_live_image(shapes: &[egui::epaint::ClippedShape]) -> bool {
    fn is_ours(id: egui::TextureId) -> bool {
        // The font atlas is Managed(0); anything else here is our frame.
        !matches!(id, egui::TextureId::Managed(0))
    }
    fn walk(shape: &egui::Shape) -> bool {
        match shape {
            // egui paints an unrotated image as a rectangle with a texture
            // brush, and only a rotated one as a mesh.
            egui::Shape::Rect(rect) => rect
                .brush
                .as_ref()
                .is_some_and(|brush| is_ours(brush.fill_texture_id)),
            egui::Shape::Mesh(mesh) => is_ours(mesh.texture_id),
            egui::Shape::Vec(shapes) => shapes.iter().any(walk),
            _ => false,
        }
    }
    shapes.iter().any(|clipped| walk(&clipped.shape))
}

#[test]
fn the_controls_stay_inside_the_sidebar() {
    // The regression this exists for: sizing the sliders from
    // `available_width()` inside a horizontal layout laid them out to x=698
    // in a panel 340 wide. They spilled across the live view, and the central
    // panel lost 220px to them.
    for (width, height) in [(1280.0f32, 820.0f32), (1376.0, 576.0), (1024.0, 640.0)] {
        let mut ui = Ui::new();
        ui.size = (width, height);
        ui.connect();
        ui.run_until("the control table", |ui| !ui.app.controls.is_empty());
        ui.frame();

        let extent = left_panel_extent(&ui.last_shapes);
        assert!(
            extent < 400.0,
            "left panel content reaches x={extent:.0} at {width}x{height}; the \
             panel is 340 wide"
        );
        assert!(
            ui.app.viewport_width > width * 0.4,
            "the live view got only {:.0}px of a {width}px window",
            ui.app.viewport_width
        );
    }
}

#[test]
fn the_live_view_is_actually_drawn() {
    let mut ui = Ui::new();
    ui.connect();
    ui.app.send(WorkerCommand::StartStream);
    ui.run_until("a frame on screen", |ui| ui.app.texture.is_some());
    ui.frame();
    assert!(
        shows_live_image(&ui.last_shapes),
        "the live view texture was never painted; painted text was {:#?}",
        ui.painted
    );
}

#[test]
fn the_preview_white_balance_is_labelled_and_visible() {
    let mut ui = Ui::new();
    ui.connect();
    ui.app.send(WorkerCommand::StartStream);
    ui.run_until("a frame on screen", |ui| ui.app.texture.is_some());
    ui.frame();

    // Named for what it does, not "neutralise colour".
    assert!(
        ui.shows("White balance preview"),
        "painted: {:#?}",
        ui.painted
    );
    // What it is applying is on screen, so the correction is not silent.
    assert!(ui.shows("preview gains"), "painted: {:#?}", ui.painted);
    // And the live view says it is being corrected.
    assert!(ui.shows("WB preview"), "painted: {:#?}", ui.painted);
}

#[test]
fn a_stopped_live_view_says_so_without_covering_the_frame() {
    // Four things can leave a still picture on screen — stopped, stalled,
    // lost, connecting — and they used to look identical. The frame itself
    // is left alone deliberately: it is usually why the view was stopped.
    let mut ui = Ui::new();
    ui.connect();
    ui.app.send(WorkerCommand::StartStream);
    ui.run_until("a frame on screen", |ui| ui.app.texture.is_some());

    ui.app.send(WorkerCommand::StopStream);
    ui.run_until("the stopped banner", |ui| ui.shows("Live view stopped"));

    // The picture is still there and still drawn.
    assert!(
        ui.app.texture.is_some(),
        "the frame should not be discarded"
    );
    assert!(
        shows_live_image(&ui.last_shapes),
        "the frame should still be painted"
    );
    assert!(ui.shows("Clear image"), "painted: {:#?}", ui.painted);
}

#[test]
fn clearing_the_image_sticks() {
    let mut ui = Ui::new();
    ui.connect();
    ui.app.send(WorkerCommand::StartStream);
    ui.run_until("a frame on screen", |ui| ui.app.texture.is_some());
    ui.app.send(WorkerCommand::StopStream);
    ui.run_until("the stream to stop", |ui| !ui.app.status.streaming);

    ui.app.clear_image();
    ui.frame();
    assert!(ui.app.texture.is_none(), "the image should be gone");

    // The renderer holds the last frame and redraws it whenever a display
    // setting changes, so a clear that does not reach it comes undone a
    // moment later.
    ui.app.white_balance_preview = !ui.app.white_balance_preview;
    for _ in 0..20 {
        ui.frame();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        ui.app.texture.is_none(),
        "the cleared image came back when a display setting changed"
    );
    assert!(
        !shows_live_image(&ui.last_shapes),
        "a cleared live view should not still be painting a frame"
    );
}

#[test]
fn the_capture_panel_offers_the_naming_count_and_delay() {
    let mut ui = Ui::new();
    ui.connect();
    ui.frame();
    for label in ["Save to", "Name", "Frames", "Delay", "Start capture"] {
        assert!(ui.shows(label), "{label:?} missing from {:#?}", ui.painted);
    }
    // Zero frames reads as unlimited rather than as a literal zero.
    assert!(ui.shows("until stopped"), "painted: {:#?}", ui.painted);
}

#[test]
fn a_running_capture_offers_a_stop_button() {
    let mut ui = Ui::new();
    let directory = std::env::temp_dir().join(format!("firstlight-ui-{}", std::process::id()));
    ui.connect();
    ui.app.output_dir = directory.display().to_string();
    ui.app.file_pattern = "light_0001.fits".into();
    ui.app.capture_frames = 0;

    let request = ui.app.capture_request();
    ui.app.send(WorkerCommand::StartRecording(request));
    ui.run_until("the capture to start", |ui| ui.shows("Stop capture"));
    assert!(!ui.shows("Start capture"), "both buttons are showing");

    ui.app.send(WorkerCommand::StopRecording);
    ui.run_until("the capture to stop", |ui| ui.shows("Start capture"));
    std::fs::remove_dir_all(&directory).ok();
}

#[test]
fn the_stack_control_reaches_the_screen() {
    let mut ui = Ui::new();
    ui.connect();
    ui.frame();
    assert!(ui.shows("Stack"), "painted: {:#?}", ui.painted);
    // Reads as "off" rather than a bare 1, and says nothing more until on.
    assert!(ui.shows("off"), "painted: {:#?}", ui.painted);
    assert!(!ui.shows("averaging"));

    ui.app.stack_depth = 4;
    ui.app.send(WorkerCommand::StartStream);
    ui.run_until("the stack to fill", |ui| ui.app.stacked_frames >= 2);
    ui.frame();
    // What it is actually doing is on screen, not just what was asked for.
    assert!(ui.shows("averaging"), "painted: {:#?}", ui.painted);
    // And the live view is marked, so a smooth picture is never mistaken
    // for a single clean frame.
    assert!(ui.shows("stack x"), "painted: {:#?}", ui.painted);
}

#[test]
fn taking_darks_asks_you_to_cover_the_camera_first() {
    // The confirmation step is the whole point: only the person in the room
    // can cover the camera, and a dark taken uncovered is worse than none.
    let mut ui = Ui::new();
    ui.connect();
    ui.frame();
    assert!(ui.shows("Take darks"), "painted: {:#?}", ui.painted);
    assert!(!ui.shows("Cover the camera"));

    ui.app.confirming_darks = true;
    ui.frame();
    assert!(ui.shows("Cover the camera"), "painted: {:#?}", ui.painted);

    ui.app.dark_frames = 3;
    ui.app.take_darks();
    ui.app.send(WorkerCommand::StartStream);
    ui.run_until("the dark to arrive", |ui| ui.app.dark.is_some());
    ui.frame();
    // Once taken, the control says what it has and the view is marked.
    assert!(ui.shows("Subtract dark"), "painted: {:#?}", ui.painted);
    assert!(ui.shows("frames at"), "painted: {:#?}", ui.painted);
    assert!(ui.shows("dark"), "painted: {:#?}", ui.painted);
}

/// A saved-dark path of this test's own, inside the temp directory.
///
/// The app loads a master dark at start and deletes it on Clear. Tests must
/// not reach the one belonging to whoever is running them, and must not
/// reach each other's either — they run in parallel in one process.
fn scratch_dark_path() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "firstlight-test-dark-{}-{}.fits",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::remove_file(&path).ok();
    path
}
