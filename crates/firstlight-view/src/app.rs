//! Application state and the frame loop.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use firstlight_core::camera::{CameraId, CameraInfo};
use firstlight_core::control::{Binning, BitDepth, ControlId, ControlInfo, Roi};
use firstlight_core::display::{self, DisplayOptions, Stretch};
use firstlight_core::frame::FrameMeta;
use firstlight_core::registry::Registry;
use firstlight_core::worker::{
    ConnectionState, WorkerCommand, WorkerHandle, WorkerStatus, WorkerUpdate, timestamped_name,
};

/// Control writes go over USB. Dragging a slider must not turn into hundreds
/// of them, so values are coalesced and sent at most this often; the final
/// value is always sent when the drag ends.
const CONTROL_THROTTLE: Duration = Duration::from_millis(120);

/// After sending a control change, ignore what the camera reports for that
/// control for this long. A status snapshot arrives every 200 ms carrying the
/// value read back from the camera, and a value we set 20 ms ago has usually
/// not been applied and reported yet — adopting it would snap the slider back
/// under the user's pointer, which is exactly what "the slider does not drag
/// smoothly" looks like.
const CONTROL_SETTLE: Duration = Duration::from_millis(600);

/// Longest edge of the texture we upload. Larger frames are subsampled for
/// display only — the recorded and saved data is always full resolution.
const MAX_DISPLAY_EDGE: u32 = 1600;

const LOG_LIMIT: usize = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogKind {
    Info,
    Warning,
    Error,
}

pub struct LogLine {
    pub at: std::time::SystemTime,
    pub kind: LogKind,
    pub text: String,
}

impl LogLine {
    /// `HH:MM:SS` UTC, so a log line can be matched against a file name or a
    /// FITS `DATE-OBS` later.
    pub fn clock(&self) -> String {
        let utc = firstlight_core::time_util::utc_from_system_time(self.at);
        format!("{:02}:{:02}:{:02}", utc.hour, utc.minute, utc.second)
    }
}

/// Preset window sizes offered in the ROI dropdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoiChoice {
    Full,
    Centred(u32, u32),
}

impl RoiChoice {
    pub fn label(&self) -> String {
        match self {
            RoiChoice::Full => "Full frame".to_string(),
            RoiChoice::Centred(w, h) => format!("{w} x {h} (centred)"),
        }
    }

    /// Resolve to an actual ROI on a sensor of the given binned size.
    pub fn resolve(&self, width: u32, height: u32) -> Roi {
        match self {
            RoiChoice::Full => Roi::full(width, height),
            RoiChoice::Centred(w, h) => {
                let w = (*w).min(width);
                let h = (*h).min(height);
                Roi::new((width - w) / 2, (height - h) / 2, w, h).align_even()
            }
        }
    }
}

pub struct FirstLightApp {
    pub worker: WorkerHandle,

    pub cameras: Vec<CameraInfo>,
    pub enumeration_errors: Vec<String>,
    /// Backends explaining why they cannot see anything in this build.
    pub backend_notes: Vec<String>,
    pub selected: Option<CameraId>,
    pub controls: Vec<ControlInfo>,
    /// Slider positions. Kept separate from [`WorkerStatus::settings`] so a
    /// status arriving mid-drag does not yank the slider out from under the
    /// pointer.
    pub values: BTreeMap<ControlId, i64>,
    pending: BTreeMap<ControlId, (i64, Instant)>,
    /// Controls the user has touched recently; see [`CONTROL_SETTLE`].
    editing: BTreeMap<ControlId, Instant>,

    pub status: WorkerStatus,
    pub log: VecDeque<LogLine>,

    pub texture: Option<egui::TextureHandle>,
    pub last_meta: Option<FrameMeta>,
    /// Levels the last auto-stretch settled on, shown in the UI.
    pub last_levels: (u16, u16),
    display_times: VecDeque<Instant>,

    pub auto_stretch: bool,
    pub gamma: f32,
    pub debayer: bool,
    pub display: DisplayOptions,

    pub roi_choice: RoiChoice,
    pub binning_choice: Binning,
    pub bit_depth_choice: BitDepth,

    pub output_dir: String,
    pub last_saved: Option<PathBuf>,
    pub auto_reconnect: bool,
}

impl FirstLightApp {
    pub fn new(ctx: &egui::Context, registry: Registry) -> FirstLightApp {
        ctx.set_visuals(egui::Visuals::dark());
        let worker = WorkerHandle::spawn(registry);
        let output_dir = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".into());

        FirstLightApp {
            worker,
            cameras: Vec::new(),
            enumeration_errors: Vec::new(),
            backend_notes: Vec::new(),
            selected: None,
            controls: Vec::new(),
            values: BTreeMap::new(),
            pending: BTreeMap::new(),
            editing: BTreeMap::new(),
            status: WorkerStatus::default(),
            log: VecDeque::new(),
            texture: None,
            last_meta: None,
            last_levels: (0, 0),
            display_times: VecDeque::new(),
            auto_stretch: true,
            gamma: 1.0,
            debayer: true,
            display: DisplayOptions::default(),
            roi_choice: RoiChoice::Full,
            binning_choice: Binning::ONE,
            bit_depth_choice: BitDepth::SIXTEEN,
            output_dir,
            last_saved: None,
            auto_reconnect: true,
        }
    }

    pub fn send(&mut self, command: WorkerCommand) {
        if let Err(e) = self.worker.send(command) {
            self.push_log(LogKind::Error, format!("camera thread is gone: {e}"));
        }
    }

    pub fn push_log(&mut self, kind: LogKind, text: impl Into<String>) {
        self.log.push_back(LogLine {
            at: std::time::SystemTime::now(),
            kind,
            text: text.into(),
        });
        while self.log.len() > LOG_LIMIT {
            self.log.pop_front();
        }
    }

    /// True while the user owns this control's value and the camera's own
    /// reading must not overwrite it.
    fn is_editing(&self, id: ControlId) -> bool {
        self.pending.contains_key(&id)
            || self
                .editing
                .get(&id)
                .is_some_and(|since| since.elapsed() < CONTROL_SETTLE)
    }

    /// Queue a control change; the actual send is throttled.
    pub fn set_control(&mut self, id: ControlId, value: i64, immediate: bool) {
        self.values.insert(id, value);
        self.editing.insert(id, Instant::now());
        if immediate {
            self.pending.remove(&id);
            self.send(WorkerCommand::SetControl { id, value });
        } else {
            self.pending.insert(id, (value, Instant::now()));
        }
    }

    fn flush_pending_controls(&mut self) {
        let due: Vec<(ControlId, i64)> = self
            .pending
            .iter()
            .filter(|(_, (_, queued))| queued.elapsed() >= CONTROL_THROTTLE)
            .map(|(id, (value, _))| (*id, *value))
            .collect();
        for (id, value) in due {
            self.pending.remove(&id);
            self.send(WorkerCommand::SetControl { id, value });
        }
    }

    pub fn output_path(&self, extension: &str) -> PathBuf {
        PathBuf::from(&self.output_dir).join(timestamped_name("firstlight", extension))
    }

    pub fn display_fps(&self) -> f32 {
        self.display_times.len() as f32
    }

    pub fn connected(&self) -> bool {
        self.status.state.is_connected()
    }

    /// One pass of camera-thread bookkeeping: drain updates, send throttled
    /// control changes, turn the newest frame into a texture.
    ///
    /// Separate from the drawing pass so tests can drive it directly, and so
    /// it still runs when the window is hidden.
    pub fn tick(&mut self, ctx: &egui::Context) {
        self.drain_updates();
        self.flush_pending_controls();
        self.update_texture(ctx);
    }

    fn drain_updates(&mut self) {
        // Bounded so a burst can never stall a frame; anything left over is
        // picked up on the next repaint, which is 16 ms away.
        for _ in 0..256 {
            let Ok(update) = self.worker.updates().try_recv() else {
                break;
            };
            match update {
                WorkerUpdate::Cameras {
                    cameras,
                    errors,
                    notes,
                } => {
                    for error in &errors {
                        self.push_log(LogKind::Warning, error.clone());
                    }
                    self.enumeration_errors = errors;
                    self.backend_notes = notes;
                    // Keep the selection if that camera is still attached.
                    if let Some(selected) = &self.selected
                        && !cameras.iter().any(|c| &c.id == selected)
                        && !self.connected()
                    {
                        self.selected = None;
                    }
                    if self.selected.is_none() {
                        self.selected = cameras.first().map(|c| c.id.clone());
                    }
                    self.cameras = cameras;
                }
                WorkerUpdate::Controls(controls) => {
                    for control in &controls {
                        self.values.entry(control.id).or_insert(control.default);
                    }
                    self.controls = controls;
                }
                WorkerUpdate::Status(status) => {
                    let status = *status;
                    // Adopt the camera's values, but never for a control the
                    // user is currently working: their pointer wins.
                    if !self.is_editing(ControlId::ExposureUs) {
                        self.values
                            .insert(ControlId::ExposureUs, status.settings.exposure_us as i64);
                    }
                    if !self.is_editing(ControlId::Gain) {
                        self.values.insert(ControlId::Gain, status.settings.gain);
                    }
                    if !self.is_editing(ControlId::Offset) {
                        self.values
                            .insert(ControlId::Offset, status.settings.offset);
                    }
                    if status.state.is_connected() && !self.status.state.is_connected() {
                        self.binning_choice = status.settings.binning;
                        self.bit_depth_choice = status.settings.bit_depth;
                    }
                    self.status = status;
                }
                WorkerUpdate::Event(event) => {
                    let kind = if event.is_fatal() {
                        LogKind::Error
                    } else {
                        LogKind::Info
                    };
                    self.push_log(kind, event.to_string());
                }
                WorkerUpdate::Failed {
                    context,
                    message,
                    fatal,
                } => {
                    let kind = if fatal {
                        LogKind::Error
                    } else {
                        LogKind::Warning
                    };
                    self.push_log(kind, format!("{context}: {message}"));
                }
                WorkerUpdate::Saved { path, frames } => {
                    self.push_log(
                        LogKind::Info,
                        format!("wrote {} ({frames} frame(s))", path.display()),
                    );
                    self.last_saved = Some(path);
                }
                WorkerUpdate::Stopped => {
                    self.push_log(LogKind::Error, "the camera thread stopped");
                }
            }
        }
    }

    /// Convert the newest frame into a texture. Runs at most once per
    /// repaint, and only when a new frame actually arrived.
    fn update_texture(&mut self, ctx: &egui::Context) {
        let Some(frame) = self.worker.latest_frame() else {
            return;
        };

        // Subsample large sensors for display only. A 26 MPix frame does not
        // need to become a 26 MPix texture to be focused on.
        let longest = frame.width().max(frame.height());
        let subsample = (longest / MAX_DISPLAY_EDGE.max(1)).max(1);
        self.display = DisplayOptions {
            stretch: if self.auto_stretch {
                Stretch::auto()
            } else {
                Stretch::Linear
            },
            debayer: self.debayer,
            subsample,
            gamma: self.gamma,
        };

        let image = display::render(&frame, &self.display);
        self.last_levels = (image.black, image.white);
        self.last_meta = Some(frame.meta.clone());

        let colour = egui::ColorImage::from_rgba_unmultiplied(
            [image.width as usize, image.height as usize],
            &image.rgba,
        );
        match &mut self.texture {
            // Reusing the handle keeps the GPU allocation stable across
            // frames; a new one every frame would churn texture memory.
            Some(texture) if texture.size() == [image.width as usize, image.height as usize] => {
                texture.set(colour, egui::TextureOptions::NEAREST);
            }
            slot => {
                *slot = Some(ctx.load_texture("live-view", colour, egui::TextureOptions::NEAREST));
            }
        }

        let now = Instant::now();
        self.display_times.push_back(now);
        while self
            .display_times
            .front()
            .is_some_and(|t| now.duration_since(*t) > Duration::from_secs(1))
        {
            self.display_times.pop_front();
        }
    }
}

impl eframe::App for FirstLightApp {
    /// Runs before every repaint, and also while the window is hidden. All
    /// the talking to the camera thread happens here, so the drawing pass
    /// below is pure layout.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.tick(ctx);

        // Keep animating while a camera is attached: frames arrive on their
        // own schedule, not in response to input.
        if self.status.streaming || !matches!(self.status.state, ConnectionState::Disconnected) {
            ctx.request_repaint_after(Duration::from_millis(16));
        } else {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        crate::ui::left_panel(self, ui);
        crate::ui::log_panel(self, ui);
        crate::ui::central_panel(self, ui);
    }

    fn on_exit(&mut self) {
        // Finalise any recording before the process goes away.
        let _ = self.worker.send(WorkerCommand::StopRecording);
        let _ = self.worker.send(WorkerCommand::Disconnect);
    }
}
