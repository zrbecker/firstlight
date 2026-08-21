//! File writers for the two formats astronomy tooling actually reads:
//! SER for video, FITS for stills.
//!
//! Both writers take frames exactly as the camera produced them. Nothing here
//! debayers, stretches or rescales — that belongs to the display path only,
//! and baking it into a recording throws away data you cannot get back.

pub mod fits;
pub mod sequence;
pub mod ser;

pub use fits::{FitsMetadata, write_fits};
pub use sequence::{FileSequence, FitsSequenceWriter};
pub use ser::{SerColorId, SerMetadata, SerWriter};

/// Copy `src` into a fixed-size, space-padded ASCII field, the way both SER
/// and FITS want their text.
pub(crate) fn pad_ascii(dst: &mut [u8], src: &str) {
    dst.fill(b' ');
    for (slot, byte) in dst.iter_mut().zip(src.bytes()) {
        // Non-ASCII would corrupt fixed-width fields; substitute rather than
        // truncate so the field stays aligned.
        *slot = if byte.is_ascii_graphic() || byte == b' ' {
            byte
        } else {
            b'?'
        };
    }
}
