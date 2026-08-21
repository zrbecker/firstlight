//! The camera IO thread.
//!
//! Every blocking or unpredictable operation — enumeration, open, control
//! writes that go over USB, frame waits, file writes — happens here, on one
//! thread that owns the camera. Callers (the GUI, the CLI) send commands and
//! read status; nothing they do can block on the device.
//!
//! Two channels leave the worker, and the split matters:
//!
//! * status and events go out on an unbounded queue, because losing an error
//!   report is unacceptable and the messages are tiny;
//! * frames go into a one-deep [`FrameRing`], because a live view wants the
//!   *newest* frame and a backlog of stale ones is worse than useless.
//!
//! Recording writes happen before the display hand-off and are never skipped,
//! so a slow or hidden UI cannot punch holes in a capture.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::{Receiver, Sender, TryRecvError, unbounded};

use crate::camera::{Camera, CameraId, CameraInfo};
use crate::control::{Binning, BitDepth, ControlId, ControlInfo, Roi, WhiteBalance};
use crate::dark::MasterDark;
use crate::error::{Error, Result};
use crate::event::CameraEvent;
use crate::format::fits::{FitsMetadata, write_fits};
use crate::format::sequence::FitsSequenceWriter;
use crate::format::ser::{SerMetadata, SerWriter};
use crate::frame::Frame;
use crate::registry::Registry;
use crate::ring::FrameRing;

/// How long the worker waits for a frame before looping round to check for
/// commands. Short, because responsiveness beats a few wasted wake-ups.
const FRAME_POLL: Duration = Duration::from_millis(50);
/// Idle loop period when not streaming.
const IDLE_POLL: Duration = Duration::from_millis(100);
/// How often a status snapshot is published.
const STATUS_PERIOD: Duration = Duration::from_millis(200);
/// Gap between reconnect attempts after a device loss.
const RECONNECT_PERIOD: Duration = Duration::from_millis(1000);
/// Slack added to the expected frame period before reporting a stalled stream.
const STALL_GRACE: Duration = Duration::from_secs(5);
/// How often read-only controls (a sensor temperature, say) are re-read.
/// Writable ones are only re-read when something might have changed them,
/// because each read is a round trip that competes with frame delivery.
const READONLY_REFRESH: Duration = Duration::from_secs(2);

/// Optional stopping condition for a recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RecordLimit {
    pub frames: Option<u64>,
    pub duration: Option<Duration>,
}

impl RecordLimit {
    pub fn frames(n: u64) -> RecordLimit {
        RecordLimit {
            frames: Some(n),
            duration: None,
        }
    }

    pub fn duration(d: Duration) -> RecordLimit {
        RecordLimit {
            frames: None,
            duration: Some(d),
        }
    }

    fn reached(&self, frames: u64, elapsed: Duration) -> bool {
        self.frames.is_some_and(|limit| frames >= limit)
            || self.duration.is_some_and(|limit| elapsed >= limit)
    }
}

/// What a recording writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RecordFormat {
    /// One FITS file per frame, numbered from a template. Each file carries
    /// the full acquisition header, which is what makes a run self-describing.
    #[default]
    FitsSequence,
    /// A single SER video file. Compact and what planetary stacking software
    /// expects, but it has nowhere to record exposure, gain or white balance.
    Ser,
}

/// Everything a recording run needs to know.
#[derive(Debug, Clone)]
pub struct RecordRequest {
    pub format: RecordFormat,
    /// For a FITS sequence, the template for the first file, e.g.
    /// `~/captures/m42/light_0001.fits`. For SER, the file itself.
    pub path: PathBuf,
    /// Stop after this many frames or this long. `None` runs until stopped.
    pub limit: Option<RecordLimit>,
    /// Idle time between one exposure ending and the next being kept.
    ///
    /// Zero saves every frame the camera delivers. Anything else makes the
    /// period between saved frames `exposure + delay`, so a one second
    /// exposure with a two second delay is kept every three seconds.
    pub delay: Duration,
}

impl RecordRequest {
    pub fn new(path: impl Into<PathBuf>) -> RecordRequest {
        RecordRequest {
            format: RecordFormat::default(),
            path: path.into(),
            limit: None,
            delay: Duration::ZERO,
        }
    }

    pub fn limit(mut self, limit: Option<RecordLimit>) -> RecordRequest {
        self.limit = limit;
        self
    }

    pub fn delay(mut self, delay: Duration) -> RecordRequest {
        self.delay = delay;
        self
    }

    pub fn format(mut self, format: RecordFormat) -> RecordRequest {
        self.format = format;
        self
    }
}

#[derive(Debug, Clone)]
pub enum WorkerCommand {
    /// Re-enumerate every backend.
    RefreshCameras,
    Connect(CameraId),
    Disconnect,
    StartStream,
    StopStream,
    SetControl {
        id: ControlId,
        value: i64,
    },
    SetRoi(Roi),
    SetBinning(Binning),
    SetBitDepth(BitDepth),
    /// Begin recording frames.
    StartRecording(RecordRequest),
    StopRecording,
    /// Save the next frame as FITS.
    Snap {
        path: PathBuf,
    },
    /// Ask the camera to measure and store its own white balance.
    AutoWhiteBalance,
    /// Collect frames for a master dark. The camera must be covered; nothing
    /// here can check that, so the caller is responsible for asking.
    CaptureDark {
        frames: usize,
    },
    /// Abandon a dark in progress.
    CancelDark,
    /// Keep trying to re-open the camera after a device loss.
    SetAutoReconnect(bool),
    Shutdown,
}

/// Where the camera connection currently stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    /// The device vanished. `reason` is what the backend said.
    Lost {
        reason: String,
    },
    /// Actively retrying after a loss.
    Reconnecting {
        attempt: u32,
        reason: String,
    },
}

impl ConnectionState {
    pub fn is_connected(&self) -> bool {
        matches!(self, ConnectionState::Connected)
    }

    pub fn label(&self) -> String {
        match self {
            ConnectionState::Disconnected => "disconnected".into(),
            ConnectionState::Connecting => "connecting...".into(),
            ConnectionState::Connected => "connected".into(),
            ConnectionState::Lost { reason } => format!("device lost: {reason}"),
            ConnectionState::Reconnecting { attempt, .. } => {
                format!("reconnecting (attempt {attempt})...")
            }
        }
    }
}

/// Live geometry and exposure settings, as last read from the camera.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CameraSettings {
    pub exposure_us: u64,
    pub gain: i64,
    pub offset: i64,
    pub roi: Roi,
    pub binning: Binning,
    pub bit_depth: BitDepth,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecordingProgress {
    /// The SER file, or the directory a FITS sequence is filling.
    pub path: PathBuf,
    /// The most recent file written, for a sequence.
    pub last_file: Option<PathBuf>,
    pub format: RecordFormat,
    pub frames: u64,
    pub bytes: u64,
    pub elapsed: Duration,
    pub limit: Option<RecordLimit>,
    /// Time until the next frame is due to be kept, when a delay is set.
    pub next_in: Option<Duration>,
}

/// A snapshot of everything a UI needs to render, published periodically.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkerStatus {
    pub state: ConnectionState,
    pub camera: Option<CameraInfo>,
    pub streaming: bool,
    pub settings: CameraSettings,
    /// Frames per second, measured over the last second of delivery.
    pub fps: f32,
    /// Frames the worker received since the stream started.
    pub frames_received: u64,
    /// Frames the backend threw away because the worker was behind.
    pub camera_dropped: u64,
    /// Frames the worker threw away because the UI was behind. These were
    /// still recorded; only the live view skipped them.
    pub display_dropped: u64,
    pub recording: Option<RecordingProgress>,
    pub temperature_c: Option<f32>,
    /// Set when the stream has gone quiet for longer than it should have.
    pub stalled: bool,
    /// Frames collected so far for a master dark, and how many were asked
    /// for. `None` when no dark is being taken.
    pub dark_progress: Option<(usize, usize)>,
    /// What the camera says each of its controls is currently set to.
    ///
    /// Read from the device rather than assumed, because a camera keeps its
    /// own state: white balance gains, gamma and the rest survive a power
    /// cycle and may have been left somewhere unexpected by other software.
    /// A UI that shows defaults instead is lying about the hardware.
    pub control_values: BTreeMap<ControlId, i64>,
}

impl Default for WorkerStatus {
    fn default() -> Self {
        WorkerStatus {
            state: ConnectionState::Disconnected,
            camera: None,
            streaming: false,
            settings: CameraSettings::default(),
            fps: 0.0,
            frames_received: 0,
            camera_dropped: 0,
            display_dropped: 0,
            recording: None,
            temperature_c: None,
            stalled: false,
            dark_progress: None,
            control_values: BTreeMap::new(),
        }
    }
}

/// Messages from the worker to whoever owns the handle.
#[derive(Debug, Clone)]
pub enum WorkerUpdate {
    /// Result of enumeration, plus any per-backend failures as text, plus
    /// notes from backends that cannot see anything in this build.
    Cameras {
        cameras: Vec<CameraInfo>,
        errors: Vec<String>,
        notes: Vec<String>,
    },
    /// Control table of the camera that just connected.
    Controls(Vec<ControlInfo>),
    /// Boxed: a status snapshot dwarfs every other variant, and this enum
    /// travels through a channel several times a second.
    Status(Box<WorkerStatus>),
    /// An asynchronous device event, passed through for the log.
    Event(CameraEvent),
    /// An operation failed. `context` says which one.
    Failed {
        context: String,
        message: String,
        fatal: bool,
    },
    /// A file was written.
    Saved { path: PathBuf, frames: u64 },
    /// A dark capture ended, carrying the master if one was built and
    /// `None` if it was cancelled or abandoned. Sent exactly once per
    /// capture, whatever the outcome, so a client showing progress always
    /// learns that it is over — the status is not enough on its own,
    /// because it arrives separately and a moment later. Boxed because a
    /// master carries a frame's worth of pixels and this enum travels
    /// through a channel constantly.
    DarkFinished(Option<Box<MasterDark>>),
    /// The worker has stopped; no further updates will arrive.
    Stopped,
}

/// Handle to a running worker thread.
///
/// Dropping the handle shuts the worker down and joins it, which also
/// finalises any recording in progress.
pub struct WorkerHandle {
    commands: Sender<WorkerCommand>,
    updates: Receiver<WorkerUpdate>,
    frames: Arc<FrameRing>,
    thread: Option<JoinHandle<()>>,
}

impl WorkerHandle {
    /// Start a worker over the given backends. It enumerates once at startup
    /// and then does nothing until commanded.
    pub fn spawn(registry: Registry) -> WorkerHandle {
        let (cmd_tx, cmd_rx) = unbounded();
        let (update_tx, update_rx) = unbounded();
        // One frame deep: the display always wants the newest frame.
        let frames = Arc::new(FrameRing::new(1));
        let worker_frames = frames.clone();

        let thread = thread::Builder::new()
            .name("firstlight-camera".into())
            .spawn(move || {
                let mut worker = Worker::new(registry, cmd_rx, update_tx.clone(), worker_frames);
                worker.run();
                let _ = update_tx.send(WorkerUpdate::Stopped);
            })
            .expect("spawning the camera thread");

        WorkerHandle {
            commands: cmd_tx,
            updates: update_rx,
            frames,
            thread: Some(thread),
        }
    }

    /// Queue a command. Fails only if the worker thread has already exited.
    pub fn send(&self, command: WorkerCommand) -> Result<()> {
        self.commands
            .send(command)
            .map_err(|_| Error::ChannelClosed)
    }

    pub fn updates(&self) -> &Receiver<WorkerUpdate> {
        &self.updates
    }

    /// The most recent frame, if one arrived since the last call. Returns
    /// `None` rather than waiting, so a UI can call it every repaint.
    pub fn latest_frame(&self) -> Option<Frame> {
        let mut latest = None;
        while let Some(frame) = self.frames.try_recv() {
            latest = Some(frame);
        }
        latest
    }

    /// Frames dropped on the way to the display. They were still recorded.
    pub fn display_dropped(&self) -> u64 {
        self.frames.dropped()
    }

    /// The queue frames arrive on.
    ///
    /// Handed out so a caller can do its own rendering on its own thread
    /// rather than converting frames on whatever thread happens to be
    /// drawing. Taking frames from here means [`WorkerHandle::latest_frame`]
    /// will not see them: pick one or the other.
    pub fn frame_ring(&self) -> Arc<FrameRing> {
        self.frames.clone()
    }

    /// Stop the worker and wait for it, finalising any recording.
    pub fn shutdown(mut self) {
        self.shutdown_inner();
    }

    fn shutdown_inner(&mut self) {
        let _ = self.commands.send(WorkerCommand::Shutdown);
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            tracing::error!("camera thread panicked");
        }
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        self.shutdown_inner();
    }
}

/// Where a recording's frames are going.
enum RecordingSink {
    Ser(SerWriter),
    FitsSequence(Box<FitsSequenceWriter>),
}

struct Recording {
    sink: RecordingSink,
    /// The SER file, or the directory of a FITS sequence.
    path: PathBuf,
    format: RecordFormat,
    frames: u64,
    started: Instant,
    limit: Option<RecordLimit>,
    /// Gap the user asked for between exposures; zero keeps every frame.
    delay: Duration,
    /// When the next frame is due to be kept, once the run has started.
    /// Held as an absolute time and advanced by whole periods, so the cadence
    /// cannot drift. `None` until the first frame anchors the grid.
    next_due: Option<Instant>,
}

impl Recording {
    fn bytes(&self) -> u64 {
        match &self.sink {
            RecordingSink::Ser(writer) => writer.bytes_written(),
            RecordingSink::FitsSequence(writer) => writer.bytes_written(),
        }
    }

    fn last_file(&self) -> Option<PathBuf> {
        match &self.sink {
            RecordingSink::Ser(_) => None,
            RecordingSink::FitsSequence(writer) => {
                writer.last_written().map(|path| path.to_path_buf())
            }
        }
    }

    /// Whether this frame is the one to keep, given the cadence.
    ///
    /// The camera free-runs whatever delay is asked for, so a delay is served
    /// by discarding frames rather than by idling the sensor — stopping and
    /// restarting a stream is not reliable enough to do between exposures.
    /// The tolerance matters: with a delay that is a whole multiple of the
    /// frame period, a frame arriving a millisecond early would otherwise be
    /// skipped and the cadence would slip by a full period every time.
    fn due(&self, now: Instant, exposure: Duration) -> bool {
        if self.delay.is_zero() {
            return true;
        }
        match self.next_due {
            // The first frame starts the run, and the grid starts with it.
            None => true,
            Some(due) => now + exposure / 2 >= due,
        }
    }

    fn period(&self, exposure: Duration) -> Duration {
        exposure + self.delay
    }

    /// Move the due time on by whole periods, catching up if frames were
    /// missed so the schedule stays on its original grid.
    fn advance(&mut self, now: Instant, exposure: Duration) {
        let period = self.period(exposure).max(Duration::from_millis(1));
        let mut due = self.next_due.unwrap_or(now) + period;
        while due <= now {
            due += period;
        }
        self.next_due = Some(due);
    }

    fn next_in(&self, now: Instant) -> Option<Duration> {
        if self.delay.is_zero() {
            return None;
        }
        Some(self.next_due?.saturating_duration_since(now))
    }
}

struct Worker {
    registry: Registry,
    commands: Receiver<WorkerCommand>,
    updates: Sender<WorkerUpdate>,
    frames: Arc<FrameRing>,

    camera: Option<Box<dyn Camera>>,
    events: Option<Receiver<CameraEvent>>,
    info: Option<CameraInfo>,
    /// Key used to find the same physical device again after a replug.
    reconnect_key: Option<String>,
    backend_name: Option<&'static str>,

    state: ConnectionState,
    /// The user's preference, changed only by `SetAutoReconnect`.
    auto_reconnect: bool,
    /// True between a successful connect and an explicit disconnect. Kept
    /// separate from the preference above so that disconnecting on purpose
    /// does not quietly turn the user's setting off, and reconnecting later
    /// does not quietly turn it back on.
    session: bool,
    reconnect_attempt: u32,
    next_reconnect: Option<Instant>,
    /// Whether the stream should be running, as opposed to whether it is.
    /// Reconnect restores intent, not just the connection.
    want_stream: bool,

    settings: CameraSettings,
    /// The camera's control table, kept so its values can be re-read.
    controls: Vec<ControlInfo>,
    /// Last known value of every control, as reported by the camera.
    control_values: BTreeMap<ControlId, i64>,
    last_readonly_refresh: Option<Instant>,
    /// Settings the user asked for, re-applied after every reconnect.
    desired: Vec<(ControlId, i64)>,

    recording: Option<Recording>,
    /// Frames gathered so far towards a master dark, with the target count.
    dark: Option<(Vec<Frame>, usize)>,
    /// Whether the stream was already running when the dark started, so it
    /// can be left as it was found.
    dark_was_streaming: bool,
    /// Path to write the next frame to as FITS.
    pending_snap: Option<PathBuf>,

    frames_received: u64,
    frame_times: std::collections::VecDeque<Instant>,
    last_frame_at: Option<Instant>,
    stall_reported: bool,
    last_status: Option<Instant>,
    running: bool,
}

impl Worker {
    fn new(
        registry: Registry,
        commands: Receiver<WorkerCommand>,
        updates: Sender<WorkerUpdate>,
        frames: Arc<FrameRing>,
    ) -> Worker {
        Worker {
            registry,
            commands,
            updates,
            frames,
            camera: None,
            events: None,
            info: None,
            reconnect_key: None,
            backend_name: None,
            state: ConnectionState::Disconnected,
            auto_reconnect: true,
            session: false,
            reconnect_attempt: 0,
            next_reconnect: None,
            want_stream: false,
            settings: CameraSettings::default(),
            controls: Vec::new(),
            control_values: BTreeMap::new(),
            last_readonly_refresh: None,
            desired: Vec::new(),
            recording: None,
            dark: None,
            dark_was_streaming: false,
            pending_snap: None,
            frames_received: 0,
            frame_times: std::collections::VecDeque::new(),
            last_frame_at: None,
            stall_reported: false,
            last_status: None,
            running: true,
        }
    }

    fn run(&mut self) {
        self.enumerate();
        self.publish_status();

        while self.running {
            self.drain_commands();
            if !self.running {
                break;
            }
            self.drain_events();
            self.try_reconnect();

            self.check_liveness();

            // Keep live readings (sensor temperature and the like) current
            // without polling the whole table over USB.
            if self.camera.is_some()
                && self
                    .last_readonly_refresh
                    .is_none_or(|t| t.elapsed() >= READONLY_REFRESH)
            {
                self.refresh_control_values(false);
                self.last_readonly_refresh = Some(Instant::now());
            }

            if self.camera.is_some() && self.streaming() {
                self.pump_frame();
            } else {
                // Nothing to poll: block on the command channel so an idle
                // worker costs nothing.
                match self.commands.recv_timeout(IDLE_POLL) {
                    Ok(command) => self.handle(command),
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        // Handle dropped without a Shutdown; tidy up anyway.
                        self.running = false;
                    }
                }
            }

            if self
                .last_status
                .is_none_or(|t| t.elapsed() >= STATUS_PERIOD)
            {
                self.publish_status();
            }
        }

        self.finish_recording("worker shutting down");
        if let Some(camera) = self.camera.as_mut() {
            let _ = camera.stop_streaming();
            let _ = camera.disconnect();
        }
        self.camera = None;
        self.state = ConnectionState::Disconnected;
        self.publish_status();
    }

    fn streaming(&self) -> bool {
        self.camera.as_ref().is_some_and(|c| c.is_streaming())
    }

    fn drain_commands(&mut self) {
        loop {
            match self.commands.try_recv() {
                Ok(command) => self.handle(command),
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.running = false;
                    return;
                }
            }
        }
    }

    fn handle(&mut self, command: WorkerCommand) {
        match command {
            WorkerCommand::RefreshCameras => self.enumerate(),
            WorkerCommand::Connect(id) => self.connect(&id),
            WorkerCommand::Disconnect => {
                self.finish_recording("disconnecting");
                self.want_stream = false;
                self.session = false;
                self.next_reconnect = None;
                if let Some(camera) = self.camera.as_mut() {
                    let _ = camera.stop_streaming();
                    if let Err(e) = camera.disconnect() {
                        self.fail("disconnect", &e);
                    }
                }
                self.camera = None;
                self.events = None;
                self.state = ConnectionState::Disconnected;
                self.publish_status();
            }
            WorkerCommand::StartStream => {
                self.want_stream = true;
                self.reset_rate_stats();
                let result = self.with_camera("start stream", |camera| camera.start_streaming());
                if result.is_some() {
                    self.publish_status();
                }
            }
            WorkerCommand::StopStream => {
                self.want_stream = false;
                self.with_camera("stop stream", |camera| camera.stop_streaming());
                self.publish_status();
            }
            WorkerCommand::SetControl { id, value } => {
                if self
                    .with_camera("set control", |camera| camera.set_control(id, value))
                    .is_some()
                {
                    // Remember it so a reconnect restores the same state.
                    self.desired.retain(|(existing, _)| *existing != id);
                    self.desired.push((id, value));
                    // Read back rather than assuming: cameras clamp and round.
                    self.refresh_control_values(true);
                    self.read_settings();
                    self.publish_status();
                }
            }
            WorkerCommand::SetRoi(roi) => {
                if self
                    .with_camera("set ROI", |camera| camera.set_roi(roi))
                    .is_some()
                {
                    self.read_settings();
                    self.publish_status();
                }
            }
            WorkerCommand::SetBinning(binning) => {
                if self
                    .with_camera("set binning", |camera| camera.set_binning(binning))
                    .is_some()
                {
                    self.read_settings();
                    self.publish_status();
                }
            }
            WorkerCommand::SetBitDepth(depth) => {
                if self
                    .with_camera("set bit depth", |camera| camera.set_bit_depth(depth))
                    .is_some()
                {
                    self.read_settings();
                    self.publish_status();
                }
            }
            WorkerCommand::StartRecording(request) => self.start_recording(request),
            WorkerCommand::StopRecording => self.finish_recording("stopped by request"),
            WorkerCommand::Snap { path } => {
                self.pending_snap = Some(path);
                // A still with no stream running still has to work.
                if !self.streaming() {
                    self.with_camera("start stream for snapshot", |camera| {
                        camera.start_streaming()
                    });
                }
            }
            WorkerCommand::AutoWhiteBalance => self.auto_white_balance(),
            WorkerCommand::CaptureDark { frames } => self.start_dark(frames),
            WorkerCommand::CancelDark => self.finish_dark(None),
            WorkerCommand::SetAutoReconnect(on) => {
                self.auto_reconnect = on;
                self.next_reconnect = match (on, &self.state) {
                    // Turning it back on while the device is missing should
                    // start retrying now, not wait for the next loss.
                    (true, ConnectionState::Lost { .. })
                    | (true, ConnectionState::Reconnecting { .. }) => {
                        Some(Instant::now() + RECONNECT_PERIOD)
                    }
                    (true, _) => self.next_reconnect,
                    (false, _) => None,
                };
            }
            WorkerCommand::Shutdown => self.running = false,
        }
    }

    fn auto_white_balance(&mut self) {
        if self
            .with_camera("automatic white balance", |camera| {
                camera.auto_white_balance()
            })
            .is_some()
        {
            // The gains have just been rewritten; read them back so the
            // sliders show what was chosen.
            self.refresh_control_values(true);
            self.read_settings();
            self.publish_status();
        }
    }

    /// The white balance gains the camera currently reports, if it has them.
    fn white_balance_snapshot(&self) -> Option<WhiteBalance> {
        Some(WhiteBalance {
            red: *self.control_values.get(&ControlId::WbRed)?,
            green: *self.control_values.get(&ControlId::WbGreen)?,
            blue: *self.control_values.get(&ControlId::WbBlue)?,
        })
    }

    /// Begin collecting frames for a master dark.
    ///
    /// The camera must already be covered. Nothing here can verify that from
    /// a single frame — a dim scene and a covered sensor look alike until you
    /// see the whole picture — so the master checks itself once it is built
    /// and says so if it looks wrong.
    fn start_dark(&mut self, frames: usize) {
        let frames = frames.clamp(1, 256);
        self.dark = Some((Vec::with_capacity(frames), frames));
        self.dark_was_streaming = self.streaming();
        if !self.dark_was_streaming
            && self
                .with_camera("start stream for darks", |camera| camera.start_streaming())
                .is_none()
        {
            self.finish_dark(None);
            return;
        }
        self.publish_status();
    }

    /// Add a frame to the dark in progress, and build the master once enough
    /// have arrived.
    fn collect_dark(&mut self, frame: &Frame) {
        // Only frames that describe themselves: a dark taken across a
        // settings change would be an average of two different cameras.
        if !frame.meta.settings_settled {
            return;
        }
        let done = match self.dark.as_mut() {
            Some((frames, target)) => {
                frames.push(frame.clone());
                frames.len() >= *target
            }
            None => return,
        };
        if done {
            let (frames, _) = self.dark.take().expect("checked just above");
            match MasterDark::from_frames(&frames) {
                Ok(dark) => self.finish_dark(Some(dark)),
                Err(e) => {
                    self.fail("master dark", &e);
                    self.finish_dark(None);
                }
            }
        } else {
            self.publish_status();
        }
    }

    /// Publish a finished master, or clean up an abandoned one, and put the
    /// stream back the way it was found.
    fn finish_dark(&mut self, dark: Option<MasterDark>) {
        self.dark = None;
        if let Some(dark) = dark {
            for complaint in dark.complaints(u16::MAX) {
                let _ = self.updates.send(WorkerUpdate::Event(CameraEvent::Warning {
                    message: complaint,
                }));
            }
            let _ = self
                .updates
                .send(WorkerUpdate::DarkFinished(Some(Box::new(dark))));
        } else {
            let _ = self.updates.send(WorkerUpdate::DarkFinished(None));
        }
        if !self.dark_was_streaming {
            self.with_camera("stop stream after darks", |camera| camera.stop_streaming());
        }
        self.publish_status();
    }

    fn enumerate(&mut self) {
        let (cameras, errors) = self.registry.enumerate();
        let errors = errors
            .into_iter()
            .map(|(backend, e)| format!("{backend}: {e}"))
            .collect();
        let notes = self.registry.notes();
        let _ = self.updates.send(WorkerUpdate::Cameras {
            cameras,
            errors,
            notes,
        });
    }

    fn connect(&mut self, id: &CameraId) {
        self.finish_recording("connecting to another camera");
        if let Some(camera) = self.camera.as_mut() {
            let _ = camera.stop_streaming();
            let _ = camera.disconnect();
        }
        self.camera = None;
        self.events = None;
        self.state = ConnectionState::Connecting;
        self.publish_status();

        match self.registry.open_any(id) {
            Ok(camera) => {
                let info = camera.info().clone();
                self.reconnect_key = Some(info.reconnect_key().to_string());
                self.backend_name = Some(info.backend);
                self.info = Some(info);
                self.events = Some(camera.events());
                self.camera = Some(camera);
                self.state = ConnectionState::Connected;
                self.reconnect_attempt = 0;
                self.next_reconnect = None;
                self.session = true;
                self.desired.clear();
                self.publish_controls();
                self.refresh_control_values(true);
                self.read_settings();
                self.publish_status();
            }
            Err(e) => {
                self.state = ConnectionState::Disconnected;
                self.fail("connect", &e);
                self.publish_status();
            }
        }
    }

    /// Run an operation on the camera, reporting failures and demoting the
    /// connection when the error says the device is gone.
    fn with_camera<T>(
        &mut self,
        context: &str,
        operation: impl FnOnce(&mut Box<dyn Camera>) -> Result<T>,
    ) -> Option<T> {
        let Some(camera) = self.camera.as_mut() else {
            self.fail(context, &Error::NotConnected);
            return None;
        };
        match operation(camera) {
            Ok(value) => Some(value),
            Err(e) => {
                let fatal = e.is_fatal();
                self.fail(context, &e);
                if fatal {
                    self.device_lost(e.to_string());
                }
                None
            }
        }
    }

    fn publish_controls(&mut self) {
        let controls = self
            .camera
            .as_ref()
            .and_then(|camera| camera.controls().ok())
            .unwrap_or_default();
        self.controls = controls.clone();
        let _ = self.updates.send(WorkerUpdate::Controls(controls));
    }

    /// Re-read control values from the camera.
    ///
    /// With `writable_too` this reads the whole table, which is what to do
    /// after connecting or after changing something. Without it only the
    /// read-only controls are refreshed, which is the cheap periodic case:
    /// nothing else can change behind our back.
    fn refresh_control_values(&mut self, writable_too: bool) {
        let Some(camera) = self.camera.as_ref() else {
            return;
        };
        for control in &self.controls {
            if !writable_too && !control.read_only {
                continue;
            }
            // A control that will not answer is not worth failing over; it
            // simply keeps whatever value was last known.
            if let Ok(value) = camera.control(control.id) {
                self.control_values.insert(control.id, value);
            }
        }
        // A full pass covers the read-only ones too, so the periodic timer
        // can start again from here.
        if writable_too {
            self.last_readonly_refresh = Some(Instant::now());
        }
    }

    fn read_settings(&mut self) {
        let Some(camera) = self.camera.as_ref() else {
            return;
        };
        // Individual controls a camera does not implement are not an error
        // here; the status simply keeps its previous value.
        let mut settings = self.settings.clone();
        if let Ok(v) = camera.exposure_us() {
            settings.exposure_us = v;
        }
        if let Ok(v) = camera.gain() {
            settings.gain = v;
        }
        if let Ok(v) = camera.offset() {
            settings.offset = v;
        }
        if let Ok(v) = camera.roi() {
            settings.roi = v;
        }
        if let Ok(v) = camera.binning() {
            settings.binning = v;
        }
        if let Ok(v) = camera.bit_depth() {
            settings.bit_depth = v;
        }
        self.settings = settings;
    }

    fn pump_frame(&mut self) {
        let result = self
            .camera
            .as_mut()
            .map(|camera| camera.next_frame(FRAME_POLL));
        match result {
            Some(Ok(frame)) => {
                self.last_frame_at = Some(Instant::now());
                self.stall_reported = false;
                self.frames_received += 1;
                self.note_frame_time();
                self.record_frame(&frame);
                self.snap_frame(&frame);
                self.collect_dark(&frame);
                // Display last: recording must never be starved by the UI.
                self.frames.push(frame);
            }
            Some(Err(Error::Timeout(_))) => self.check_stall(),
            Some(Err(e)) => {
                let fatal = e.is_fatal();
                self.fail("frame capture", &e);
                if fatal {
                    self.device_lost(e.to_string());
                } else {
                    // Non-fatal but real: stop the stream rather than spin.
                    self.with_camera("stop stream after error", |camera| camera.stop_streaming());
                }
            }
            None => {}
        }
    }

    /// A camera can vanish while idle, and nobody would find out until the
    /// next command failed. Backends track this locally (no bus traffic), so
    /// checking every loop is free and keeps the UI honest.
    fn check_liveness(&mut self) {
        if self
            .camera
            .as_ref()
            .is_some_and(|camera| !camera.is_connected())
        {
            self.device_lost("camera reports it is no longer connected".into());
        }
    }

    fn note_frame_time(&mut self) {
        let now = Instant::now();
        self.frame_times.push_back(now);
        while self
            .frame_times
            .front()
            .is_some_and(|t| now.duration_since(*t) > Duration::from_secs(1))
        {
            self.frame_times.pop_front();
        }
    }

    fn reset_rate_stats(&mut self) {
        self.frame_times.clear();
        self.frames_received = 0;
        self.last_frame_at = None;
        self.stall_reported = false;
        self.frames.reset();
    }

    /// A stream that has gone quiet for much longer than the exposure implies
    /// is stuck. Say so once, rather than leaving the UI showing a frozen
    /// image with no explanation.
    fn check_stall(&mut self) {
        let expected = Duration::from_micros(self.settings.exposure_us.saturating_mul(3))
            .saturating_add(STALL_GRACE);
        let since = self
            .last_frame_at
            .map(|t| t.elapsed())
            .unwrap_or_else(|| Duration::ZERO);
        if self.last_frame_at.is_some() && since > expected && !self.stall_reported {
            self.stall_reported = true;
            let _ = self
                .updates
                .send(WorkerUpdate::Event(CameraEvent::FrameTimeout));
            let _ = self.updates.send(WorkerUpdate::Failed {
                context: "frame capture".into(),
                message: format!("no frame for {:.1}s", since.as_secs_f32()),
                fatal: false,
            });
        }
    }

    fn record_frame(&mut self, frame: &Frame) {
        let exposure = Duration::from_micros(frame.meta.exposure_us);
        let Some(recording) = self.recording.as_mut() else {
            return;
        };
        // A frame that was already integrating when a setting changed carries
        // the old value while being labelled with the new one. Never write
        // that to a file.
        if !frame.meta.settings_settled {
            return;
        }

        // A frame arriving inside the gap the user asked for is discarded
        // rather than written; it still reaches the live view.
        let now = Instant::now();
        if !recording.due(now, exposure) {
            return;
        }
        recording.advance(now, exposure);

        let written = match &mut recording.sink {
            RecordingSink::Ser(writer) => writer.write_frame(frame),
            RecordingSink::FitsSequence(writer) => writer.write_frame(frame).map(|_| ()),
        };
        if let Err(e) = written {
            let message = e.to_string();
            self.finish_recording("write error");
            let _ = self.updates.send(WorkerUpdate::Failed {
                context: "recording".into(),
                message,
                fatal: false,
            });
            return;
        }
        recording.frames += 1;
        let done = recording
            .limit
            .is_some_and(|limit| limit.reached(recording.frames, recording.started.elapsed()));
        if done {
            self.finish_recording("limit reached");
        }
    }

    fn snap_frame(&mut self, frame: &Frame) {
        if self.pending_snap.is_some() && !frame.meta.settings_settled {
            // Wait for one that describes itself; the request stays pending.
            return;
        }
        let Some(path) = self.pending_snap.take() else {
            return;
        };
        let info = self.info.clone();
        let meta = FitsMetadata {
            instrument: info
                .as_ref()
                .map(|i| i.display_name.clone())
                .unwrap_or_default(),
            pixel_size_um: info.as_ref().map(|i| i.pixel_size_um),
            // These cameras bake the white balance into the raw data, so the
            // file has to carry the gains or nobody can undo them later.
            white_balance: self.white_balance_snapshot(),
            ..FitsMetadata::default()
        };
        match write_fits(&path, frame, &meta) {
            Ok(()) => {
                let _ = self.updates.send(WorkerUpdate::Saved { path, frames: 1 });
            }
            Err(e) => {
                let _ = self.updates.send(WorkerUpdate::Failed {
                    context: "snapshot".into(),
                    message: format!("{}: {e}", path.display()),
                    fatal: false,
                });
            }
        }
        // A snapshot taken with the stream stopped leaves it stopped.
        if !self.want_stream {
            self.with_camera("stop stream after snapshot", |camera| {
                camera.stop_streaming()
            });
        }
    }

    fn start_recording(&mut self, request: RecordRequest) {
        self.finish_recording("superseded by a new recording");
        let instrument = self
            .info
            .as_ref()
            .map(|i| i.display_name.clone())
            .unwrap_or_default();

        let opened = match request.format {
            RecordFormat::Ser => {
                SerWriter::create(&request.path, SerMetadata::for_camera(instrument))
                    .map(|writer| (RecordingSink::Ser(writer), request.path.clone()))
            }
            RecordFormat::FitsSequence => {
                let meta = FitsMetadata {
                    instrument,
                    pixel_size_um: self.info.as_ref().map(|i| i.pixel_size_um),
                    white_balance: self.white_balance_snapshot(),
                    ..FitsMetadata::default()
                };
                FitsSequenceWriter::create(&request.path, meta).map(|writer| {
                    let directory = writer.directory().to_path_buf();
                    (RecordingSink::FitsSequence(Box::new(writer)), directory)
                })
            }
        };

        match opened {
            Ok((sink, path)) => {
                if let RecordingSink::FitsSequence(writer) = &sink
                    && writer.skipped_existing()
                {
                    // Their frames will not be numbered where they asked, and
                    // finding that out afterwards would be worse.
                    let _ = self.updates.send(WorkerUpdate::Event(CameraEvent::Warning {
                        message: format!(
                            "files already exist there; this run starts at number {}",
                            writer.first_index()
                        ),
                    }));
                }
                let now = Instant::now();
                self.recording = Some(Recording {
                    sink,
                    path,
                    format: request.format,
                    frames: 0,
                    started: now,
                    limit: request.limit,
                    delay: request.delay,
                    next_due: None,
                });
                // Recording without a stream would silently produce nothing.
                if !self.streaming() {
                    self.want_stream = true;
                    self.with_camera("start stream for recording", |camera| {
                        camera.start_streaming()
                    });
                }
                self.publish_status();
            }
            Err(e) => {
                let _ = self.updates.send(WorkerUpdate::Failed {
                    context: "start recording".into(),
                    message: e.to_string(),
                    fatal: false,
                });
            }
        }
    }

    fn finish_recording(&mut self, reason: &str) {
        let Some(recording) = self.recording.take() else {
            return;
        };
        let path = recording.path.clone();
        let frames = recording.frames;
        tracing::info!(path = %path.display(), frames, reason, "finishing recording");
        let outcome = match recording.sink {
            // SER needs its header patched with the frame count; a sequence
            // of FITS files is complete as soon as the last one is written.
            RecordingSink::Ser(writer) => writer.finish().map(u64::from),
            RecordingSink::FitsSequence(writer) => Ok(writer.frames()),
        };
        match outcome {
            Ok(written) => {
                let _ = self.updates.send(WorkerUpdate::Saved {
                    path,
                    frames: written,
                });
            }
            Err(e) => {
                let _ = self.updates.send(WorkerUpdate::Failed {
                    context: "finalise recording".into(),
                    message: format!("{}: {e}", path.display()),
                    fatal: false,
                });
            }
        }
        self.publish_status();
    }

    fn drain_events(&mut self) {
        let Some(events) = self.events.clone() else {
            return;
        };
        while let Ok(event) = events.try_recv() {
            let fatal = event.is_fatal();
            let reason = event.to_string();
            let _ = self.updates.send(WorkerUpdate::Event(event));
            if fatal {
                self.device_lost(reason);
            }
        }
    }

    /// Tear down a dead handle and arm the reconnect timer.
    fn device_lost(&mut self, reason: String) {
        if matches!(
            self.state,
            ConnectionState::Lost { .. } | ConnectionState::Reconnecting { .. }
        ) {
            return;
        }
        tracing::warn!(reason, "camera lost");
        // Keep the partial recording: an interrupted capture is still data.
        self.finish_recording("device lost");
        if let Some(camera) = self.camera.as_mut() {
            let _ = camera.stop_streaming();
            let _ = camera.disconnect();
        }
        self.camera = None;
        self.events = None;
        self.state = ConnectionState::Lost { reason };
        self.reconnect_attempt = 0;
        self.next_reconnect =
            (self.auto_reconnect && self.session).then(|| Instant::now() + RECONNECT_PERIOD);
        self.publish_status();
    }

    fn try_reconnect(&mut self) {
        if !self.auto_reconnect || !self.session || self.camera.is_some() {
            return;
        }
        let (Some(deadline), Some(key), Some(backend_name)) = (
            self.next_reconnect,
            self.reconnect_key.clone(),
            self.backend_name,
        ) else {
            return;
        };
        if Instant::now() < deadline {
            return;
        }
        self.reconnect_attempt += 1;
        let reason = match &self.state {
            ConnectionState::Lost { reason } => reason.clone(),
            ConnectionState::Reconnecting { reason, .. } => reason.clone(),
            _ => String::new(),
        };
        self.state = ConnectionState::Reconnecting {
            attempt: self.reconnect_attempt,
            reason,
        };
        self.next_reconnect = Some(Instant::now() + RECONNECT_PERIOD);

        let Some(backend) = self.registry.backend(backend_name) else {
            return;
        };
        let found = match backend.find_by_key(&key) {
            Ok(found) => found,
            Err(e) => {
                tracing::debug!(error = %e, "enumeration failed during reconnect");
                None
            }
        };
        let Some(info) = found else {
            self.publish_status();
            return;
        };

        match backend.open(&info.id) {
            Ok(camera) => {
                tracing::info!(camera = %info.id, "camera reconnected");
                self.events = Some(camera.events());
                self.info = Some(camera.info().clone());
                self.camera = Some(camera);
                self.state = ConnectionState::Connected;
                self.reconnect_attempt = 0;
                self.next_reconnect = None;
                let _ = self
                    .updates
                    .send(WorkerUpdate::Event(CameraEvent::Reconnected));
                self.publish_controls();
                self.reapply_settings();
                if self.want_stream {
                    self.reset_rate_stats();
                    self.with_camera("restart stream after reconnect", |camera| {
                        camera.start_streaming()
                    });
                }
                self.publish_status();
            }
            Err(e) => {
                // Common right after a replug: the device is enumerated but
                // the driver has not finished claiming it. Keep retrying.
                tracing::debug!(error = %e, "reconnect open failed");
                self.publish_status();
            }
        }
    }

    /// Push the geometry and controls the user chose back onto a camera that
    /// has just come back, so a replug is invisible apart from the gap.
    fn reapply_settings(&mut self) {
        let settings = self.settings.clone();
        let desired = self.desired.clone();
        self.with_camera("restore bit depth", |camera| {
            camera.set_bit_depth(settings.bit_depth)
        });
        self.with_camera("restore binning", |camera| {
            camera.set_binning(settings.binning)
        });
        self.with_camera("restore ROI", |camera| camera.set_roi(settings.roi));
        for (id, value) in desired {
            self.with_camera("restore control", |camera| camera.set_control(id, value));
        }
        self.refresh_control_values(true);
        self.read_settings();
    }

    fn fail(&mut self, context: &str, error: &Error) {
        tracing::warn!(context, error = %error, "camera operation failed");
        let _ = self.updates.send(WorkerUpdate::Failed {
            context: context.to_string(),
            message: error.to_string(),
            fatal: error.is_fatal(),
        });
    }

    fn publish_status(&mut self) {
        self.last_status = Some(Instant::now());
        let temperature = self.camera.as_ref().and_then(|c| c.temperature_c().ok());
        let status = WorkerStatus {
            state: self.state.clone(),
            camera: self.info.clone(),
            streaming: self.streaming(),
            settings: self.settings.clone(),
            fps: self.frame_times.len() as f32,
            frames_received: self.frames_received,
            camera_dropped: self.camera.as_ref().map_or(0, |c| c.dropped_frames()),
            display_dropped: self.frames.dropped(),
            recording: self.recording.as_ref().map(|r| RecordingProgress {
                path: r.path.clone(),
                last_file: r.last_file(),
                format: r.format,
                frames: r.frames,
                bytes: r.bytes(),
                elapsed: r.started.elapsed(),
                limit: r.limit,
                next_in: r.next_in(Instant::now()),
            }),
            temperature_c: temperature,
            stalled: self.stall_reported,
            dark_progress: self
                .dark
                .as_ref()
                .map(|(frames, target)| (frames.len(), *target)),
            control_values: self.control_values.clone(),
        };
        let _ = self.updates.send(WorkerUpdate::Status(Box::new(status)));
    }
}

/// Default FITS filename for a snapshot taken now: `prefix_YYYYMMDD_HHMMSS.fits`.
pub fn timestamped_name(prefix: &str, extension: &str) -> String {
    let now = crate::time_util::utc_from_system_time(SystemTime::now());
    format!(
        "{prefix}_{:04}{:02}{:02}_{:02}{:02}{:02}.{extension}",
        now.year, now.month, now.day, now.hour, now.minute, now.second
    )
}
