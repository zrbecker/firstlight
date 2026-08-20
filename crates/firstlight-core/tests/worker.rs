//! The worker is what stands between a misbehaving camera and a frozen UI,
//! so these tests are mostly about what happens when things go wrong.

use std::sync::Arc;
use std::time::{Duration, Instant};

use firstlight_core::simulator::{SimHandle, SimulatorBackend};
use firstlight_core::worker::{
    ConnectionState, RecordLimit, WorkerCommand, WorkerHandle, WorkerStatus, WorkerUpdate,
};
use firstlight_core::{Backend, BayerPattern, ControlId, PixelFormat, Registry, Roi};

const DEADLINE: Duration = Duration::from_secs(20);

/// A worker plus the full history of what it has told us.
///
/// Reading the update channel directly in each test turned out to be a trap:
/// one `recv` that discards a message another assertion needed makes tests
/// fail for reasons that have nothing to do with the code under test. So
/// everything is drained into a log, and assertions run over that.
struct Session {
    worker: WorkerHandle,
    sim: SimHandle,
    id: firstlight_core::CameraId,
    seen: Vec<WorkerUpdate>,
}

impl Session {
    fn new() -> Session {
        let backend = Arc::new(SimulatorBackend::single(
            64,
            48,
            PixelFormat::Bayer(BayerPattern::Rggb),
        ));
        let sim = backend.handle(0).unwrap();
        let id = backend.enumerate().unwrap()[0].id.clone();
        let registry = Registry::new().with(backend as Arc<dyn Backend>);
        Session {
            worker: WorkerHandle::spawn(registry),
            sim,
            id,
            seen: Vec::new(),
        }
    }

    fn send(&self, command: WorkerCommand) {
        self.worker.send(command).expect("worker is alive");
    }

    fn pump(&mut self) {
        while let Ok(update) = self.worker.updates().try_recv() {
            self.seen.push(update);
        }
    }

    /// Index to start looking from, so a wait cannot be satisfied by
    /// something that happened before it.
    fn mark(&mut self) -> usize {
        self.pump();
        self.seen.len()
    }

    fn wait<T>(&mut self, what: &str, mut find: impl FnMut(&[WorkerUpdate]) -> Option<T>) -> T {
        let deadline = Instant::now() + DEADLINE;
        loop {
            self.pump();
            if let Some(found) = find(&self.seen) {
                return found;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {what}; last status was {:#?}",
                    self.last_status()
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Wait for a status published after this call.
    fn wait_status(
        &mut self,
        what: &str,
        mut predicate: impl FnMut(&WorkerStatus) -> bool,
    ) -> WorkerStatus {
        let from = self.mark();
        self.wait(what, move |seen| {
            seen[from..].iter().find_map(|update| match update {
                WorkerUpdate::Status(status) if predicate(status) => Some((**status).clone()),
                _ => None,
            })
        })
    }

    /// Wait for any update in the whole history matching `predicate`.
    fn wait_update(
        &mut self,
        what: &str,
        from: usize,
        mut predicate: impl FnMut(&WorkerUpdate) -> bool,
    ) -> WorkerUpdate {
        self.wait(what, move |seen| {
            seen[from..].iter().find(|u| predicate(u)).cloned()
        })
    }

    /// Assert a property holds for every status published over a window.
    /// Used for the negative cases: proving something does *not* happen.
    fn assert_stays(
        &mut self,
        window: Duration,
        mut predicate: impl FnMut(&WorkerStatus) -> bool,
        message: &str,
    ) {
        let deadline = Instant::now() + window;
        let from = self.mark();
        while Instant::now() < deadline {
            self.pump();
            for update in &self.seen[from..] {
                if let WorkerUpdate::Status(status) = update {
                    assert!(predicate(status), "{message}: {:?}", status.state);
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn last_status(&self) -> Option<WorkerStatus> {
        self.seen.iter().rev().find_map(|update| match update {
            WorkerUpdate::Status(status) => Some((**status).clone()),
            _ => None,
        })
    }

    fn connect(&mut self) -> WorkerStatus {
        let id = self.id.clone();
        self.send(WorkerCommand::Connect(id));
        self.wait_status("connection", |s| s.state.is_connected())
    }
}

fn temp_path(name: &str) -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("firstlight-worker-{}-{name}", std::process::id()));
    path
}

#[test]
fn connect_stream_and_deliver_frames() {
    let mut session = Session::new();
    session.connect();

    session.send(WorkerCommand::StartStream);
    session.wait_status("frames", |s| s.frames_received > 2);

    let frame = (0..40)
        .find_map(|_| {
            std::thread::sleep(Duration::from_millis(50));
            session.worker.latest_frame()
        })
        .expect("a frame should reach the display path");
    assert_eq!((frame.width(), frame.height()), (64, 48));
}

#[test]
fn controls_and_geometry_round_trip_through_the_worker() {
    let mut session = Session::new();
    session.connect();

    let update = session.wait_update("control table", 0, |u| {
        matches!(u, WorkerUpdate::Controls(_))
    });
    let WorkerUpdate::Controls(controls) = update else {
        unreachable!()
    };
    assert!(controls.iter().any(|c| c.id == ControlId::ExposureUs));
    assert!(
        controls.iter().all(|c| c.min < c.max && c.step >= 1),
        "every control must advertise a usable range"
    );

    session.send(WorkerCommand::SetControl {
        id: ControlId::ExposureUs,
        value: 7_500,
    });
    session.send(WorkerCommand::SetRoi(Roi::new(0, 0, 32, 32)));
    let status = session.wait_status("settings", |s| {
        s.settings.exposure_us == 7_500 && s.settings.roi.width == 32
    });
    assert_eq!(status.settings.roi, Roi::new(0, 0, 32, 32));
}

#[test]
fn an_invalid_control_value_is_reported_and_does_not_kill_the_worker() {
    let mut session = Session::new();
    session.connect();

    let from = session.mark();
    session.send(WorkerCommand::SetControl {
        id: ControlId::Gain,
        value: 10_000_000,
    });
    let update = session.wait_update("failure report", from, |u| {
        matches!(u, WorkerUpdate::Failed { .. })
    });
    let WorkerUpdate::Failed {
        context,
        message,
        fatal,
    } = update
    else {
        unreachable!()
    };
    assert_eq!(context, "set control");
    assert!(!fatal, "a rejected value is not a device failure");
    assert!(message.contains("out of range"), "message was {message}");

    // Still alive afterwards.
    session.send(WorkerCommand::StartStream);
    session.wait_status("frames after a rejected control", |s| s.frames_received > 0);
}

#[test]
fn recording_writes_a_ser_file_and_reports_progress() {
    let mut session = Session::new();
    let path = temp_path("record.ser");
    session.connect();
    session.send(WorkerCommand::SetControl {
        id: ControlId::ExposureUs,
        value: 2_000,
    });
    let from = session.mark();
    session.send(WorkerCommand::StartRecording {
        path: path.clone(),
        limit: Some(RecordLimit::frames(5)),
    });

    let update = session.wait_update("saved recording", from, |u| {
        matches!(u, WorkerUpdate::Saved { .. })
    });
    let WorkerUpdate::Saved {
        path: saved_path,
        frames,
    } = update
    else {
        unreachable!()
    };
    assert_eq!(saved_path, path);
    assert_eq!(frames, 5, "the frame limit should stop the recording");

    let bytes = std::fs::read(&path).unwrap();
    // 178 byte header + 5 frames of 64x48x2 + 5 timestamps.
    assert_eq!(bytes.len(), 178 + 5 * 64 * 48 * 2 + 5 * 8);
    assert_eq!(&bytes[0..14], b"LUCAM-RECORDER");
    std::fs::remove_file(&path).ok();
}

#[test]
fn snapshot_writes_fits_without_a_stream_running() {
    let mut session = Session::new();
    let path = temp_path("snap.fits");
    session.connect();

    let from = session.mark();
    session.send(WorkerCommand::Snap { path: path.clone() });
    session.wait_update("saved snapshot", from, |u| {
        matches!(u, WorkerUpdate::Saved { .. })
    });

    let bytes = std::fs::read(&path).unwrap();
    assert!(bytes.starts_with(b"SIMPLE  ="));
    assert_eq!(bytes.len() % 2880, 0);

    // The stream was started only for the snapshot, so it must be stopped.
    session.wait_status("idle after snapshot", |s| !s.streaming);
    std::fs::remove_file(&path).ok();
}

#[test]
fn a_device_loss_is_surfaced_and_the_worker_reconnects_by_itself() {
    let mut session = Session::new();
    session.connect();
    session.send(WorkerCommand::StartStream);
    session.wait_status("frames", |s| s.frames_received > 1);

    session.sim.unplug();
    let lost = session.wait_status("device loss", |s| {
        matches!(
            s.state,
            ConnectionState::Lost { .. } | ConnectionState::Reconnecting { .. }
        )
    });
    assert!(!lost.streaming, "a lost device must not report streaming");

    // While unplugged the worker keeps retrying rather than giving up.
    session.wait_status(
        "reconnect attempts",
        |s| matches!(s.state, ConnectionState::Reconnecting { attempt, .. } if attempt >= 1),
    );

    session.sim.replug();
    let back = session.wait_status("reconnection", |s| s.state.is_connected());
    assert!(back.camera.is_some());
    // Streaming intent survives the round trip.
    session.wait_status("frames after reconnect", |s| {
        s.streaming && s.frames_received > 0
    });
}

#[test]
fn an_idle_camera_being_unplugged_is_noticed_too() {
    let mut session = Session::new();
    session.connect();
    // No stream running: nothing is polling the device, and the loss still
    // has to reach the UI.
    session.sim.unplug();
    session.wait_status("device loss while idle", |s| !s.state.is_connected());
}

#[test]
fn settings_are_restored_after_a_reconnect() {
    let mut session = Session::new();
    session.connect();
    session.send(WorkerCommand::SetControl {
        id: ControlId::ExposureUs,
        value: 3_300,
    });
    session.send(WorkerCommand::SetRoi(Roi::new(0, 0, 32, 16)));
    session.wait_status("settings applied", |s| {
        s.settings.exposure_us == 3_300 && s.settings.roi.width == 32
    });

    session.sim.unplug();
    session.wait_status("device loss", |s| !s.state.is_connected());
    session.sim.replug();

    let restored = session.wait_status("restored settings", |s| {
        s.state.is_connected() && s.settings.exposure_us == 3_300
    });
    assert_eq!(restored.settings.roi, Roi::new(0, 0, 32, 16));
}

#[test]
fn an_interrupted_recording_still_leaves_a_readable_file() {
    let mut session = Session::new();
    let path = temp_path("interrupted.ser");
    session.connect();
    session.send(WorkerCommand::SetControl {
        id: ControlId::ExposureUs,
        value: 2_000,
    });
    let from = session.mark();
    session.send(WorkerCommand::StartRecording {
        path: path.clone(),
        limit: None,
    });
    session.wait_status("recording progress", |s| {
        s.recording.as_ref().is_some_and(|r| r.frames >= 2)
    });

    session.sim.unplug();
    let update = session.wait_update("finalised recording", from, |u| {
        matches!(u, WorkerUpdate::Saved { .. })
    });
    let WorkerUpdate::Saved { frames, .. } = update else {
        unreachable!()
    };
    assert!(
        frames >= 2,
        "expected the frames captured before the unplug"
    );

    let bytes = std::fs::read(&path).unwrap();
    let count = i32::from_le_bytes(bytes[38..42].try_into().unwrap());
    assert_eq!(count as u64, frames, "SER frame count must be patched in");
    assert_eq!(bytes.len() as u64, 178 + frames * 64 * 48 * 2 + frames * 8);
    std::fs::remove_file(&path).ok();
}

#[test]
fn a_frozen_camera_is_reported_as_stalled_rather_than_hanging() {
    let mut session = Session::new();
    session.connect();
    session.send(WorkerCommand::StartStream);
    session.wait_status("frames", |s| s.frames_received > 1);

    session.sim.freeze_frames(true);
    let stalled = session.wait_status("stall report", |s| s.stalled);
    assert!(
        stalled.state.is_connected(),
        "a stall is not a disconnection"
    );

    session.sim.freeze_frames(false);
    session.wait_status("recovery", |s| !s.stalled);
}

#[test]
fn commands_are_still_answered_while_the_camera_is_gone() {
    let mut session = Session::new();
    session.connect();
    session.sim.unplug();
    session.wait_status("device loss", |s| !s.state.is_connected());

    // The GUI keeps issuing commands after a loss; none of them may block or
    // panic, and the failures must come back as reports.
    let from = session.mark();
    session.send(WorkerCommand::StartStream);
    session.send(WorkerCommand::SetControl {
        id: ControlId::Gain,
        value: 200,
    });
    session.send(WorkerCommand::RefreshCameras);

    let update = session.wait_update("enumeration", from, |u| {
        matches!(u, WorkerUpdate::Cameras { .. })
    });
    let WorkerUpdate::Cameras { cameras, .. } = update else {
        unreachable!()
    };
    assert!(cameras.is_empty(), "an unplugged camera must not enumerate");

    session.sim.replug();
    session.wait_status("reconnection", |s| s.state.is_connected());
}

#[test]
fn a_slow_consumer_loses_display_frames_but_not_recorded_ones() {
    let mut session = Session::new();
    let path = temp_path("slow-consumer.ser");
    session.connect();
    session.send(WorkerCommand::SetControl {
        id: ControlId::ExposureUs,
        value: 1_000,
    });
    let from = session.mark();
    session.send(WorkerCommand::StartRecording {
        path: path.clone(),
        limit: Some(RecordLimit::frames(12)),
    });

    // Never call `latest_frame`: the display queue is one deep, so most
    // frames must be dropped there while every one still reaches the file.
    let update = session.wait_update("saved recording", from, |u| {
        matches!(u, WorkerUpdate::Saved { .. })
    });
    let WorkerUpdate::Saved { frames, .. } = update else {
        unreachable!()
    };
    assert_eq!(frames, 12);
    assert!(
        session.worker.display_dropped() > 0,
        "the display path should have dropped frames"
    );
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(bytes.len(), 178 + 12 * 64 * 48 * 2 + 12 * 8);
    std::fs::remove_file(&path).ok();
}

#[test]
fn dropping_the_handle_stops_the_worker() {
    let mut session = Session::new();
    session.connect();
    session.send(WorkerCommand::StartStream);
    session.wait_status("frames", |s| s.frames_received > 0);

    let updates = session.worker.updates().clone();
    drop(session);
    // The channel closes only once the thread has actually exited.
    let deadline = Instant::now() + DEADLINE;
    let mut stopped = false;
    while Instant::now() < deadline {
        match updates.recv_timeout(Duration::from_millis(200)) {
            Ok(WorkerUpdate::Stopped) => {
                stopped = true;
                break;
            }
            Ok(_) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                stopped = true;
                break;
            }
            Err(_) => {}
        }
    }
    assert!(stopped, "the worker thread should have exited");
}

#[test]
fn a_deliberate_disconnect_does_not_reconnect_behind_your_back() {
    let mut session = Session::new();
    session.connect();
    session.send(WorkerCommand::Disconnect);
    session.wait_status("the disconnect", |s| {
        matches!(s.state, ConnectionState::Disconnected)
    });

    // The camera is still attached, so only the reconnect logic could bring
    // it back — and it must not.
    session.assert_stays(
        Duration::from_secs(3),
        |s| matches!(s.state, ConnectionState::Disconnected),
        "a camera disconnected on purpose came back by itself",
    );
}

#[test]
fn auto_reconnect_can_be_turned_off_and_back_on() {
    let mut session = Session::new();
    session.connect();
    session.send(WorkerCommand::SetAutoReconnect(false));
    session.sim.unplug();
    session.wait_status("the device loss", |s| !s.state.is_connected());

    // Plugged back in, but the user asked us not to chase it.
    session.sim.replug();
    session.assert_stays(
        Duration::from_secs(3),
        |s| !s.state.is_connected(),
        "reconnected despite auto-reconnect being off",
    );

    // Turning it back on picks the camera up without another unplug.
    session.send(WorkerCommand::SetAutoReconnect(true));
    session.wait_status("the reconnection", |s| s.state.is_connected());
}

#[test]
fn the_status_carries_what_the_camera_says_every_control_is_set_to() {
    // The bug this guards: only exposure, gain and offset were ever read
    // back, so a UI showed defaults for everything else — including white
    // balance, which a camera remembers between sessions and which another
    // application may well have left somewhere surprising.
    let mut session = Session::new();
    session.connect();

    let status = session.wait_status("control values", |s| !s.control_values.is_empty());
    assert!(
        status.control_values.contains_key(&ControlId::WbRed),
        "white balance is missing from {:?}",
        status.control_values.keys().collect::<Vec<_>>()
    );

    // Change one without going near the exposure/gain/offset special cases.
    session.send(WorkerCommand::SetControl {
        id: ControlId::WbRed,
        value: 250,
    });
    let status = session.wait_status("the new white balance", |s| {
        s.control_values.get(&ControlId::WbRed) == Some(&250)
    });
    // Everything else keeps its own value rather than being reset.
    assert_eq!(status.control_values.get(&ControlId::WbBlue), Some(&100));
}

#[test]
fn read_only_controls_are_reported_but_refused() {
    let mut session = Session::new();
    session.connect();
    let temperature = ControlId::Vendor(16);

    session.wait_status("the read-only control", |s| {
        s.control_values.contains_key(&temperature)
    });

    let from = session.mark();
    session.send(WorkerCommand::SetControl {
        id: temperature,
        value: 0,
    });
    let update = session.wait_update("the refusal", from, |u| {
        matches!(u, WorkerUpdate::Failed { .. })
    });
    let WorkerUpdate::Failed { message, fatal, .. } = update else {
        unreachable!()
    };
    assert!(message.contains("read-only"), "{message}");
    assert!(!fatal);
}
