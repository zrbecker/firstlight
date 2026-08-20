//! Frame-to-texture conversion, on its own thread.
//!
//! Debayering and stretching a 1920x1080 frame costs single-digit
//! milliseconds optimised and over a hundred unoptimised. Either way it has no
//! business happening on the thread that draws the window: the whole premise
//! of this application is that nothing the camera does can make the UI stop
//! responding, and converting its frames counts as something the camera does.
//!
//! So a renderer thread owns the frame queue, produces finished RGBA images,
//! and leaves the newest one in a slot the UI picks up when it repaints. If
//! the UI is slow, the renderer overwrites the slot and the intermediate
//! images are simply never shown, which is the right thing for a live view.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use firstlight_core::display::{self, DisplayImage, DisplayOptions};
use firstlight_core::frame::FrameMeta;
use firstlight_core::ring::FrameRing;

/// How long the renderer waits for a frame before looking at whether the
/// display settings changed.
const POLL: Duration = Duration::from_millis(30);

/// A rendered frame, with the metadata that produced it.
pub struct Rendered {
    pub image: DisplayImage,
    pub meta: FrameMeta,
}

struct Slot {
    latest: Mutex<Option<Rendered>>,
    options: Mutex<DisplayOptions>,
    stop: AtomicBool,
    /// Cleared when the render loop leaves, including by unwinding, so the
    /// application can tell a stopped renderer from a quiet one.
    alive: AtomicBool,
    /// A panic the render loop survived, waiting to be reported.
    fault: Mutex<Option<String>>,
}

/// Handle to the renderer thread.
pub struct Renderer {
    slot: Arc<Slot>,
    thread: Option<JoinHandle<()>>,
}

impl Renderer {
    pub fn spawn(frames: Arc<FrameRing>, options: DisplayOptions) -> Renderer {
        let slot = Arc::new(Slot {
            latest: Mutex::new(None),
            options: Mutex::new(options),
            stop: AtomicBool::new(false),
            alive: AtomicBool::new(true),
            fault: Mutex::new(None),
        });
        let worker = slot.clone();
        let thread = thread::Builder::new()
            .name("firstlight-render".into())
            .spawn(move || run(worker, frames))
            .expect("spawning the render thread");
        Renderer {
            slot,
            thread: Some(thread),
        }
    }

    /// Change what the renderer produces. The last frame is re-rendered even
    /// if none arrive, so toggling auto-stretch on a paused camera still
    /// changes what is on screen.
    pub fn set_options(&self, options: DisplayOptions) {
        *self.slot.options.lock().unwrap_or_else(|e| e.into_inner()) = options;
    }

    /// Whether the render loop is still running.
    ///
    /// A renderer that has stopped leaves the last image on screen for ever,
    /// which looks exactly like a frozen camera and hides the cause. The
    /// application checks this and restarts it.
    pub fn is_alive(&self) -> bool {
        self.slot.alive.load(Ordering::SeqCst)
    }

    /// A panic the render loop caught and carried on from, if any.
    pub fn take_fault(&self) -> Option<String> {
        self.slot
            .fault
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// Stop the render loop, for tests that need to prove the application
    /// notices and recovers.
    pub fn stop_for_test(&mut self) {
        self.slot.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    /// Take the newest rendered frame, if there is one since the last call.
    pub fn take(&self) -> Option<Rendered> {
        self.slot
            .latest
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        self.slot.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Marks the renderer as stopped however the loop leaves — returning, or
/// unwinding from a panic.
struct AliveUntilDropped(Arc<Slot>);

impl Drop for AliveUntilDropped {
    fn drop(&mut self) {
        self.0.alive.store(false, Ordering::SeqCst);
    }
}

fn run(slot: Arc<Slot>, frames: Arc<FrameRing>) {
    let _alive = AliveUntilDropped(slot.clone());
    let mut last_frame = None;
    let mut last_options: Option<DisplayOptions> = None;

    while !slot.stop.load(Ordering::SeqCst) {
        // Drain: only the newest frame is worth rendering, and the ring is
        // one deep anyway.
        let mut fresh = false;
        while let Some(frame) = frames.try_recv() {
            last_frame = Some(frame);
            fresh = true;
        }
        if !fresh {
            if let Ok(frame) = frames.recv_timeout(POLL) {
                last_frame = Some(frame);
                fresh = true;
            }
        }

        let options = *slot.options.lock().unwrap_or_else(|e| e.into_inner());
        let options_changed = last_options != Some(options);
        last_options = Some(options);

        if !(fresh || options_changed) {
            continue;
        }
        let Some(frame) = &last_frame else {
            continue;
        };

        // A panic while converting one frame should cost that frame, not the
        // live view: dropping the thread here leaves a still picture on
        // screen with nothing to say why.
        let rendered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            display::render(frame, &options)
        }));
        match rendered {
            Ok(image) => {
                *slot.latest.lock().unwrap_or_else(|e| e.into_inner()) = Some(Rendered {
                    image,
                    meta: frame.meta.clone(),
                });
            }
            Err(payload) => {
                let message = payload
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic".to_string());
                *slot.fault.lock().unwrap_or_else(|e| e.into_inner()) = Some(message);
            }
        }
    }
}
