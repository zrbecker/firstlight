//! A small, dependency-free FITS writer for single-image files.
//!
//! Only what a camera actually needs: one primary HDU, 8 or 16 bit integer
//! data, big-endian as the standard requires, with the acquisition keywords
//! that stacking software (Siril, PixInsight, AstroPixelProcessor) looks for.
//! Using this instead of `fitsio` keeps the build free of cfitsio, which
//! matters a great deal for shipping a macOS app bundle.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::SystemTime;

use crate::error::Result;
use crate::format::pad_ascii;
use crate::frame::{Frame, PixelFormat};
use crate::time_util::utc_from_system_time;

const BLOCK: usize = 2880;
const CARD: usize = 80;

/// Order the rows sit in the file. Cameras hand over the first row read out
/// first, which is the top row, so `TOP-DOWN` is the honest default; FITS
/// itself is nominally bottom-up and readers use this keyword to agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowOrder {
    TopDown,
    BottomUp,
}

impl RowOrder {
    fn as_str(&self) -> &'static str {
        match self {
            RowOrder::TopDown => "TOP-DOWN",
            RowOrder::BottomUp => "BOTTOM-UP",
        }
    }
}

/// Observation metadata that does not come from the frame itself.
#[derive(Debug, Clone)]
pub struct FitsMetadata {
    pub instrument: String,
    pub telescope: String,
    pub observer: String,
    pub object: String,
    /// Physical pixel size at bin 1, micrometres.
    pub pixel_size_um: Option<f32>,
    pub focal_length_mm: Option<f32>,
    pub row_order: RowOrder,
    /// White balance gains the camera was applying, in its own units.
    ///
    /// Worth recording because some cameras — SVBONY's and ZWO's among them —
    /// apply these to the raw data before any software sees it, and store
    /// nothing to say they did. Without the numbers the file cannot tell you
    /// how it was scaled, and the balance cannot be undone in processing.
    pub white_balance: Option<crate::control::WhiteBalance>,
    /// Extra `KEYWORD = value` cards, written verbatim as strings.
    pub extra: Vec<(String, String)>,
}

impl Default for FitsMetadata {
    fn default() -> Self {
        FitsMetadata {
            instrument: String::new(),
            telescope: String::new(),
            observer: String::new(),
            object: String::new(),
            pixel_size_um: None,
            focal_length_mm: None,
            row_order: RowOrder::TopDown,
            white_balance: None,
            extra: Vec::new(),
        }
    }
}

impl FitsMetadata {
    pub fn for_camera(instrument: impl Into<String>) -> FitsMetadata {
        FitsMetadata {
            instrument: instrument.into(),
            ..FitsMetadata::default()
        }
    }
}

/// Write one frame as a FITS file, replacing anything already at `path`.
pub fn write_fits(path: impl AsRef<Path>, frame: &Frame, meta: &FitsMetadata) -> Result<()> {
    let file = File::create(path.as_ref())?;
    let mut out = BufWriter::with_capacity(1 << 20, file);
    write_fits_to(&mut out, frame, meta)?;
    out.flush()?;
    out.get_mut().sync_all()?;
    Ok(())
}

/// Write one frame as FITS to any sink.
pub fn write_fits_to(out: &mut impl Write, frame: &Frame, meta: &FitsMetadata) -> Result<()> {
    let header = build_header(frame, meta);
    out.write_all(&header)?;
    write_data(out, frame)?;
    Ok(())
}

fn build_header(frame: &Frame, meta: &FitsMetadata) -> Vec<u8> {
    let m = &frame.meta;
    let sixteen_bit = m.bytes_per_sample() == 2;
    let planes = m.format.samples_per_pixel();
    let mut cards = Cards::default();

    cards.logical("SIMPLE", true, "conforms to FITS standard");
    cards.integer(
        "BITPIX",
        if sixteen_bit { 16 } else { 8 },
        "bits per sample",
    );
    cards.integer("NAXIS", if planes > 1 { 3 } else { 2 }, "number of axes");
    cards.integer("NAXIS1", i64::from(m.width), "image width");
    cards.integer("NAXIS2", i64::from(m.height), "image height");
    if planes > 1 {
        cards.integer("NAXIS3", planes as i64, "colour planes (R,G,B)");
    }
    if sixteen_bit {
        // FITS 16 bit integers are signed; BZERO shifts them to unsigned.
        cards.float("BZERO", 32768.0, "offset for unsigned 16-bit data");
        cards.float("BSCALE", 1.0, "linear scale factor");
    }

    let obs = utc_from_system_time(m.timestamp);
    cards.string("DATE-OBS", &obs.to_string(), "UTC at start of exposure");
    cards.float(
        "EXPTIME",
        m.exposure_us as f64 / 1_000_000.0,
        "exposure time, seconds",
    );
    cards.float(
        "EXPOSURE",
        m.exposure_us as f64 / 1_000_000.0,
        "exposure time, seconds",
    );
    cards.integer("GAIN", m.gain, "camera gain, native units");
    cards.integer("OFFSET", m.offset, "camera offset, ADU");
    cards.integer("BLKLEVEL", m.offset, "black level, ADU");
    cards.integer("XBINNING", i64::from(m.binning.factor()), "binning, X");
    cards.integer("YBINNING", i64::from(m.binning.factor()), "binning, Y");
    cards.integer("XORGSUBF", i64::from(m.roi.x), "ROI origin, X");
    cards.integer("YORGSUBF", i64::from(m.roi.y), "ROI origin, Y");
    cards.integer(
        "BITDEPTH",
        i64::from(m.bit_depth.bits()),
        "significant bits per sample",
    );
    cards.integer("FRAMENO", m.sequence as i64, "frame sequence number");

    if let Some(px) = meta.pixel_size_um {
        let binned = f64::from(px) * f64::from(m.binning.factor());
        cards.float("XPIXSZ", binned, "pixel size incl. binning, micron");
        cards.float("YPIXSZ", binned, "pixel size incl. binning, micron");
    }
    if let Some(fl) = meta.focal_length_mm {
        cards.float("FOCALLEN", f64::from(fl), "focal length, mm");
    }
    if let Some(t) = m.temperature_c {
        cards.float("CCD-TEMP", f64::from(t), "sensor temperature, C");
    }

    // Bayer keywords must only appear on genuinely mosaiced data, or stacking
    // software will happily debayer an already-debayered or mono image.
    if let PixelFormat::Bayer(pattern) = m.format {
        cards.string("BAYERPAT", pattern.as_str(), "colour filter array pattern");
        cards.integer("XBAYROFF", 0, "Bayer X offset within this frame");
        cards.integer("YBAYROFF", 0, "Bayer Y offset within this frame");
    }
    cards.string(
        "ROWORDER",
        meta.row_order.as_str(),
        "row order of image data",
    );

    if let Some(wb) = meta.white_balance {
        cards.integer("WB_R", wb.red, "red white balance gain, camera units");
        cards.integer("WB_G", wb.green, "green white balance gain, camera units");
        cards.integer("WB_B", wb.blue, "blue white balance gain, camera units");
    }

    if !meta.instrument.is_empty() {
        cards.string("INSTRUME", &meta.instrument, "camera");
    }
    if !meta.telescope.is_empty() {
        cards.string("TELESCOP", &meta.telescope, "telescope");
    }
    if !meta.observer.is_empty() {
        cards.string("OBSERVER", &meta.observer, "observer");
    }
    if !meta.object.is_empty() {
        cards.string("OBJECT", &meta.object, "target");
    }
    for (key, value) in &meta.extra {
        cards.string(key, value, "");
    }

    cards.string(
        "SWCREATE",
        concat!("firstlight ", env!("CARGO_PKG_VERSION")),
        "software that wrote this file",
    );
    cards.string(
        "DATE",
        &utc_from_system_time(SystemTime::now()).to_string(),
        "UTC this file was written",
    );
    cards.raw("END");
    cards.finish()
}

fn write_data(out: &mut impl Write, frame: &Frame) -> Result<()> {
    let m = &frame.meta;
    let planes = m.format.samples_per_pixel();
    let mut written = 0usize;
    let mut buf: Vec<u8> = Vec::with_capacity(m.width as usize * 2);

    if m.bytes_per_sample() == 1 && planes == 1 {
        // Already exactly the FITS byte layout.
        out.write_all(&frame.data)?;
        written += frame.data.len();
    } else {
        // FITS stores colour as separate planes, not interleaved, and 16 bit
        // samples big-endian and signed.
        for plane in 0..planes {
            for y in 0..m.height {
                buf.clear();
                for x in 0..m.width {
                    let sample = frame.sample(x, y, plane).unwrap_or(0);
                    if m.bytes_per_sample() == 2 {
                        let signed = i32::from(sample) - 32768;
                        buf.extend_from_slice(&(signed as i16).to_be_bytes());
                    } else {
                        buf.push(sample as u8);
                    }
                }
                out.write_all(&buf)?;
                written += buf.len();
            }
        }
    }

    // Data must fill whole 2880 byte blocks.
    let padding = (BLOCK - written % BLOCK) % BLOCK;
    if padding > 0 {
        out.write_all(&vec![0u8; padding])?;
    }
    Ok(())
}

/// Truncate to at most `max` bytes without splitting a character. Keyword and
/// value text can come from a user, and a panicking slice would be a poor way
/// to find that out.
fn clip(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Accumulates 80-column FITS cards.
#[derive(Default)]
struct Cards {
    buf: Vec<u8>,
}

impl Cards {
    fn push(&mut self, text: &str) {
        let start = self.buf.len();
        self.buf.resize(start + CARD, b' ');
        pad_ascii(&mut self.buf[start..start + CARD], text);
    }

    fn raw(&mut self, text: &str) {
        self.push(text);
    }

    /// `KEYWORD= <value padded to col 30> / comment`
    fn valued(&mut self, key: &str, value: &str, comment: &str) {
        let key = clip(key, 8);
        let mut card = format!("{key:<8}= {value:>20}");
        if !comment.is_empty() {
            card.push_str(" / ");
            card.push_str(comment);
            card.truncate(CARD);
        }
        self.push(&card);
    }

    fn integer(&mut self, key: &str, value: i64, comment: &str) {
        self.valued(key, &value.to_string(), comment);
    }

    fn float(&mut self, key: &str, value: f64, comment: &str) {
        // Fixed point, trailing zeros trimmed but always at least one decimal
        // digit so the card is unambiguously floating point rather than an
        // integer a strict reader would type differently.
        let mut text = format!("{value:.6}");
        if text.contains('.') {
            while text.ends_with('0') {
                text.pop();
            }
            if text.ends_with('.') {
                text.push('0');
            }
        }
        self.valued(key, &text, comment);
    }

    fn logical(&mut self, key: &str, value: bool, comment: &str) {
        self.valued(key, if value { "T" } else { "F" }, comment);
    }

    /// Strings are single-quoted, minimum 8 characters, with embedded quotes
    /// doubled as the standard requires.
    fn string(&mut self, key: &str, value: &str, comment: &str) {
        let escaped = value.replace('\'', "''");
        let escaped = clip(&escaped, 60);
        let quoted = format!("'{escaped:<8}'");
        let key = clip(key, 8);
        let mut card = format!("{key:<8}= {quoted}");
        if !comment.is_empty() {
            // Comments line up at column 33 where they fit, as is conventional.
            while card.len() < 32 {
                card.push(' ');
            }
            card.push_str(" / ");
            card.push_str(comment);
            card.truncate(CARD);
        }
        self.push(&card);
    }

    /// Pad the header out to whole 2880 byte blocks with blank cards.
    fn finish(mut self) -> Vec<u8> {
        let remainder = self.buf.len() % BLOCK;
        if remainder != 0 {
            self.buf.resize(self.buf.len() + (BLOCK - remainder), b' ');
        }
        self.buf
    }
}
