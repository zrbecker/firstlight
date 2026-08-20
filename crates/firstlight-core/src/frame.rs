//! Frames as they come off the sensor, plus the metadata that must travel
//! with them all the way into a SER or FITS file.

use std::sync::Arc;
use std::time::SystemTime;

use crate::control::{Binning, BitDepth, Roi};

/// Colour filter array phase of the top-left pixel of the *full sensor*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BayerPattern {
    Rggb,
    Bggr,
    Grbg,
    Gbrg,
}

impl BayerPattern {
    /// The pattern as seen from an ROI whose origin is `(x, y)` on the sensor.
    /// An odd offset swaps the phase; ROIs are normally forced even, but a
    /// backend that allows odd origins still reports the truth this way.
    pub fn shifted(self, x: u32, y: u32) -> BayerPattern {
        use BayerPattern::*;
        let mut p = self;
        if x % 2 == 1 {
            p = match p {
                Rggb => Grbg,
                Grbg => Rggb,
                Bggr => Gbrg,
                Gbrg => Bggr,
            };
        }
        if y % 2 == 1 {
            p = match p {
                Rggb => Gbrg,
                Gbrg => Rggb,
                Bggr => Grbg,
                Grbg => Bggr,
            };
        }
        p
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            BayerPattern::Rggb => "RGGB",
            BayerPattern::Bggr => "BGGR",
            BayerPattern::Grbg => "GRBG",
            BayerPattern::Gbrg => "GBRG",
        }
    }

    pub fn parse(s: &str) -> Option<BayerPattern> {
        Some(match s.to_ascii_uppercase().as_str() {
            "RGGB" => BayerPattern::Rggb,
            "BGGR" => BayerPattern::Bggr,
            "GRBG" => BayerPattern::Grbg,
            "GBRG" => BayerPattern::Gbrg,
            _ => return None,
        })
    }

    /// Channel index (0=R, 1=G, 2=B) of the pixel at `(x, y)` within the frame.
    pub fn channel_at(&self, x: u32, y: u32) -> usize {
        let (even_row, even_col) = (y % 2 == 0, x % 2 == 0);
        match (self, even_row, even_col) {
            (BayerPattern::Rggb, true, true) => 0,
            (BayerPattern::Rggb, true, false) => 1,
            (BayerPattern::Rggb, false, true) => 1,
            (BayerPattern::Rggb, false, false) => 2,

            (BayerPattern::Bggr, true, true) => 2,
            (BayerPattern::Bggr, true, false) => 1,
            (BayerPattern::Bggr, false, true) => 1,
            (BayerPattern::Bggr, false, false) => 0,

            (BayerPattern::Grbg, true, true) => 1,
            (BayerPattern::Grbg, true, false) => 0,
            (BayerPattern::Grbg, false, true) => 2,
            (BayerPattern::Grbg, false, false) => 1,

            (BayerPattern::Gbrg, true, true) => 1,
            (BayerPattern::Gbrg, true, false) => 2,
            (BayerPattern::Gbrg, false, true) => 0,
            (BayerPattern::Gbrg, false, false) => 1,
        }
    }
}

impl std::fmt::Display for BayerPattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Layout of the bytes in [`Frame::data`].
///
/// Frames are always delivered in the sensor's native layout; nothing in the
/// capture path debayers or rescales. Display code makes its own copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// One sample per pixel, no colour filter.
    Mono,
    /// One sample per pixel behind a colour filter array.
    Bayer(BayerPattern),
    /// Three interleaved samples per pixel, R then G then B.
    Rgb,
}

impl PixelFormat {
    pub fn samples_per_pixel(&self) -> usize {
        match self {
            PixelFormat::Mono | PixelFormat::Bayer(_) => 1,
            PixelFormat::Rgb => 3,
        }
    }

    pub fn bayer(&self) -> Option<BayerPattern> {
        match self {
            PixelFormat::Bayer(p) => Some(*p),
            _ => None,
        }
    }

    pub fn is_colour(&self) -> bool {
        !matches!(self, PixelFormat::Mono)
    }
}

impl std::fmt::Display for PixelFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PixelFormat::Mono => f.write_str("MONO"),
            PixelFormat::Bayer(p) => write!(f, "BAYER_{p}"),
            PixelFormat::Rgb => f.write_str("RGB"),
        }
    }
}

/// Everything about a frame except the pixels.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameMeta {
    /// Monotonically increasing index of frames *delivered* by the backend.
    /// Gaps against `dropped` are the honest record of what was lost.
    pub sequence: u64,
    /// Wall-clock time the frame was handed over by the SDK. Used for the SER
    /// timestamp trailer and the FITS `DATE-OBS` card.
    pub timestamp: SystemTime,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub bit_depth: BitDepth,
    pub exposure_us: u64,
    pub gain: i64,
    pub offset: i64,
    pub binning: Binning,
    pub roi: Roi,
    /// Cumulative count of frames the backend produced but nobody consumed
    /// in time, since the stream started.
    pub dropped: u64,
    pub temperature_c: Option<f32>,
}

impl FrameMeta {
    pub fn bytes_per_sample(&self) -> usize {
        self.bit_depth.bytes_per_sample()
    }

    pub fn stride(&self) -> usize {
        self.width as usize * self.format.samples_per_pixel() * self.bytes_per_sample()
    }

    /// Exactly how many bytes [`Frame::data`] must hold.
    pub fn expected_len(&self) -> usize {
        self.stride() * self.height as usize
    }
}

/// One frame: metadata plus a reference-counted buffer.
///
/// The buffer is shared rather than copied because the recorder and the
/// display path both want the same bytes and neither should stall the other.
#[derive(Debug, Clone)]
pub struct Frame {
    pub meta: FrameMeta,
    pub data: Arc<[u8]>,
}

impl Frame {
    /// Build a frame, checking the buffer against the metadata. A backend that
    /// mis-sizes a buffer is a bug worth catching at the boundary, not a
    /// panic three layers down in the debayer.
    pub fn new(meta: FrameMeta, data: impl Into<Arc<[u8]>>) -> crate::Result<Frame> {
        let data: Arc<[u8]> = data.into();
        if data.len() != meta.expected_len() {
            return Err(crate::Error::other(format!(
                "frame buffer is {} bytes, metadata describes {} ({}x{} {} {})",
                data.len(),
                meta.expected_len(),
                meta.width,
                meta.height,
                meta.format,
                meta.bit_depth
            )));
        }
        Ok(Frame { meta, data })
    }

    pub fn width(&self) -> u32 {
        self.meta.width
    }

    pub fn height(&self) -> u32 {
        self.meta.height
    }

    /// Sample at `(x, y, channel)` widened to u16, or `None` if out of bounds.
    pub fn sample(&self, x: u32, y: u32, channel: usize) -> Option<u16> {
        if x >= self.meta.width || y >= self.meta.height {
            return None;
        }
        let spp = self.meta.format.samples_per_pixel();
        if channel >= spp {
            return None;
        }
        let bps = self.meta.bytes_per_sample();
        let idx = y as usize * self.meta.stride() + (x as usize * spp + channel) * bps;
        Some(match bps {
            1 => u16::from(self.data[idx]),
            _ => u16::from_le_bytes([self.data[idx], self.data[idx + 1]]),
        })
    }

    /// All samples widened to u16, row-major, channels interleaved.
    pub fn to_u16(&self) -> Vec<u16> {
        match self.meta.bytes_per_sample() {
            1 => self.data.iter().map(|&b| u16::from(b)).collect(),
            _ => self
                .data
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect(),
        }
    }
}
