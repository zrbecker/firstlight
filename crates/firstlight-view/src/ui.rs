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

    for error in app.enumeration_errors.clone() {
        ui.label(
            RichText::new(error)
                .small()
                .color(Color32::from_rgb(220, 160, 60)),
        );
    }

    // Why the list may be empty for reasons that have nothing to do with what
    // is plugged in — a backend whose SDK was not compiled in, say. Without
    // this, "no cameras" and "this build cannot see your camera" look
    // identical, which costs somebody an evening.
    for note in app.backend_notes.clone() {
        ui.label(
            RichText::new(format!("\u{24d8} {note}"))
                .small()
                .color(Color32::from_rgb(150, 170, 210)),
        );
    }
}

fn capture_section(app: &mut FirstLightApp, ui: &mut egui::Ui) {
    ui.heading("Capture");
    ui.add_space(4.0);
    let connected = app.connected();
    let recording = app.status.recording.clone();

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

    ui.add_space(6.0);

    // The frames are named from this, so `light_0001.fits` gives
    // `light_0001.fits`, `light_0002.fits` and so on.
    ui.horizontal(|ui| {
        ui.label("Save to");
        ui.add_enabled(
            recording.is_none(),
            egui::TextEdit::singleline(&mut app.output_dir).desired_width(f32::INFINITY),
        )
        .on_hover_text("Directory the captured frames go into");
    });
    ui.horizontal(|ui| {
        ui.label("Name");
        ui.add_enabled(
            recording.is_none(),
            egui::TextEdit::singleline(&mut app.file_pattern).desired_width(f32::INFINITY),
        )
        .on_hover_text(
            "Name of the first file. The digits set where the numbering starts \
             and how wide it is, so light_0001.fits gives light_0002.fits next. \
             Existing files are never overwritten.",
        );
    });

    ui.horizontal(|ui| {
        ui.label("Frames");
        ui.add_enabled(
            recording.is_none(),
            egui::DragValue::new(&mut app.capture_frames)
                .range(0..=1_000_000)
                .speed(1.0)
                .custom_formatter(|v, _| {
                    if v < 1.0 {
                        "until stopped".to_string()
                    } else {
                        format!("{v:.0}")
                    }
                }),
        )
        .on_hover_text("How many frames to keep. Zero runs until you stop it.");

        ui.label("Delay");
        ui.add_enabled(
            recording.is_none(),
            egui::DragValue::new(&mut app.capture_delay_s)
                .range(0.0..=3600.0)
                .speed(0.1)
                .suffix(" s"),
        )
        .on_hover_text(
            "Idle time between exposures. The period between saved frames is \
             the exposure plus this, so a 1 s exposure with a 2 s delay keeps \
             one frame every 3 s. Frames in between still reach the live view.",
        );
    });

    ui.horizontal(|ui| {
        if recording.is_some() {
            if ui
                .button(RichText::new("Stop capture").color(Color32::from_rgb(255, 140, 140)))
                .clicked()
            {
                app.send(WorkerCommand::StopRecording);
            }
        } else if ui
            .add_enabled(connected, egui::Button::new("Start capture"))
            .on_hover_text(
                "Write frames as FITS files, one per frame, each with its own \
                 exposure, gain and white balance in the header.",
            )
            .clicked()
        {
            let request = app.capture_request();
            app.send(WorkerCommand::StartRecording(request));
        }
    });

    if let Some(recording) = recording {
        let limit = recording
            .limit
            .and_then(|limit| limit.frames)
            .map(|frames| format!(" of {frames}"))
            .unwrap_or_default();
        ui.label(
            RichText::new(format!(
                "capturing — {} frame{}{}, {}, {:.0}s",
                recording.frames,
                if recording.frames == 1 { "" } else { "s" },
                limit,
                format_bytes(recording.bytes),
                recording.elapsed.as_secs_f32()
            ))
            .color(Color32::from_rgb(255, 170, 120)),
        );
        if let Some(next_in) = recording.next_in {
            ui.label(
                RichText::new(format!("next frame in {:.1}s", next_in.as_secs_f32()))
                    .small()
                    .color(Color32::GRAY),
            );
        }
        if let Some(last) = &recording.last_file {
            ui.label(
                RichText::new(format!(
                    "wrote {}",
                    last.file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default()
                ))
                .small()
                .color(Color32::GRAY),
            );
        }
    } else if let Some(saved) = &app.last_saved {
        ui.label(
            RichText::new(format!("last saved: {}", saved.display()))
                .small()
                .color(Color32::GRAY),
        );
    }
}

fn controls_section(app: &mut FirstLightApp, ui: &mut egui::Ui) {
    let controls = app.controls.clone();
    let enabled = app.connected();

    ui.horizontal(|ui| {
        ui.heading("Controls");
        let any_writable = controls.iter().any(|c| !c.read_only);
        if ui
            .add_enabled(
                enabled && any_writable,
                egui::Button::new("Reset all").small(),
            )
            .on_hover_text(
                "Put every control back to the default the camera reports. \
                 Geometry is left alone.",
            )
            .clicked()
        {
            app.reset_all_controls();
        }

        // Shown even where it cannot be used, and disabled with a reason: a
        // button that vanishes is indistinguishable from a broken build.
        // Offered by any camera that exposes the gains, since the balance is
        // measured from the picture rather than asked of the vendor SDK.
        let supported = [ControlId::WbRed, ControlId::WbGreen, ControlId::WbBlue]
            .iter()
            .all(|id| controls.iter().any(|control| control.id == *id));
        let response = ui
            .add_enabled(enabled && supported, egui::Button::new("Auto WB").small())
            .on_hover_text(if supported {
                "Measure a white balance from what the camera is looking at and \
                 store it in the camera, so captures come out balanced too. \
                 Point it at something neutral first."
            } else {
                "This camera cannot measure its own white balance; set the WB \
                 sliders by hand."
            });
        if response.clicked() {
            app.send(WorkerCommand::AutoWhiteBalance);
        }
    });
    ui.add_space(4.0);

    if controls.is_empty() {
        ui.label(RichText::new("Connect a camera to see its controls.").color(Color32::GRAY));
        return;
    }

    for control in controls {
        let mut value = *app.values.get(&control.id).unwrap_or(&control.default);
        let is_exposure = control.id == ControlId::ExposureUs;
        let at_default = value == control.default;

        ui.horizontal(|ui| {
            // A camera keeps its own settings, and they are not always the
            // ones it reports as default, so "put this back" is worth one
            // click rather than a hunt for the right number.
            let can_reset = enabled && !control.read_only && !at_default;
            if ui
                .add_enabled(can_reset, egui::Button::new("\u{21ba}").small())
                .on_hover_text(format!("Reset to default ({})", control.default))
                .clicked()
            {
                app.reset_control(control.id);
            }

            ui.add_enabled_ui(enabled && !control.read_only, |ui| {
                // Deliberately not sized from `available_width()`: inside a
                // horizontal layout, and during egui's sizing pass, that is
                // far larger than the panel, and the sliders end up laid out
                // hundreds of pixels wider than the sidebar they live in.
                // egui's default width fits, and the row grows by the button.
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
                    // Coalesce while dragging, then send the final value at
                    // once so the camera ends up where the pointer stopped.
                    app.set_control(control.id, value, response.drag_stopped());
                } else if response.drag_stopped() {
                    app.set_control(control.id, value, true);
                }
            });
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
    ui.checkbox(&mut app.white_balance_preview, "White balance preview")
        .on_hover_text(
            "Stretch each colour channel separately so the picture looks \
             neutral whatever the light. This is a preview only — recordings \
             and stills keep exactly what the sensor measured, gains and all. \
             To change the data, set the camera's white balance instead.",
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
    // Show what the preview balance is actually doing, so the correction is
    // never invisible — that was the whole objection to having it at all.
    if app.white_balance_preview {
        let [r, g, b] = app.last_channel_levels;
        ui.label(
            RichText::new(format!(
                "preview gains R {:.2}  G {:.2}  B {:.2}",
                channel_gain(g, r),
                1.0,
                channel_gain(g, b)
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

/// Says what state the picture is in, above the picture rather than over it.
///
/// A still frame on screen can mean four different things — stopped on
/// purpose, the camera stalled, the camera vanished, or connecting — and
/// without this they all look identical. Deliberately not dimming or hiding
/// the frame: a stopped live view is usually stopped so somebody can study
/// the last frame, and obscuring it would defeat the point.
fn banner(app: &mut FirstLightApp, ui: &mut egui::Ui) {
    let stale_frame = !app.status.streaming && app.texture.is_some();
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
        // Not a fault, so it does not get an alarming colour: this is just
        // what you asked for, said out loud.
        _ if stale_frame => Some((
            Color32::from_rgb(70, 82, 104),
            "Live view stopped — showing the last frame".to_string(),
        )),
        _ => None,
    };
    let Some((colour, text)) = message else {
        return;
    };
    let mut clear = false;
    egui::Frame::NONE
        .fill(colour)
        .inner_margin(egui::Margin::symmetric(8, 6))
        .corner_radius(egui::CornerRadius::same(4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(text).color(Color32::WHITE).strong());
                // Offered whenever a frame is sitting there not being
                // updated, whatever the reason for that.
                if stale_frame
                    && ui
                        .small_button("Clear image")
                        .on_hover_text("Remove the frozen frame from the live view")
                        .clicked()
                {
                    clear = true;
                }
            });
        });
    if clear {
        app.clear_image();
    }
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
        // So nobody judges colour from a picture that has been corrected.
        if app.white_balance_preview {
            ui.label(
                RichText::new("WB preview")
                    .small()
                    .color(Color32::from_rgb(150, 170, 210)),
            )
            .on_hover_text("The preview is colour-balanced; recordings are not.");
        }
    });
    ui.add_space(4.0);

    // Remembered so the renderer sizes its output to the window rather than
    // producing pixels nothing can show. Ignored when egui is measuring
    // rather than drawing, where the available width is not a real one.
    let width = ui.available_size().x;
    if width.is_finite() && width > 0.0 {
        app.viewport_width = width.max(320.0);
    }

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

/// How much a channel is being scaled relative to green, from the levels the
/// preview balance chose for each.
fn channel_gain(green: (u16, u16), channel: (u16, u16)) -> f32 {
    let green_span = f32::from(green.1.saturating_sub(green.0)).max(1.0);
    let span = f32::from(channel.1.saturating_sub(channel.0)).max(1.0);
    green_span / span
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
