//! Widget layout. All of it is pure UI: the only side effects are commands
//! queued for the camera thread.

use egui::{Color32, RichText};
use firstlight_core::control::ControlId;
use firstlight_core::worker::{ConnectionState, WorkerCommand};

use crate::app::{FirstLightApp, LogKind, RoiChoice};

pub fn left_panel(app: &mut FirstLightApp, ui: &mut egui::Ui) {
    egui::Panel::left("controls")
        .resizable(true)
        .default_size(340.0)
        .min_size(300.0)
        .max_size(560.0)
        .show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                camera_section(app, ui);
                ui.separator();
                capture_section(app, ui);
                ui.separator();
                controls_section(app, ui);
                ui.separator();
                geometry_section(app, ui);
                ui.separator();
                display_section(app, ui);
                ui.separator();
                statistics_section(app, ui);
            });
        });
}

fn camera_section(app: &mut FirstLightApp, ui: &mut egui::Ui) {
    ui.heading("Camera");
    ui.add_space(4.0);

    let selected_label = app
        .selected
        .as_ref()
        .and_then(|id| app.cameras.iter().find(|c| &c.id == id))
        .map(|c| format!("{} [{}]", c.display_name, c.backend))
        .unwrap_or_else(|| {
            if app.cameras.is_empty() {
                "No cameras found".to_string()
            } else {
                "Select a camera".to_string()
            }
        });

    let mut chosen = app.selected.clone();
    egui::ComboBox::from_id_salt("camera-picker")
        .selected_text(selected_label)
        .width(ui.available_width() - 8.0)
        .show_ui(ui, |ui| {
            for camera in &app.cameras {
                ui.selectable_value(
                    &mut chosen,
                    Some(camera.id.clone()),
                    format!("{} [{}]", camera.display_name, camera.backend),
                );
            }
        });
    if chosen != app.selected {
        app.selected = chosen;
    }

    ui.horizontal(|ui| {
        if ui.button("Refresh").clicked() {
            app.send(WorkerCommand::RefreshCameras);
        }
        let connected = !matches!(app.status.state, ConnectionState::Disconnected);
        if connected {
            if ui.button("Disconnect").clicked() {
                app.send(WorkerCommand::Disconnect);
            }
        } else {
            let can_connect = app.selected.is_some();
            if ui
                .add_enabled(can_connect, egui::Button::new("Connect"))
                .clicked()
                && let Some(id) = app.selected.clone()
            {
                app.send(WorkerCommand::Connect(id));
            }
        }
    });

    ui.add_space(4.0);
    let (colour, text) = state_badge(&app.status.state);
    ui.label(RichText::new(text).color(colour).strong());
    if let ConnectionState::Reconnecting { attempt, reason } = &app.status.state {
        ui.label(
            RichText::new(format!("after: {reason} (attempt {attempt})"))
                .small()
                .color(Color32::GRAY),
        );
    }

    let mut auto = app.auto_reconnect;
    if ui
        .checkbox(&mut auto, "Reconnect automatically")
        .on_hover_text(
            "Keep looking for the same camera after an unplug, and restore \
             its settings when it comes back.",
        )
        .changed()
    {
        app.auto_reconnect = auto;
        app.send(WorkerCommand::SetAutoReconnect(auto));
    }

    if !app.enumeration_errors.is_empty() {
        for error in app.enumeration_errors.clone() {
            ui.label(
                RichText::new(error)
                    .small()
                    .color(Color32::from_rgb(220, 160, 60)),
            );
        }
    }
}

fn capture_section(app: &mut FirstLightApp, ui: &mut egui::Ui) {
    ui.heading("Capture");
    ui.add_space(4.0);
    let connected = app.connected();

    ui.horizontal(|ui| {
        if app.status.streaming {
            if ui
                .add_enabled(connected, egui::Button::new("Stop live view"))
                .clicked()
            {
                app.send(WorkerCommand::StopStream);
            }
        } else if ui
            .add_enabled(connected, egui::Button::new("Start live view"))
            .clicked()
        {
            app.send(WorkerCommand::StartStream);
        }

        if ui
            .add_enabled(connected, egui::Button::new("Snap FITS"))
            .on_hover_text("Save the next frame, full resolution and unstretched")
            .clicked()
        {
            let path = app.output_path("fits");
            app.send(WorkerCommand::Snap { path });
        }
    });

    ui.horizontal(|ui| {
        let recording = app.status.recording.is_some();
        if recording {
            if ui
                .button(RichText::new("Stop recording").color(Color32::from_rgb(255, 140, 140)))
                .clicked()
            {
                app.send(WorkerCommand::StopRecording);
            }
        } else if ui
            .add_enabled(connected, egui::Button::new("Record SER"))
            .on_hover_text("Write every frame to a SER file; the live view keeps its own copy")
            .clicked()
        {
            let path = app.output_path("ser");
            app.send(WorkerCommand::StartRecording { path, limit: None });
        }
    });

    ui.horizontal(|ui| {
        ui.label("Save to");
        ui.add(
            egui::TextEdit::singleline(&mut app.output_dir)
                .desired_width(ui.available_width() - 8.0),
        );
    });

    if let Some(recording) = app.status.recording.clone() {
        ui.label(
            RichText::new(format!(
                "recording {} — {} frames, {}, {:.0}s",
                recording
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default(),
                recording.frames,
                format_bytes(recording.bytes),
                recording.elapsed.as_secs_f32()
            ))
            .color(Color32::from_rgb(255, 170, 120)),
        );
    }
    if let Some(saved) = &app.last_saved {
        ui.label(
            RichText::new(format!("last saved: {}", saved.display()))
                .small()
                .color(Color32::GRAY),
        );
    }
}

fn controls_section(app: &mut FirstLightApp, ui: &mut egui::Ui) {
    ui.heading("Controls");
    ui.add_space(4.0);
    if app.controls.is_empty() {
        ui.label(RichText::new("Connect a camera to see its controls.").color(Color32::GRAY));
        return;
    }

    let controls = app.controls.clone();
    let enabled = app.connected();
    for control in controls {
        let mut value = *app.values.get(&control.id).unwrap_or(&control.default);
        let is_exposure = control.id == ControlId::ExposureUs;

        ui.add_enabled_ui(enabled && !control.read_only, |ui| {
            let mut slider = egui::Slider::new(&mut value, control.min..=control.max)
                .logarithmic(control.logarithmic)
                .clamping(egui::SliderClamping::Always)
                .text(control.label.clone());
            if is_exposure {
                slider = slider.custom_formatter(|v, _| format_exposure(v.max(0.0) as u64));
            } else if !control.unit.is_empty() {
                slider = slider.suffix(format!(" {}", control.unit));
            }
            let response = ui.add(slider);
            if response.changed() {
                // Coalesce while dragging, then send the final value at once
                // so the camera ends up exactly where the pointer stopped.
                app.set_control(control.id, value, response.drag_stopped());
            } else if response.drag_stopped() {
                app.set_control(control.id, value, true);
            }
        });
    }
}

fn geometry_section(app: &mut FirstLightApp, ui: &mut egui::Ui) {
    ui.heading("Geometry");
    ui.add_space(4.0);

    let Some(info) = app.status.camera.clone() else {
        ui.label(RichText::new("Connect a camera to change geometry.").color(Color32::GRAY));
        return;
    };
    let enabled = app.connected();
    let binned_width = info.max_width / app.binning_choice.factor().max(1);
    let binned_height = info.max_height / app.binning_choice.factor().max(1);

    ui.add_enabled_ui(enabled, |ui| {
        // ROI
        let mut roi_choice = app.roi_choice;
        egui::ComboBox::from_label("ROI")
            .selected_text(roi_choice.label())
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut roi_choice, RoiChoice::Full, RoiChoice::Full.label());
                for (w, h) in [
                    (1920, 1080),
                    (1280, 960),
                    (1024, 768),
                    (800, 600),
                    (640, 480),
                    (320, 240),
                ] {
                    if w <= binned_width && h <= binned_height {
                        let choice = RoiChoice::Centred(w, h);
                        ui.selectable_value(&mut roi_choice, choice, choice.label());
                    }
                }
            });
        if roi_choice != app.roi_choice {
            app.roi_choice = roi_choice;
            let roi = roi_choice.resolve(binned_width, binned_height);
            app.send(WorkerCommand::SetRoi(roi));
        }

        // Binning
        let mut binning = app.binning_choice;
        egui::ComboBox::from_label("Binning")
            .selected_text(binning.to_string())
            .show_ui(ui, |ui| {
                for option in &info.binnings {
                    ui.selectable_value(&mut binning, *option, option.to_string());
                }
            });
        if binning != app.binning_choice {
            app.binning_choice = binning;
            // Binning resets the frame to full size on every backend, so the
            // ROI selector has to follow or it would lie.
            app.roi_choice = RoiChoice::Full;
            app.send(WorkerCommand::SetBinning(binning));
        }

        // Bit depth
        let mut depth = app.bit_depth_choice;
        egui::ComboBox::from_label("Bit depth")
            .selected_text(depth.to_string())
            .show_ui(ui, |ui| {
                for option in &info.bit_depths {
                    ui.selectable_value(&mut depth, *option, option.to_string());
                }
            });
        if depth != app.bit_depth_choice {
            app.bit_depth_choice = depth;
            app.send(WorkerCommand::SetBitDepth(depth));
        }
    });

    ui.label(
        RichText::new(format!(
            "sensor {}x{}, current {} at {}",
            info.max_width, info.max_height, app.status.settings.roi, app.status.settings.binning
        ))
        .small()
        .color(Color32::GRAY),
    );
}

fn display_section(app: &mut FirstLightApp, ui: &mut egui::Ui) {
    ui.heading("Display");
    ui.add_space(4.0);
    ui.checkbox(&mut app.auto_stretch, "Auto stretch")
        .on_hover_text(
            "Percentile histogram stretch, applied to the on-screen copy only. \
             Recorded and saved frames are never stretched.",
        );
    ui.checkbox(&mut app.neutralise_colour, "Neutralise colour")
        .on_hover_text(
            "Stretch each colour channel against its own histogram, which \
             cancels a colour cast from the sensor, the lighting, or white \
             balance gains stored in the camera. Display only: recordings and \
             stills keep exactly what the sensor measured.",
        );
    ui.checkbox(&mut app.debayer, "Debayer for display")
        .on_hover_text("Off shows the raw mosaic, which makes a wrong Bayer phase obvious");
    ui.add(
        egui::Slider::new(&mut app.gamma, 0.2..=2.0)
            .text("Gamma")
            .clamping(egui::SliderClamping::Always),
    );
    if app.auto_stretch {
        ui.label(
            RichText::new(format!(
                "levels {} – {}",
                app.last_levels.0, app.last_levels.1
            ))
            .small()
            .color(Color32::GRAY),
        );
    }
}

fn statistics_section(app: &mut FirstLightApp, ui: &mut egui::Ui) {
    ui.heading("Statistics");
    ui.add_space(4.0);
    egui::Grid::new("stats").num_columns(2).show(ui, |ui| {
        ui.label("Camera fps");
        ui.label(format!("{:.1}", app.status.fps));
        ui.end_row();

        ui.label("Display fps");
        ui.label(format!("{:.1}", app.display_fps()));
        ui.end_row();

        ui.label("Frames");
        ui.label(app.status.frames_received.to_string());
        ui.end_row();

        ui.label("Dropped (camera)");
        ui.label(RichText::new(app.status.camera_dropped.to_string()).color(
            if app.status.camera_dropped > 0 {
                Color32::from_rgb(255, 170, 120)
            } else {
                ui.visuals().text_color()
            },
        ));
        ui.end_row();

        ui.label("Dropped (display)");
        ui.label(app.status.display_dropped.to_string())
            .on_hover_text("Frames the live view skipped. Recording is unaffected.");
        ui.end_row();

        if let Some(temperature) = app.status.temperature_c {
            ui.label("Sensor");
            ui.label(format!("{temperature:.1} °C"));
            ui.end_row();
        }

        if let Some(meta) = &app.last_meta {
            ui.label("Last frame");
            ui.label(format!(
                "{}x{} {} {}",
                meta.width, meta.height, meta.bit_depth, meta.format
            ));
            ui.end_row();
        }
    });
}

/// The event log lives across the bottom: device trouble should be visible
/// without hunting for it.
pub fn log_panel(app: &mut FirstLightApp, ui: &mut egui::Ui) {
    egui::Panel::bottom("log")
        .resizable(true)
        .default_size(150.0)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Log");
                if ui.small_button("Clear").clicked() {
                    app.log.clear();
                }
            });
            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for line in &app.log {
                        let colour = match line.kind {
                            LogKind::Info => ui.visuals().text_color(),
                            LogKind::Warning => Color32::from_rgb(230, 180, 80),
                            LogKind::Error => Color32::from_rgb(240, 120, 120),
                        };
                        ui.label(
                            RichText::new(format!("{}  {}", line.clock(), line.text))
                                .color(colour)
                                .monospace(),
                        );
                    }
                });
        });
}

pub fn central_panel(app: &mut FirstLightApp, ui: &mut egui::Ui) {
    egui::CentralPanel::default_margins().show(ui, |ui| {
        banner(app, ui);
        image_area(app, ui);
    });
}

/// A loud, permanent indication when the camera is not there. The live view
/// keeps showing the last frame, and without this it would look live.
fn banner(app: &FirstLightApp, ui: &mut egui::Ui) {
    let message = match &app.status.state {
        ConnectionState::Lost { reason } => Some((
            Color32::from_rgb(150, 40, 40),
            format!("Camera lost: {reason}"),
        )),
        ConnectionState::Reconnecting { attempt, .. } => Some((
            Color32::from_rgb(150, 100, 30),
            format!("Reconnecting… (attempt {attempt})"),
        )),
        ConnectionState::Connecting => {
            Some((Color32::from_rgb(60, 90, 140), "Connecting…".to_string()))
        }
        _ if app.status.stalled => Some((
            Color32::from_rgb(150, 100, 30),
            "No frames arriving — the camera has stopped delivering".to_string(),
        )),
        _ => None,
    };
    let Some((colour, text)) = message else {
        return;
    };
    egui::Frame::NONE
        .fill(colour)
        .inner_margin(egui::Margin::symmetric(8, 6))
        .corner_radius(egui::CornerRadius::same(4))
        .show(ui, |ui| {
            ui.label(RichText::new(text).color(Color32::WHITE).strong());
        });
    ui.add_space(4.0);
}

fn image_area(app: &mut FirstLightApp, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!(
                "{:.1} fps   {} frames   {} dropped (camera)   {} dropped (display)",
                app.status.fps,
                app.status.frames_received,
                app.status.camera_dropped,
                app.status.display_dropped
            ))
            .monospace(),
        );
        if app.status.recording.is_some() {
            ui.label(
                RichText::new("● REC")
                    .color(Color32::from_rgb(240, 90, 90))
                    .strong(),
            );
        }
    });
    ui.add_space(4.0);

    // Remembered so the renderer sizes its output to the window rather than
    // producing pixels nothing can show.
    app.viewport_width = ui.available_size().x.max(320.0);

    let Some(texture) = &app.texture else {
        ui.centered_and_justified(|ui| {
            ui.label(
                RichText::new(match app.status.state {
                    ConnectionState::Disconnected => "Connect a camera to start.",
                    _ if !app.status.streaming => "Start the live view to see frames.",
                    _ => "Waiting for the first frame…",
                })
                .color(Color32::GRAY),
            );
        });
        return;
    };

    // Fit to the panel, never magnifying beyond 4x so a small ROI does not
    // turn into a wall of soft pixels.
    let available = ui.available_size();
    let size = texture.size_vec2();
    let scale = (available.x / size.x).min(available.y / size.y).min(4.0);
    let target = size * scale.max(0.05);
    ui.centered_and_justified(|ui| {
        ui.add(egui::Image::new(egui::load::SizedTexture::new(
            texture.id(),
            target,
        )));
    });
}

fn state_badge(state: &ConnectionState) -> (Color32, String) {
    match state {
        ConnectionState::Disconnected => (Color32::GRAY, "Disconnected".into()),
        ConnectionState::Connecting => (Color32::from_rgb(120, 160, 220), "Connecting…".into()),
        ConnectionState::Connected => (Color32::from_rgb(120, 210, 130), "Connected".into()),
        ConnectionState::Lost { .. } => (Color32::from_rgb(240, 120, 120), "Device lost".into()),
        ConnectionState::Reconnecting { .. } => {
            (Color32::from_rgb(240, 190, 100), "Reconnecting…".into())
        }
    }
}

fn format_exposure(micros: u64) -> String {
    if micros >= 1_000_000 {
        format!("{:.3} s", micros as f64 / 1e6)
    } else if micros >= 1_000 {
        format!("{:.1} ms", micros as f64 / 1e3)
    } else {
        format!("{micros} µs")
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
