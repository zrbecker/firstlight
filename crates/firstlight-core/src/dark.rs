//! A master dark, and subtracting it from the live view.
//!
//! Every sensor has a pattern it produces with no light at all: a black level
//! pedestal, pixels that leak more charge than their neighbours, and the
//! structure left by the readout electronics. It is the same pattern every
//! frame, so averaging frames together does nothing to it — which is why a
//! deep stack makes it *more* obvious rather than less. Once the random noise
//! has been averaged away, what is left standing is exactly this.
//!
//! Subtracting a frame of the same pattern removes it. That is all a dark is:
//! a picture of what the camera produces in the dark, taken at the same
//! exposure, gain and offset as the real frames, and subtracted from them.
//!
//! Like everything under [`crate::display`], this is applied to the preview
//! only. Recordings stay exactly as the sensor produced them, because
//! calibration belongs in processing where it can be redone, checked, and
//! combined with flats and bias frames.

use std::time::Duration;

use crate::error::{Error, Result};
use crate::frame::{Frame, FrameMeta};

/// How different an exposure may be and still count as matching, as a
/// fraction. Cameras round the exposure they were asked for, so an exact
/// comparison would reject a dark taken at the very same setting.
const EXPOSURE_TOLERANCE: f64 = 0.02;

/// The averaged frame a camera produces with no light reaching it.
#[derive(Debug, Clone)]
pub struct MasterDark {
    /// Mean value of every sample, in the frame's own sample order. Held as
    /// floats because the average of sixteen frames is not an integer, and
    /// rounding it away would put some of the noise back.
    samples: Vec<f32>,
    /// The level the pattern sits on, added back after subtraction.
    pedestal: f32,
    shape: Shape,
    /// The settings this was taken at. A dark is only valid for frames that
    /// match, because dark current scales with exposure and everything scales
    /// with gain.
    pub exposure_us: u64,
    pub gain: i64,
    pub offset: i64,
    /// How many frames were averaged. More is better: the master carries its
    /// own noise, reduced by the square root of this.
    pub frames: usize,
    /// Fraction of the samples that came in sitting at zero.
    ///
    /// Measured across the input frames rather than the average, because
    /// averaging hides it: covered frames at offset 0 on an SV305C Pro had
    /// 59.5% of their samples clipped, and not one sample of the resulting
    /// master was zero. What is clipped is gone, so the master describes a
    /// black level the sensor never actually reaches.
    clipped: f32,
}

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

/// Why a dark cannot be used with the frames now arriving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DarkMismatch {
    /// A different ROI, binning or bit depth: the frames are not even the
    /// same size.
    Shape,
    /// Dark current accumulates with time, so a dark is only valid for the
    /// exposure it was taken at.
    Exposure { dark_us: u64, frame_us: u64 },
    /// Gain amplifies the pattern along with everything else.
    Gain { dark: i64, frame: i64 },
    /// The offset sets the pedestal the pattern sits on.
    Offset { dark: i64, frame: i64 },
}

impl std::fmt::Display for DarkMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DarkMismatch::Shape => f.write_str("the frame size or bit depth has changed"),
            DarkMismatch::Exposure { dark_us, frame_us } => write!(
                f,
                "taken at {:.3}s, frames are {:.3}s",
                *dark_us as f64 / 1e6,
                *frame_us as f64 / 1e6
            ),
            DarkMismatch::Gain { dark, frame } => {
                write!(f, "taken at gain {dark}, frames are gain {frame}")
            }
            DarkMismatch::Offset { dark, frame } => {
                write!(f, "taken at offset {dark}, frames are offset {frame}")
            }
        }
    }
}

impl MasterDark {
    /// Average a set of frames into a master.
    ///
    /// Every frame must have the same shape; the settings are taken from the
    /// first. Fails on an empty set rather than inventing a neutral dark,
    /// because a dark of nothing would silently do nothing.
    pub fn from_frames(frames: &[Frame]) -> Result<MasterDark> {
        let first = frames
            .first()
            .ok_or_else(|| Error::other("no frames to build a master dark from"))?;
        let shape = Shape::of(&first.meta);
        let mut sums = vec![0f64; shape.samples()];

        let mut clipped = 0u64;
        for frame in frames {
            if Shape::of(&frame.meta) != shape {
                return Err(Error::other(
                    "the frames changed shape while the dark was being taken",
                ));
            }
            visit_samples(frame, |index, value| {
                sums[index] += f64::from(value);
                clipped += u64::from(value == 0);
            });
        }

        let count = frames.len() as f64;
        let samples: Vec<f32> = sums.iter().map(|total| (total / count) as f32).collect();
        // The level the pattern sits on. Added back after subtraction so the
        // result keeps its black point instead of landing on zero, where half
        // the noise would clip and bias everything above it.
        let pedestal = median(&samples);

        Ok(MasterDark {
            samples,
            pedestal,
            shape,
            exposure_us: first.meta.exposure_us,
            gain: first.meta.gain,
            offset: first.meta.offset,
            frames: frames.len(),
            clipped: clipped as f32 / (shape.samples() * frames.len()).max(1) as f32,
        })
    }

    pub fn width(&self) -> u32 {
        self.shape.width
    }

    pub fn height(&self) -> u32 {
        self.shape.height
    }

    pub fn exposure(&self) -> Duration {
        Duration::from_micros(self.exposure_us)
    }

    /// The level the dark sits at, which is the camera's black point.
    pub fn pedestal(&self) -> f32 {
        self.pedestal
    }

    /// Why this dark does not apply to a frame, or `None` if it does.
    pub fn mismatch(&self, meta: &FrameMeta) -> Option<DarkMismatch> {
        if Shape::of(meta) != self.shape {
            return Some(DarkMismatch::Shape);
        }
        let dark_us = self.exposure_us.max(1) as f64;
        if ((meta.exposure_us as f64) - dark_us).abs() / dark_us > EXPOSURE_TOLERANCE {
            return Some(DarkMismatch::Exposure {
                dark_us: self.exposure_us,
                frame_us: meta.exposure_us,
            });
        }
        if meta.gain != self.gain {
            return Some(DarkMismatch::Gain {
                dark: self.gain,
                frame: meta.gain,
            });
        }
        if meta.offset != self.offset {
            return Some(DarkMismatch::Offset {
                dark: self.offset,
                frame: meta.offset,
            });
        }
        None
    }

    /// Everything wrong with this master that is worth saying out loud.
    ///
    /// All of these are warnings rather than refusals: each is a heuristic,
    /// and an unusual setup should not be blocked by one. Returned together
    /// so a caller reports the lot instead of only the first thing noticed.
    pub fn complaints(&self, full_scale: u16) -> Vec<String> {
        let scale = f32::from(full_scale.max(1));
        let mut out = Vec::new();

        // A covered sensor is uniform; anything pointed at a scene has bright
        // parts. A large gap between the brightest samples and the typical
        // level means something was almost certainly in view.
        let spread = (percentile(&self.samples, 99.9) - self.pedestal) / scale;
        if spread > 0.10 {
            out.push(format!(
                "this does not look like a dark frame: the brightest parts sit \
                 {:.0}% of full scale above the rest. Is the camera covered?",
                spread * 100.0
            ));
        }

        // Clipping at the black end is not noise, it is missing data: those
        // pixels were darker than zero and the camera had nowhere to put
        // them. Subtracting a master built from them removes a pattern the
        // sensor has, but leaves behind the part that was cut off.
        if self.clipped > 0.02 {
            out.push(format!(
                "{:.0}% of this dark arrived clipped at zero — raise the offset \
                 until the black level clears the floor, or neither the dark \
                 nor the frames it corrects can be trusted at the bottom end",
                self.clipped * 100.0
            ));
        }

        out
    }

    /// Subtract the dark from a frame.
    ///
    /// The dark's own level is added back, so the result keeps the black
    /// point it had rather than landing on zero — where half of the noise
    /// would clip against the bottom of the range and bias everything above
    /// it. What is removed is the *pattern*, not the pedestal.
    pub fn apply(&self, frame: &Frame) -> Frame {
        if self.mismatch(&frame.meta).is_some() {
            return frame.clone();
        }
        let max = f32::from(u16::MAX);
        let mut data = vec![0u8; self.shape.samples() * self.shape.bytes_per_sample];

        if self.shape.bytes_per_sample == 1 {
            for (index, out) in data.iter_mut().enumerate() {
                let value = f32::from(frame.data[index]);
                let corrected = value - self.samples[index] + self.pedestal;
                *out = corrected.clamp(0.0, 255.0) as u8;
            }
        } else {
            for (index, out) in data.chunks_exact_mut(2).enumerate() {
                let value = f32::from(u16::from_le_bytes([
                    frame.data[index * 2],
                    frame.data[index * 2 + 1],
                ]));
                let corrected = value - self.samples[index] + self.pedestal;
                out.copy_from_slice(&(corrected.clamp(0.0, max) as u16).to_le_bytes());
            }
        }

        Frame::new(frame.meta.clone(), data)
            .expect("the corrected frame has the shape it came from")
    }
}

/// Walk a frame's samples widened to u16, shared so the sum and the
/// subtraction cannot disagree about the layout.
fn visit_samples(frame: &Frame, mut visit: impl FnMut(usize, u16)) {
    if frame.meta.bytes_per_sample() == 1 {
        for (index, byte) in frame.data.iter().enumerate() {
            visit(index, u16::from(*byte));
        }
    } else {
        for (index, pair) in frame.data.chunks_exact(2).enumerate() {
            visit(index, u16::from_le_bytes([pair[0], pair[1]]));
        }
    }
}

fn median(values: &[f32]) -> f32 {
    percentile(values, 50.0)
}

/// Approximate percentile via a histogram, which is far cheaper than sorting
/// two million floats and precise enough for a black level.
fn percentile(values: &[f32], percent: f32) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    let (mut low, mut high) = (f32::MAX, f32::MIN);
    for value in values {
        low = low.min(*value);
        high = high.max(*value);
    }
    if high <= low {
        return low;
    }
    const BINS: usize = 4096;
    let scale = (BINS - 1) as f32 / (high - low);
    let mut histogram = vec![0u32; BINS];
    for value in values {
        histogram[(((value - low) * scale) as usize).min(BINS - 1)] += 1;
    }
    let target = (values.len() as f32 * percent / 100.0) as u32;
    let mut cumulative = 0u32;
    for (bin, count) in histogram.iter().enumerate() {
        cumulative += count;
        if cumulative >= target {
            return low + bin as f32 / scale;
        }
    }
    high
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{Binning, BitDepth, Roi};
    use crate::frame::PixelFormat;
    use std::time::SystemTime;

    fn meta(width: u32, height: u32, depth: BitDepth) -> FrameMeta {
        FrameMeta {
            sequence: 0,
            timestamp: SystemTime::UNIX_EPOCH,
            width,
            height,
            format: PixelFormat::Mono,
            bit_depth: depth,
            exposure_us: 50_000,
            gain: 450,
            offset: 50,
            binning: Binning::ONE,
            roi: Roi::full(width, height),
            dropped: 0,
            temperature_c: None,
            settings_settled: true,
        }
    }

    /// A frame with a fixed hot-pixel pattern plus optional extra signal.
    fn frame_with_pattern(pattern: &[u16], extra: u16) -> Frame {
        let data: Vec<u8> = pattern
            .iter()
            .flat_map(|value| (value + extra).to_le_bytes())
            .collect();
        Frame::new(meta(4, 2, BitDepth::SIXTEEN), data).unwrap()
    }

    #[test]
    fn subtracting_a_dark_removes_the_fixed_pattern() {
        // Eight pixels sitting on a pedestal of 3200, three of them hot.
        let pattern = [3200u16, 3200, 9000, 3200, 3200, 6000, 3200, 4500];
        let dark = MasterDark::from_frames(&[frame_with_pattern(&pattern, 0)]).unwrap();

        // The same camera, now with 500 counts of real signal everywhere.
        let light = frame_with_pattern(&pattern, 500);
        let corrected = dark.apply(&light);

        // Every pixel should come back to the pedestal plus the signal, with
        // the hot ones no longer standing out at all.
        let values = corrected.to_u16();
        assert!(
            values.iter().all(|v| *v == values[0]),
            "the pattern should be gone, leaving a flat field: {values:?}"
        );
        assert_eq!(values[0], 3700, "pedestal 3200 plus 500 of signal");
    }

    #[test]
    fn the_black_point_survives_subtraction() {
        // Subtracting fully would land on zero, where half the noise clips.
        let pattern = [1000u16, 1000, 1000, 1000, 1000, 1000, 1000, 1000];
        let dark = MasterDark::from_frames(&[frame_with_pattern(&pattern, 0)]).unwrap();
        assert_eq!(dark.pedestal(), 1000.0);
        let corrected = dark.apply(&frame_with_pattern(&pattern, 0));
        assert_eq!(
            corrected.to_u16()[0],
            1000,
            "the level is kept, the pattern removed"
        );
    }

    #[test]
    fn averaging_several_darks_beats_using_one() {
        // A master carries its own noise; averaging reduces it, which matters
        // because that noise is added to every frame it is subtracted from.
        let truth = [3200u16; 8];
        let mut frames = Vec::new();
        let mut seed = 7u32;
        for _ in 0..16 {
            let noisy: Vec<u16> = truth
                .iter()
                .map(|v| {
                    seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                    v + ((seed >> 20) % 200) as u16
                })
                .collect();
            frames.push(frame_with_pattern(&noisy, 0));
        }
        let one = MasterDark::from_frames(&frames[..1]).unwrap();
        let many = MasterDark::from_frames(&frames).unwrap();
        assert_eq!(many.frames, 16);

        let spread = |dark: &MasterDark| {
            let values = &dark.samples;
            let mean: f32 = values.iter().sum::<f32>() / values.len() as f32;
            (values.iter().map(|v| (v - mean).powi(2)).sum::<f32>() / values.len() as f32).sqrt()
        };
        assert!(
            spread(&many) < spread(&one),
            "averaging should smooth the master: one {:.1}, sixteen {:.1}",
            spread(&one),
            spread(&many)
        );
    }

    #[test]
    fn a_dark_is_refused_when_the_settings_no_longer_match() {
        let dark = MasterDark::from_frames(&[frame_with_pattern(&[3200; 8], 0)]).unwrap();

        assert_eq!(dark.mismatch(&meta(4, 2, BitDepth::SIXTEEN)), None);

        // Dark current scales with time, so a different exposure is a
        // different dark.
        let mut other = meta(4, 2, BitDepth::SIXTEEN);
        other.exposure_us = 100_000;
        assert!(matches!(
            dark.mismatch(&other),
            Some(DarkMismatch::Exposure { .. })
        ));

        // But a few microseconds of rounding is the same setting.
        let mut rounded = meta(4, 2, BitDepth::SIXTEEN);
        rounded.exposure_us = 50_128;
        assert_eq!(dark.mismatch(&rounded), None);

        let mut gained = meta(4, 2, BitDepth::SIXTEEN);
        gained.gain = 100;
        assert!(matches!(
            dark.mismatch(&gained),
            Some(DarkMismatch::Gain { .. })
        ));

        let mut offset = meta(4, 2, BitDepth::SIXTEEN);
        offset.offset = 10;
        assert!(matches!(
            dark.mismatch(&offset),
            Some(DarkMismatch::Offset { .. })
        ));

        assert_eq!(
            dark.mismatch(&meta(8, 2, BitDepth::SIXTEEN)),
            Some(DarkMismatch::Shape)
        );
    }

    #[test]
    fn a_mismatched_dark_leaves_the_frame_alone() {
        let dark = MasterDark::from_frames(&[frame_with_pattern(&[3200; 8], 0)]).unwrap();
        let mut wrong = meta(4, 2, BitDepth::SIXTEEN);
        wrong.gain = 1;
        let frame = Frame::new(wrong, vec![0x10; 16]).unwrap();
        // Applying anyway would be worse than doing nothing: it would remove
        // a pattern the frame does not have.
        assert_eq!(dark.apply(&frame).data, frame.data);
    }

    /// A frame big enough for a high percentile to mean something, which a
    /// handful of pixels is not.
    fn big_frame(bright_fraction: f64) -> Frame {
        let (width, height) = (64u32, 64u32);
        let mut meta = meta(width, height, BitDepth::SIXTEEN);
        meta.roi = Roi::full(width, height);
        let total = (width * height) as usize;
        let bright = (total as f64 * bright_fraction) as usize;
        let mut data = Vec::with_capacity(total * 2);
        for index in 0..total {
            let value: u16 = if index < bright { 60_000 } else { 3_200 };
            data.extend_from_slice(&value.to_le_bytes());
        }
        Frame::new(meta, data).unwrap()
    }

    #[test]
    fn a_dark_that_arrived_clipped_says_so() {
        // Offset 0 on an SV305C Pro at gain 450: 59.5% of samples came back
        // at zero, and averaging them left a master with no zero samples at
        // all. So the count has to come from the frames, not the average.
        let clipped =
            MasterDark::from_frames(&[floored_frame(0.6, 0), floored_frame(0.6, 1)]).unwrap();
        assert!(
            !clipped.samples.iter().any(|&s| s <= 0.0),
            "the average hides it, which is the whole reason for counting on the way in"
        );
        let complaints = clipped.complaints(65535);
        assert!(
            complaints.iter().any(|c| c.contains("clipped")),
            "{complaints:?}"
        );

        // A dark sitting clear of the floor has nothing to complain about.
        let fine = MasterDark::from_frames(&[floored_frame(0.0, 0)]).unwrap();
        assert_eq!(fine.complaints(65535), Vec::<String>::new());
    }

    /// A covered frame where `floor_fraction` of the samples hit zero.
    ///
    /// `parity` picks which half of them clip, so two frames can floor the
    /// same proportion of pixels without flooring the *same* pixels — which
    /// is how a real sensor behaves, and why the average comes out clean.
    fn floored_frame(floor_fraction: f64, parity: usize) -> Frame {
        let (width, height) = (64u32, 64u32);
        let mut meta = meta(width, height, BitDepth::SIXTEEN);
        meta.roi = Roi::full(width, height);
        let total = (width * height) as usize;
        let floored = (total as f64 * floor_fraction) as usize;
        let mut data = Vec::with_capacity(total * 2);
        for index in 0..total {
            // Alternating either side of the pedestal, so the average of two
            // such frames lands above zero even where one of them clipped.
            let value: u16 = if index < floored {
                if index % 2 == parity { 0 } else { 400 }
            } else {
                3_200
            };
            data.extend_from_slice(&value.to_le_bytes());
        }
        Frame::new(meta, data).unwrap()
    }

    #[test]
    fn an_uncovered_camera_is_noticed() {
        // Flat and dim: what a covered sensor looks like.
        let covered = MasterDark::from_frames(&[big_frame(0.0)]).unwrap();
        assert_eq!(covered.complaints(65535), Vec::<String>::new());

        // A fifth of the frame bright: something was plainly in view.
        let uncovered = MasterDark::from_frames(&[big_frame(0.2)]).unwrap();
        let complaints = uncovered.complaints(65535);
        assert!(
            complaints.iter().any(|c| c.contains("covered")),
            "{complaints:?}"
        );

        // A scattering of hot pixels is not a scene, and must not be
        // mistaken for one — that is exactly what a dark is meant to capture.
        let hot = MasterDark::from_frames(&[big_frame(0.0005)]).unwrap();
        assert_eq!(hot.complaints(65535), Vec::<String>::new());
    }

    #[test]
    fn building_from_nothing_fails_rather_than_doing_nothing() {
        assert!(MasterDark::from_frames(&[]).is_err());
    }

    #[test]
    fn eight_bit_frames_are_handled_too() {
        let data = vec![10u8, 20, 30, 40, 50, 60, 70, 80];
        let frame = Frame::new(meta(4, 2, BitDepth::EIGHT), data.clone()).unwrap();
        let dark = MasterDark::from_frames(std::slice::from_ref(&frame)).unwrap();
        let corrected = dark.apply(&frame);
        let values = corrected.to_u16();
        assert!(
            values.iter().all(|v| *v == values[0]),
            "the pattern should flatten: {values:?}"
        );
    }
}
