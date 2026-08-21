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
use firstlight_core::frame::{Frame, FrameMeta};
use firstlight_core::ring::FrameRing;
use firstlight_core::stack::RollingStack;

/// How long the renderer waits for a frame before looking at whether the
/// display settings changed.
const POLL: Duration = Duration::from_millis(30);

/// A rendered frame, with the metadata that produced it.
pub struct Rendered {
    pub image: DisplayImage,
    pub meta: FrameMeta,
    /// How many frames the picture was averaged from, and over how long.
    /// One frame and a zero span mean stacking is off or has just reset.
    pub stacked: usize,
    pub span: Duration,
}

struct Slot {
    latest: Mutex<Option<Rendered>>,
    options: Mutex<DisplayOptions>,
    /// How many frames to average for the live view. Held apart from
    /// `DisplayOptions` because stacking happens before rendering, not as
    /// part of it.
    stack_depth: Mutex<usize>,
    stop: AtomicBool,
    /// Cleared when the render loop leaves, including by unwinding, so the
    /// application can tell a stopped renderer from a quiet one.
    alive: AtomicBool,
    /// Asks the loop to forget the frame it is holding.
    forget: AtomicBool,
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
            stack_depth: Mutex::new(1),
            stop: AtomicBool::new(false),
            alive: AtomicBool::new(true),
            forget: AtomicBool::new(false),
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

    /// Average this many frames for the live view. One turns it off.
    pub fn set_stack_depth(&self, depth: usize) {
        *self
            .slot
            .stack_depth
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = depth;
    }

    /// Forget the last frame, so nothing repaints it.
    ///
    /// Without this the renderer still holds the frame and redraws it the
    /// moment any display setting changes, which would undo a clear a moment
    /// after the user asked for it.
    pub fn forget_frame(&self) {
        self.slot.forget.store(true, Ordering::SeqCst);
        *self.slot.latest.lock().unwrap_or_else(|e| e.into_inner()) = None;
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

/// Add a frame to the stack and return what should be displayed.
///
/// Only frames that describe themselves are stacked. While a control is being
/// adjusted the camera is still delivering frames exposed under the old
/// settings, and averaging those with the new ones would show a blend of both
/// — so the stack is left alone and the newest frame is shown as it is, which
/// keeps the view live exactly when somebody is watching it to judge a
/// change.
fn stack_frame(stack: &mut RollingStack, frame: Frame) -> Frame {
    if !frame.meta.settings_settled {
        return frame;
    }
    stack.push(frame)
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
    let mut stack = RollingStack::new(1);

    while !slot.stop.load(Ordering::SeqCst) {
        // Drain, stacking each frame as it arrives. Every frame that
        // reaches this thread contributes, even the ones the display would
        // otherwise skip past — they cost one add and one subtract each, and
        // throwing them away would waste signal that is already in hand.
        let mut fresh = false;
        while let Some(frame) = frames.try_recv() {
            last_frame = Some(stack_frame(&mut stack, frame));
            fresh = true;
        }
        if !fresh {
            if let Ok(frame) = frames.recv_timeout(POLL) {
                last_frame = Some(stack_frame(&mut stack, frame));
                fresh = true;
            }
        }

        // Changing the depth takes effect on the next frame; shrinking keeps
        // the newest frames rather than starting over.
        let depth = *slot.stack_depth.lock().unwrap_or_else(|e| e.into_inner());
        if depth != stack.depth() {
            stack.set_depth(depth);
        }

        if slot.forget.swap(false, Ordering::SeqCst) {
            last_frame = None;
            stack.clear();
            *slot.latest.lock().unwrap_or_else(|e| e.into_inner()) = None;
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
                    stacked: stack.len().max(1),
                    span: stack.span(),
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
