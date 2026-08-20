//! A bounded frame queue with drop-oldest semantics.
//!
//! Every backend has the same problem: the SDK delivers frames on its own
//! thread and the consumer may be slower. Blocking the producer is not an
//! option (in a vendor callback it stalls the USB pipe), and an unbounded
//! queue turns a slow consumer into an out-of-memory crash. So the producer
//! never waits: when the ring is full the *oldest* frame is discarded, which
//! keeps the live view current, and the loss is counted rather than hidden.
//!
//! The ring also carries the reason a stream ended, so a consumer blocked in
//! [`FrameRing::recv_timeout`] finds out about a device loss immediately
//! instead of waiting out its whole timeout.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::frame::Frame;

/// Why a stream stopped producing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamStop {
    /// Normal `stop_streaming`.
    Stopped,
    DeviceLost(String),
    UsbStall(String),
    Failed(String),
}

impl StreamStop {
    pub fn to_error(&self) -> Error {
        match self {
            StreamStop::Stopped => Error::NotStreaming,
            StreamStop::DeviceLost(r) => Error::DeviceLost(r.clone()),
            StreamStop::UsbStall(r) => Error::UsbStall(r.clone()),
            StreamStop::Failed(r) => Error::Other(r.clone()),
        }
    }

    pub fn is_fatal(&self) -> bool {
        !matches!(self, StreamStop::Stopped)
    }
}

struct Inner {
    queue: VecDeque<Frame>,
    capacity: usize,
    /// Cumulative, since the last [`FrameRing::reset`].
    dropped: u64,
    stop: Option<StreamStop>,
}

pub struct FrameRing {
    inner: Mutex<Inner>,
    ready: Condvar,
}

impl FrameRing {
    /// `capacity` is in frames and must be at least 1. Two or three is right
    /// for a live view: enough to absorb a scheduling hiccup, not enough to
    /// build up visible latency.
    pub fn new(capacity: usize) -> FrameRing {
        FrameRing {
            inner: Mutex::new(Inner {
                queue: VecDeque::with_capacity(capacity.max(1)),
                capacity: capacity.max(1),
                dropped: 0,
                stop: None,
            }),
            ready: Condvar::new(),
        }
    }

    /// Push a frame, evicting the oldest if the ring is full. Never blocks.
    /// Returns the cumulative dropped count so the caller can raise an event
    /// when it changes.
    pub fn push(&self, frame: Frame) -> u64 {
        let mut inner = self.lock();
        if inner.stop.is_some() {
            // Stream already ended; the frame is stale by definition.
            return inner.dropped;
        }
        while inner.queue.len() >= inner.capacity {
            inner.queue.pop_front();
            inner.dropped += 1;
        }
        inner.queue.push_back(frame);
        let dropped = inner.dropped;
        drop(inner);
        self.ready.notify_one();
        dropped
    }

    /// Wait up to `timeout` for a frame.
    ///
    /// Returns [`Error::Timeout`] if none arrived, or the stream's terminal
    /// error as soon as the producer reports one — a lost device wakes this
    /// call immediately rather than letting it run out the clock.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Frame> {
        let deadline = Instant::now() + timeout;
        let mut inner = self.lock();
        loop {
            if let Some(frame) = inner.queue.pop_front() {
                return Ok(frame);
            }
            if let Some(stop) = &inner.stop {
                return Err(stop.to_error());
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(Error::Timeout(timeout));
            }
            let (guard, _) = self
                .ready
                .wait_timeout(inner, deadline - now)
                .unwrap_or_else(|e| e.into_inner());
            inner = guard;
        }
    }

    /// Take a frame if one is queued, without waiting.
    pub fn try_recv(&self) -> Option<Frame> {
        self.lock().queue.pop_front()
    }

    /// Mark the stream as finished. Waiters wake at once. The first reason
    /// wins, so a device loss is not overwritten by the tidy-up `Stopped`
    /// that follows it.
    pub fn stop(&self, reason: StreamStop) {
        let mut inner = self.lock();
        if inner.stop.is_none() {
            inner.stop = Some(reason);
        }
        drop(inner);
        self.ready.notify_all();
    }

    pub fn stop_reason(&self) -> Option<StreamStop> {
        self.lock().stop.clone()
    }

    /// Clear queued frames, the drop count and the stop reason, ready for a
    /// new stream on the same ring.
    pub fn reset(&self) {
        let mut inner = self.lock();
        inner.queue.clear();
        inner.dropped = 0;
        inner.stop = None;
    }

    pub fn dropped(&self) -> u64 {
        self.lock().dropped
    }

    pub fn len(&self) -> usize {
        self.lock().queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A poisoned ring mutex means a producer thread panicked mid-push. The
    /// queue itself is still structurally sound, and losing the live view
    /// because of one bad frame helps nobody, so recover and carry on.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl std::fmt::Debug for FrameRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.lock();
        f.debug_struct("FrameRing")
            .field("queued", &inner.queue.len())
            .field("capacity", &inner.capacity)
            .field("dropped", &inner.dropped)
            .field("stop", &inner.stop)
            .finish()
    }
}
