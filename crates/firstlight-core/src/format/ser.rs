//! SER v3 writer.
//!
//! SER is a flat container: a 178 byte header, then raw frames back to back,
//! then an optional trailer of one 64 bit timestamp per frame. That is all,
//! which is precisely why planetary and lucky-imaging tools like it — and why
//! the writer must be strict about every frame having identical geometry.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::error::{Error, Result};
use crate::format::pad_ascii;
use crate::frame::{BayerPattern, Frame, FrameMeta, PixelFormat};
use crate::time_util::ticks_from_system_time;

const HEADER_LEN: usize = 178;
const FILE_ID: &str = "LUCAM-RECORDER";

/// SER colour identifiers, from the v3 specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum SerColorId {
    Mono = 0,
    BayerRggb = 8,
    BayerGrbg = 9,
    BayerGbrg = 10,
    BayerBggr = 11,
    Rgb = 100,
    Bgr = 101,
}

impl SerColorId {
    pub fn from_format(format: PixelFormat) -> SerColorId {
        match format {
            PixelFormat::Mono => SerColorId::Mono,
            PixelFormat::Rgb => SerColorId::Rgb,
            PixelFormat::Bayer(BayerPattern::Rggb) => SerColorId::BayerRggb,
            PixelFormat::Bayer(BayerPattern::Grbg) => SerColorId::BayerGrbg,
            PixelFormat::Bayer(BayerPattern::Gbrg) => SerColorId::BayerGbrg,
            PixelFormat::Bayer(BayerPattern::Bggr) => SerColorId::BayerBggr,
        }
    }
}

/// The text fields SER carries about the observation.
#[derive(Debug, Clone, Default)]
pub struct SerMetadata {
    pub observer: String,
    pub instrument: String,
    pub telescope: String,
    /// Value written to the `LittleEndian` header field.
    ///
    /// The v3 spec says 1 means the 16 bit samples are little-endian, but the
    /// original recorder wrote it inverted and essentially every reader in
    /// the wild (PIPP, AutoStakkert!, SER Player) now assumes little-endian
    /// data regardless of the flag. We always *write* little-endian samples;
    /// this only selects which of the two conventions the flag advertises.
    /// The default, 0, is what SharpCap and FireCapture emit.
    pub little_endian_flag: bool,
}

impl SerMetadata {
    pub fn for_camera(instrument: impl Into<String>) -> SerMetadata {
        SerMetadata {
            instrument: instrument.into(),
            ..SerMetadata::default()
        }
    }
}

/// Geometry every frame in a given file must match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Geometry {
    width: u32,
    height: u32,
    /// SER's `PixelDepthPerPlane`: significant bits, not container bits.
    bit_depth: u8,
    colour: SerColorId,
    bytes_per_frame: usize,
}

impl Geometry {
    fn from_meta(meta: &FrameMeta) -> Geometry {
        Geometry {
            width: meta.width,
            height: meta.height,
            bit_depth: meta.bit_depth.bits(),
            colour: SerColorId::from_format(meta.format),
            bytes_per_frame: meta.expected_len(),
        }
    }
}

/// Streaming SER writer.
///
/// The header's frame count is patched on [`SerWriter::finish`]; if the
/// process dies first the file still contains every frame written so far and
/// most tools will read it, so an interrupted capture is not a lost capture.
pub struct SerWriter {
    path: PathBuf,
    file: BufWriter<File>,
    meta: SerMetadata,
    geometry: Option<Geometry>,
    frame_count: u32,
    timestamps: Vec<i64>,
    bytes_written: u64,
    finished: bool,
}

impl SerWriter {
    /// Create a file. Geometry is taken from the first frame written, so the
    /// caller does not have to know it up front.
    pub fn create(path: impl AsRef<Path>, meta: SerMetadata) -> Result<SerWriter> {
        let path = path.as_ref().to_path_buf();
        let file = File::create(&path)?;
        let mut writer = SerWriter {
            path,
            file: BufWriter::with_capacity(1 << 20, file),
            meta,
            geometry: None,
            frame_count: 0,
            timestamps: Vec::new(),
            bytes_written: 0,
            finished: false,
        };
        // Placeholder header, rewritten in full by `finish`.
        writer.file.write_all(&[0u8; HEADER_LEN])?;
        writer.bytes_written += HEADER_LEN as u64;
        Ok(writer)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn frame_count(&self) -> u32 {
        self.frame_count
    }

    /// Bytes committed to the file so far, header included.
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    /// Append a frame.
    ///
    /// Returns [`Error::InvalidGeometry`] if it does not match the first
    /// frame: a SER file with a mid-stream size change is silently corrupt,
    /// and this is the last place to catch it.
    pub fn write_frame(&mut self, frame: &Frame) -> Result<()> {
        let geometry = Geometry::from_meta(&frame.meta);
        match self.geometry {
            None => self.geometry = Some(geometry),
            Some(existing) if existing != geometry => {
                return Err(Error::InvalidGeometry(format!(
                    "SER frame {} is {}x{} {}-bit, file is {}x{} {}-bit",
                    self.frame_count,
                    geometry.width,
                    geometry.height,
                    geometry.bit_depth,
                    existing.width,
                    existing.height,
                    existing.bit_depth
                )));
            }
            Some(_) => {}
        }
        if self.frame_count == u32::MAX {
            return Err(Error::other("SER frame count would overflow"));
        }

        self.file.write_all(&frame.data)?;
        self.bytes_written += frame.data.len() as u64;
        self.timestamps
            .push(ticks_from_system_time(frame.meta.timestamp));
        self.frame_count += 1;
        Ok(())
    }

    /// Write the timestamp trailer and the real header, then flush.
    pub fn finish(mut self) -> Result<u32> {
        self.finish_inner()?;
        Ok(self.frame_count)
    }

    fn finish_inner(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;

        // Trailer: one 64 bit tick count per frame, in frame order.
        for ticks in &self.timestamps {
            self.file.write_all(&ticks.to_le_bytes())?;
        }
        self.bytes_written += (self.timestamps.len() * 8) as u64;

        let header = self.header_bytes();
        self.file.flush()?;
        let file = self.file.get_mut();
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&header)?;
        file.flush()?;
        file.sync_all()?;
        Ok(())
    }

    fn header_bytes(&self) -> [u8; HEADER_LEN] {
        let geometry = self.geometry.unwrap_or(Geometry {
            width: 0,
            height: 0,
            bit_depth: 8,
            colour: SerColorId::Mono,
            bytes_per_frame: 0,
        });
        let mut header = [0u8; HEADER_LEN];
        pad_ascii(&mut header[0..14], FILE_ID);
        let put_i32 = |offset: usize, value: i32, header: &mut [u8; HEADER_LEN]| {
            header[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        };
        put_i32(14, 0, &mut header); // LuID, unused
        put_i32(18, geometry.colour as i32, &mut header);
        put_i32(22, i32::from(self.meta.little_endian_flag), &mut header);
        put_i32(26, geometry.width as i32, &mut header);
        put_i32(30, geometry.height as i32, &mut header);
        put_i32(34, i32::from(geometry.bit_depth), &mut header);
        put_i32(38, self.frame_count as i32, &mut header);
        pad_ascii(&mut header[42..82], &self.meta.observer);
        pad_ascii(&mut header[82..122], &self.meta.instrument);
        pad_ascii(&mut header[122..162], &self.meta.telescope);

        // Both DateTime fields get UTC: guessing at a local offset without a
        // timezone database would just be wrong in a different way.
        let now = self
            .timestamps
            .first()
            .copied()
            .unwrap_or_else(|| ticks_from_system_time(SystemTime::now()));
        header[162..170].copy_from_slice(&now.to_le_bytes());
        header[170..178].copy_from_slice(&now.to_le_bytes());
        header
    }
}

impl Drop for SerWriter {
    fn drop(&mut self) {
        // A recording aborted by a panic or an early return should still
        // leave a readable file behind.
        if let Err(e) = self.finish_inner() {
            tracing::error!(path = %self.path.display(), error = %e, "failed to finalise SER file");
        }
    }
}
