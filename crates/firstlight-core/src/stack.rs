//! Averaging the last few frames, to quieten a live view.
//!
//! Noise falls as the square root of the number of frames averaged, so a
//! stack of four is twice as clean as a single frame. That is the difference
//! between guessing at a faint smudge and seeing it.
//!
//! This is a *rolling* mean rather than a cumulative one: it averages the
//! last `k` frames and nothing older. A cumulative stack goes deeper the
//! longer it runs, which is what you want for a finished image, but it never
//! forgets — a passing cloud or a knock to the mount stays in the result for
//! the rest of the session. A rolling stack recovers within `k` frames, which
//! is the right trade for something you are watching in real time.
//!
//! It is display-only, like everything else under [`crate::display`]. What
//! gets recorded is always individual frames; stacking for a final image is a
//! job for software that also aligns, rejects outliers and calibrates.
//!
//! # What it does not do
//!
//! **It does not align.** Frames are averaged where they lie, so anything
//! that moves between them smears rather than stacks. Over a short window on
//! a tracked mount, or on a wide field where drift covers a fraction of a
//! pixel, that is invisible. Push `k` high enough, or the field narrow
//! enough, and it will blur — which is why the frame count and the time span
//! belong on screen where the trade is visible.

use std::collections::VecDeque;
use std::time::{Duration, SystemTime};

use crate::frame::{Frame, FrameMeta};

/// The most frames a stack will hold.
///
/// Not a memory limit — a hundred frames of a large sensor is a few hundred
/// megabytes, which is survivable — but a smearing limit. Beyond this the
/// window covers so much time that an unaligned average is showing you drift
/// rather than signal.
pub const MAX_DEPTH: usize = 64;

/// How the frames in the window are combined into one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Combine {
    /// The arithmetic mean. Cheap — one add and one subtract per pixel as
    /// the window slides — and the best estimator when nothing goes wrong.
    /// It keeps outliers, though: a single sample of 60000 ADU still lifts a
    /// 64-frame mean by nearly a thousand, and it stays there for the whole
    /// window.
    #[default]
    Mean,
    /// The per-pixel median. Discards outliers outright rather than diluting
    /// them, at the cost of re-reading the whole window for every displayed
    /// frame. Measured on covered frames at gain 450 with the dark
    /// subtracted, it left a quarter as many bright specks as the mean.
    Median,
}

impl Combine {
    pub fn label(&self) -> &'static str {
        match self {
            Combine::Mean => "mean",
            Combine::Median => "median",
        }
    }
}

/// A rolling combination of the last `depth` frames.
///
/// Keeps a running sum so a mean costs one add and one subtract per pixel
/// rather than a full re-average, which matters at a megapixel and
/// twenty-five frames a second. The sum is maintained even when the median
/// is selected, so switching between them shows the next frame rather than
/// starting over.
#[derive(Debug)]
pub struct RollingStack {
    depth: usize,
    combine: Combine,
    /// The frames currently in the window, oldest first.
    frames: VecDeque<Frame>,
    /// Sum of every sample in `frames`, widened so it cannot overflow: 64
    /// frames of 16-bit samples needs 22 bits.
    sum: Vec<u32>,
    /// Shape the sum was built for. A frame of any other shape starts over.
    shape: Option<Shape>,
}

/// What must match for frames to be averaged together at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Shape {
    width: u32,
    height: u32,
    bytes_per_sample: usize,
    samples_per_pixel: usize,
}

impl Shape {
    fn of(meta: &FrameMeta) -> Shape {
        Shape {
            width: meta.width,
            height: meta.height,
            bytes_per_sample: meta.bytes_per_sample(),
            samples_per_pixel: meta.format.samples_per_pixel(),
        }
    }

    fn samples(&self) -> usize {
        self.width as usize * self.height as usize * self.samples_per_pixel
    }
}

impl RollingStack {
    /// A stack averaging the last `depth` frames. A depth of one is a
    /// pass-through, which is the honest way to express "stacking off".
    pub fn new(depth: usize) -> RollingStack {
        RollingStack {
            depth: depth.clamp(1, MAX_DEPTH),
            combine: Combine::default(),
            frames: VecDeque::new(),
            sum: Vec::new(),
            shape: None,
        }
    }

    pub fn depth(&self) -> usize {
        self.depth
    }

    pub fn combine(&self) -> Combine {
        self.combine
    }

    /// Change how the window is combined. Takes effect on the next frame;
    /// the frames already held are kept, so the picture does not blank.
    pub fn set_combine(&mut self, combine: Combine) {
        self.combine = combine;
    }

    /// Change how many frames are averaged.
    ///
    /// Shrinking drops the oldest frames rather than starting over, so the
    /// picture stays live while the control is being dragged.
    pub fn set_depth(&mut self, depth: usize) {
        self.depth = depth.clamp(1, MAX_DEPTH);
        while self.frames.len() > self.depth {
            self.remove_oldest();
        }
    }

    /// How many frames the current result is averaged from.
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Total exposure the stack represents: what a single sub of this length
    /// would have collected, ignoring read noise.
    pub fn integration(&self) -> Duration {
        self.frames
            .iter()
            .map(|frame| Duration::from_micros(frame.meta.exposure_us))
            .sum()
    }

    /// Wall-clock time from the start of the oldest frame to the newest.
    ///
    /// Larger than [`RollingStack::integration`] whenever frames were skipped,
    /// and the number that says how much drift the stack has had a chance to
    /// smear over.
    pub fn span(&self) -> Duration {
        match (self.frames.front(), self.frames.back()) {
            (Some(first), Some(last)) => {
                last.meta
                    .timestamp
                    .duration_since(first.meta.timestamp)
                    .unwrap_or(Duration::ZERO)
                    + Duration::from_micros(first.meta.exposure_us)
            }
            _ => Duration::ZERO,
        }
    }

    /// Throw away everything. Call this when the frames stop being
    /// comparable for a reason the stack cannot see, such as the stream
    /// stopping.
    pub fn clear(&mut self) {
        self.frames.clear();
        self.sum.clear();
        self.shape = None;
    }

    /// Add a frame to the window.
    ///
    /// Deliberately does not combine: [`RollingStack::result`] does that, and
    /// keeping them apart is what makes an expensive combine affordable. A
    /// live view renders far less often than frames arrive — when the median
    /// takes longer than the frame interval, several frames land between
    /// repaints — and combining on every arrival meant computing results
    /// nobody would ever see, while the ones that mattered fell further
    /// behind.
    ///
    /// A frame of a different shape — a new ROI, binning or bit depth —
    /// starts the stack over, because frames of different sizes cannot be
    /// combined at all.
    pub fn push(&mut self, frame: Frame) {
        let shape = Shape::of(&frame.meta);
        if self.shape != Some(shape) {
            self.clear();
            self.shape = Some(shape);
            self.sum = vec![0u32; shape.samples()];
        }

        while self.frames.len() >= self.depth {
            self.remove_oldest();
        }
        self.add(&frame);
        self.frames.push_back(frame);
    }

    /// Combine the window into one frame, or `None` if nothing is in it.
    pub fn result(&self) -> Option<Frame> {
        if self.frames.is_empty() {
            return None;
        }
        Some(match self.combine {
            Combine::Mean => self.average(),
            Combine::Median => self.median(),
        })
    }

    /// The average of the window, as a frame.
    ///
    /// Its metadata comes from the newest frame — the settings every frame in
    /// the window shares — except that the timestamp and exposure describe
    /// the stack: when it started and how much light it represents.
    fn average(&self) -> Frame {
        let newest = self
            .frames
            .back()
            .expect("push always leaves at least one frame");
        let count = self.frames.len() as u32;
        if count == 1 {
            return newest.clone();
        }

        let shape = self.shape.expect("a shape is set alongside the sum");
        let mut data = vec![0u8; shape.samples() * shape.bytes_per_sample];
        if shape.bytes_per_sample == 1 {
            for (out, total) in data.iter_mut().zip(&self.sum) {
                *out = (total / count) as u8;
            }
        } else {
            for (out, total) in data.chunks_exact_mut(2).zip(&self.sum) {
                out.copy_from_slice(&((total / count) as u16).to_le_bytes());
            }
        }

        Frame::new(self.stack_meta(newest), data)
            .expect("the average has the shape it was summed from")
    }

    /// The per-pixel median of the window.
    ///
    /// Done in chunks of samples rather than a pixel at a time: gathering
    /// one pixel from sixty-four separate frame buffers touches sixty-four
    /// pages for four bytes of use, and at two million pixels that is most
    /// of the cost. A chunk is gathered from each frame in turn, so every
    /// buffer is read forwards, then the medians are taken within it.
    fn median(&self) -> Frame {
        let newest = self
            .frames
            .back()
            .expect("push always leaves at least one frame");
        let count = self.frames.len();
        if count == 1 {
            return newest.clone();
        }
        let shape = self.shape.expect("a shape is set alongside the sum");
        let samples = shape.samples();
        let mut data = vec![0u8; samples * shape.bytes_per_sample];

        // Sample-major within a chunk: `window[i * count + f]` is frame `f`'s
        // value for the chunk's `i`th sample.
        const CHUNK: usize = 4096;
        let mut window = vec![0u16; CHUNK * count];

        for start in (0..samples).step_by(CHUNK) {
            let len = CHUNK.min(samples - start);
            for (f, frame) in self.frames.iter().enumerate() {
                let bytes = &frame.data;
                if shape.bytes_per_sample == 1 {
                    for (i, byte) in bytes[start..start + len].iter().enumerate() {
                        window[i * count + f] = u16::from(*byte);
                    }
                } else {
                    let region = &bytes[start * 2..(start + len) * 2];
                    for (i, pair) in region.chunks_exact(2).enumerate() {
                        window[i * count + f] = u16::from_le_bytes([pair[0], pair[1]]);
                    }
                }
            }
            let middle = count / 2;
            for i in 0..len {
                let lane = &mut window[i * count..i * count + count];
                let (_, median, _) = lane.select_nth_unstable(middle);
                let value = *median;
                if shape.bytes_per_sample == 1 {
                    data[start + i] = value as u8;
                } else {
                    let at = (start + i) * 2;
                    data[at..at + 2].copy_from_slice(&value.to_le_bytes());
                }
            }
        }

        Frame::new(self.stack_meta(newest), data)
            .expect("the median has the shape it was gathered from")
    }

    /// Metadata for a combined frame: the settings every frame in the window
    /// shares, but a timestamp and exposure that describe the stack.
    fn stack_meta(&self, newest: &Frame) -> FrameMeta {
        let mut meta = newest.meta.clone();
        meta.timestamp = self
            .frames
            .front()
            .map(|first| first.meta.timestamp)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        meta.exposure_us = self.integration().as_micros().min(u64::MAX as u128) as u64;
        meta
    }

    fn add(&mut self, frame: &Frame) {
        for_each_sample(frame, |index, value| {
            self.sum[index] = self.sum[index].saturating_add(u32::from(value));
        });
    }

    fn remove_oldest(&mut self) {
        let Some(frame) = self.frames.pop_front() else {
            return;
        };
        for_each_sample(&frame, |index, value| {
            self.sum[index] = self.sum[index].saturating_sub(u32::from(value));
        });
    }
}

/// Walk a frame's samples, widened to u16.
///
/// Written once and shared by the add and subtract paths so the two cannot
/// disagree about the layout, which would corrupt the running sum in a way
/// that only shows up as a slowly drifting image.
fn for_each_sample(frame: &Frame, mut visit: impl FnMut(usize, u16)) {
    let data = &frame.data;
    if frame.meta.bytes_per_sample() == 1 {
        for (index, byte) in data.iter().enumerate() {
            visit(index, u16::from(*byte));
        }
    } else {
        for (index, pair) in data.chunks_exact(2).enumerate() {
            visit(index, u16::from_le_bytes([pair[0], pair[1]]));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{Binning, BitDepth, Roi};
    use crate::frame::PixelFormat;

    /// Push a frame and combine, which is what a caller that wants the
    /// current picture does.
    fn combined(stack: &mut RollingStack, frame: Frame) -> Frame {
        stack.push(frame);
        stack.result().expect("a frame was just pushed")
    }

    #[test]
    fn a_median_throws_an_outlier_away_where_a_mean_dilutes_it() {
        // The reason for offering it: one bright sample in a window of nine
        // still lifts a mean, and stays there until it slides out. A median
        // does not see it at all.
        let build = |combine: Combine| {
            let mut stack = RollingStack::new(9);
            stack.set_combine(combine);
            for _ in 0..8 {
                stack.push(frame(1000, BitDepth::SIXTEEN, (4, 4)));
            }
            stack.push(frame(60_000, BitDepth::SIXTEEN, (4, 4)));
            first_sample(&stack.result().unwrap())
        };
        let mean = build(Combine::Mean);
        let median = build(Combine::Median);
        assert!(mean > 6_000, "a mean should carry the outlier, got {mean}");
        assert_eq!(median, 1000, "a median should ignore it");
    }

    #[test]
    fn switching_how_frames_combine_keeps_the_frames() {
        // Changing the control mid-session must not blank the live view or
        // start the window over.
        let mut stack = RollingStack::new(5);
        for _ in 0..5 {
            stack.push(frame(800, BitDepth::SIXTEEN, (4, 4)));
        }
        assert_eq!(stack.len(), 5);
        stack.set_combine(Combine::Median);
        assert_eq!(stack.len(), 5, "the window survives the switch");
        assert_eq!(first_sample(&stack.result().unwrap()), 800);
        stack.set_combine(Combine::Mean);
        assert_eq!(first_sample(&stack.result().unwrap()), 800);
    }

    #[test]
    fn pushing_does_not_combine() {
        // What makes an expensive median affordable: frames can be taken in
        // faster than results are asked for, and the cost is paid per
        // result, not per frame.
        let mut stack = RollingStack::new(4);
        assert!(stack.result().is_none(), "nothing in it yet");
        stack.push(frame(100, BitDepth::SIXTEEN, (4, 4)));
        // Asking twice without pushing gives the same answer both times.
        let once = first_sample(&stack.result().unwrap());
        let twice = first_sample(&stack.result().unwrap());
        assert_eq!(once, twice);
    }

    fn frame(value: u16, depth: BitDepth, size: (u32, u32)) -> Frame {
        let (width, height) = size;
        let meta = FrameMeta {
            sequence: 0,
            timestamp: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            width,
            height,
            format: PixelFormat::Mono,
            bit_depth: depth,
            exposure_us: 1_000_000,
            gain: 100,
            offset: 0,
            binning: Binning::ONE,
            roi: Roi::full(width, height),
            dropped: 0,
            temperature_c: None,
            settings_settled: true,
        };
        let samples = (width * height) as usize;
        let data = if depth.bytes_per_sample() == 1 {
            vec![value as u8; samples]
        } else {
            value.to_le_bytes().repeat(samples)
        };
        Frame::new(meta, data).unwrap()
    }

    fn first_sample(frame: &Frame) -> u16 {
        frame.sample(0, 0, 0).unwrap()
    }

    #[test]
    fn a_depth_of_one_passes_frames_straight_through() {
        let mut stack = RollingStack::new(1);
        assert_eq!(
            first_sample(&combined(&mut stack, frame(100, BitDepth::SIXTEEN, (4, 4)))),
            100
        );
        assert_eq!(
            first_sample(&combined(&mut stack, frame(200, BitDepth::SIXTEEN, (4, 4)))),
            200
        );
        assert_eq!(stack.len(), 1);
    }

    #[test]
    fn the_window_rolls_forward_and_forgets() {
        // 1,2,3 then 2,3,4 then 3,4,5 — the average follows the window rather
        // than dragging the whole history along.
        let mut stack = RollingStack::new(3);
        let values = [300u16, 600, 900, 1200, 1500];
        let mut averages = Vec::new();
        for value in values {
            averages.push(first_sample(&combined(
                &mut stack,
                frame(value, BitDepth::SIXTEEN, (4, 4)),
            )));
        }
        assert_eq!(
            averages,
            vec![
                300,  // 300
                450,  // (300+600)/2
                600,  // (300+600+900)/3
                900,  // (600+900+1200)/3
                1200, // (900+1200+1500)/3
            ]
        );
        assert_eq!(stack.len(), 3, "the window never grows past its depth");
    }

    #[test]
    fn averaging_actually_reduces_noise() {
        // The whole point, checked rather than assumed: sixteen noisy frames
        // of a constant scene should average much closer to the truth than
        // any one of them.
        const TRUTH: i32 = 8_000;
        let mut stack = RollingStack::new(16);
        let mut seed = 0x1234_5678u32;
        let mut single_error = 0i64;
        let mut stacked = 0u16;
        for _ in 0..16 {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let noise = ((seed >> 16) % 2_000) as i32 - 1_000;
            let value = (TRUTH + noise) as u16;
            single_error += i64::from(noise).abs();
            stacked = first_sample(&combined(
                &mut stack,
                frame(value, BitDepth::SIXTEEN, (8, 8)),
            ));
        }
        let mean_single_error = single_error / 16;
        let stacked_error = (i32::from(stacked) - TRUTH).unsigned_abs() as i64;
        assert!(
            stacked_error * 3 < mean_single_error,
            "stacking 16 frames should be much closer to the truth: \
             stacked off by {stacked_error}, a single frame by {mean_single_error} on average"
        );
    }

    #[test]
    fn a_change_of_shape_starts_the_stack_over() {
        let mut stack = RollingStack::new(4);
        combined(&mut stack, frame(1000, BitDepth::SIXTEEN, (8, 8)));
        combined(&mut stack, frame(1000, BitDepth::SIXTEEN, (8, 8)));
        assert_eq!(stack.len(), 2);

        // A new ROI: the frames are not the same size and cannot be averaged.
        let smaller = combined(&mut stack, frame(500, BitDepth::SIXTEEN, (4, 4)));
        assert_eq!(stack.len(), 1, "the old frames must be discarded");
        assert_eq!(first_sample(&smaller), 500);
        assert_eq!((smaller.width(), smaller.height()), (4, 4));

        // And a change of bit depth, which changes the layout rather than the
        // size, is just as incompatible.
        let shallower = combined(&mut stack, frame(200, BitDepth::EIGHT, (4, 4)));
        assert_eq!(stack.len(), 1);
        assert_eq!(first_sample(&shallower), 200);
    }

    #[test]
    fn eight_bit_frames_stack_too() {
        let mut stack = RollingStack::new(2);
        combined(&mut stack, frame(100, BitDepth::EIGHT, (4, 4)));
        let averaged = combined(&mut stack, frame(200, BitDepth::EIGHT, (4, 4)));
        assert_eq!(first_sample(&averaged), 150);
        assert_eq!(averaged.data.len(), 16, "still one byte per sample");
    }

    #[test]
    fn shrinking_the_depth_keeps_the_newest_frames() {
        let mut stack = RollingStack::new(4);
        for value in [400u16, 800, 1200, 1600] {
            combined(&mut stack, frame(value, BitDepth::SIXTEEN, (4, 4)));
        }
        stack.set_depth(2);
        assert_eq!(stack.len(), 2);
        // The two newest, so the picture does not lurch backwards in time.
        let averaged = combined(&mut stack, frame(2000, BitDepth::SIXTEEN, (4, 4)));
        assert_eq!(first_sample(&averaged), 1800, "(1600+2000)/2");
    }

    #[test]
    fn the_stack_reports_what_it_represents() {
        let mut stack = RollingStack::new(3);
        for _ in 0..3 {
            combined(&mut stack, frame(100, BitDepth::SIXTEEN, (4, 4)));
        }
        // Three one-second frames.
        assert_eq!(stack.integration(), Duration::from_secs(3));
        assert_eq!(stack.len(), 3);
        // The averaged frame's exposure describes the stack, not one frame,
        // so anything reading it downstream is told the truth.
        let averaged = combined(&mut stack, frame(100, BitDepth::SIXTEEN, (4, 4)));
        assert_eq!(averaged.meta.exposure_us, 3_000_000);
    }

    #[test]
    fn depth_is_clamped_to_something_sane() {
        assert_eq!(RollingStack::new(0).depth(), 1);
        assert_eq!(RollingStack::new(10_000).depth(), MAX_DEPTH);
    }
}
