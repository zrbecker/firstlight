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
        let app = FirstLightApp::new(&ctx, registry);
        Ui {
            ctx,
            app,
            painted: Vec::new(),
        }
    }

    fn frame(&mut self) {
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
        output.textures_delta.clear();
        self.painted = painted_text(&output.shapes);
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
