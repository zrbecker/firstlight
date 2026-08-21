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

use firstlight_core::dark::MasterDark;
use firstlight_core::display::{self, DisplayImage, DisplayOptions};
use firstlight_core::frame::FrameMeta;
use firstlight_core::ring::FrameRing;
use firstlight_core::stack::{Combine, RollingStack};

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
    /// How those frames are combined.
    combine: Mutex<Combine>,
    /// The master dark to subtract, if one has been taken and the settings
    /// still match it.
    dark: Mutex<Option<Arc<MasterDark>>>,
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
            combine: Mutex::new(Combine::default()),
            dark: Mutex::new(None),
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

    /// Combine the stacked frames this way. Takes effect on the next
    /// repaint, keeping the frames already gathered.
    pub fn set_combine(&self, combine: Combine) {
        *self.slot.combine.lock().unwrap_or_else(|e| e.into_inner()) = combine;
    }

    /// Subtract this master dark from the preview, or `None` to stop.
    pub fn set_dark(&self, dark: Option<Arc<MasterDark>>) {
        *self.slot.dark.lock().unwrap_or_else(|e| e.into_inner()) = dark;
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
        // Drain, adding each frame to the window as it arrives. Every frame
        // that reaches this thread contributes, even the ones the display
        // would otherwise skip past — throwing them away would waste signal
        // that is already in hand.
        //
        // Combining is deliberately not done here. A median over a deep
        // window costs hundreds of milliseconds, so several frames land
        // between repaints; combining on arrival would compute results
        // nobody sees while the one that matters falls further behind.
        //
        // Only frames that describe themselves join the window. While a
        // control is being adjusted the camera is still delivering frames
        // exposed under the old settings, and combining those with the new
        // ones would show a blend of both — so such a frame is shown on its
        // own instead, which keeps the view live exactly when somebody is
        // watching it to judge a change.
        let mut window_changed = false;
        let mut unsettled = None;
        let mut fresh = false;
        loop {
            let frame = match frames.try_recv() {
                Some(frame) => frame,
                // Nothing waiting: block briefly the first time round so an
                // idle renderer is not a spin loop.
                None if fresh => break,
                None => match frames.recv_timeout(POLL) {
                    Ok(frame) => frame,
                    Err(_) => break,
                },
            };
            fresh = true;
            if frame.meta.settings_settled {
                stack.push(frame);
                window_changed = true;
                unsettled = None;
            } else {
                unsettled = Some(frame);
            }
        }

        // Changing the depth takes effect on the next frame; shrinking keeps
        // the newest frames rather than starting over.
        let depth = *slot.stack_depth.lock().unwrap_or_else(|e| e.into_inner());
        if depth != stack.depth() {
            stack.set_depth(depth);
            window_changed = true;
        }
        let combine = *slot.combine.lock().unwrap_or_else(|e| e.into_inner());
        if combine != stack.combine() {
            stack.set_combine(combine);
            window_changed = true;
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
        // Combine once, and only when the window it draws from has moved.
        // A display setting changing does not need the frames combined
        // again — the result would be identical.
        if let Some(frame) = unsettled {
            last_frame = Some(frame);
        } else if window_changed {
            last_frame = stack.result().or(last_frame.take());
        }
        let Some(frame) = &last_frame else {
            continue;
        };

        // Calibrate before rendering. Subtracting from the stacked result
        // rather than from each frame is the same arithmetic — both are
        // linear — and costs one subtraction per displayed frame instead of
        // one per captured frame.
        let dark = slot.dark.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let calibrated = match &dark {
            Some(dark) => std::borrow::Cow::Owned(dark.apply(frame)),
            None => std::borrow::Cow::Borrowed(frame),
        };
        let frame = calibrated.as_ref();

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
