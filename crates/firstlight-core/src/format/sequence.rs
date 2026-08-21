//! Numbered file names, and a writer that fills a directory with FITS frames.
//!
//! A capture run produces one file per frame, named from a template the user
//! gives: hand it `image_001.fits` and it writes `image_001.fits`,
//! `image_002.fits`, and so on. The template carries the width of the number
//! and where the counting starts, so `m42_0001.fit` counts four wide from one
//! and keeps the `.fit` extension.
//!
//! Nothing here ever overwrites. Frames from a night's observing cannot be
//! taken again, so an occupied name is skipped rather than replaced.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::format::fits::{FitsMetadata, write_fits};
use crate::frame::Frame;

/// Number width used when the template has no digits to copy.
const DEFAULT_DIGITS: usize = 4;
/// Extension used when the template has none.
const DEFAULT_EXTENSION: &str = "fits";

/// Successive numbered paths derived from a template.
#[derive(Debug, Clone)]
pub struct FileSequence {
    directory: PathBuf,
    prefix: String,
    digits: usize,
    extension: String,
    next: u64,
}

impl FileSequence {
    /// Build a sequence from a template path.
    ///
    /// The trailing digits of the file stem set the starting number and how
    /// wide it is printed; `frame.fits` with no digits starts at 1 and counts
    /// four wide. A template with no extension gets `.fits`, and one with no
    /// directory lands in the current one.
    pub fn from_template(template: impl AsRef<Path>) -> Result<FileSequence> {
        let template = template.as_ref();
        let directory = template
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();

        let name = template
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .filter(|name| !name.is_empty())
            .ok_or_else(|| {
                Error::other(format!("{} has no file name to number", template.display()))
            })?;

        // Split off the extension by hand rather than through `Path`, so a
        // stem that is all digits (`0001.fits`) still works.
        let (stem, extension) = match name.rsplit_once('.') {
            Some((stem, extension)) if !stem.is_empty() && !extension.is_empty() => {
                (stem.to_string(), extension.to_string())
            }
            _ => (name.clone(), DEFAULT_EXTENSION.to_string()),
        };

        let trailing_digits =
            stem.len() - stem.trim_end_matches(|c: char| c.is_ascii_digit()).len();
        let (prefix, digits, next) = if trailing_digits == 0 {
            // Nothing to count from, so append a number and leave the name as
            // the prefix — with a separator, or `frame.fits` would become
            // `frame0001.fits`.
            let prefix = if stem.ends_with(['_', '-', '.']) {
                stem.clone()
            } else {
                format!("{stem}_")
            };
            (prefix, DEFAULT_DIGITS, 1)
        } else {
            let split = stem.len() - trailing_digits;
            let number = &stem[split..];
            (
                stem[..split].to_string(),
                trailing_digits,
                number.parse::<u64>().unwrap_or(1),
            )
        };

        Ok(FileSequence {
            directory,
            prefix,
            digits,
            extension,
            next,
        })
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// The number the next file will be given.
    pub fn next_index(&self) -> u64 {
        self.next
    }

    /// The path for a given index, without consuming it.
    pub fn path_for(&self, index: u64) -> PathBuf {
        self.directory.join(format!(
            "{}{:0width$}.{}",
            self.prefix,
            index,
            self.extension,
            width = self.digits
        ))
    }

    /// The next unused index, without consuming it.
    ///
    /// Skipping rather than overwriting is deliberate: pointing a new run at
    /// a directory that already holds frames is a normal thing to do, and
    /// silently destroying them would be unforgivable.
    pub fn peek_free(&self) -> u64 {
        let mut index = self.next;
        while self.path_for(index).exists() {
            index += 1;
        }
        index
    }

    /// Take the next unused path, skipping any that already exist.
    pub fn next_free(&mut self) -> PathBuf {
        self.next = self.peek_free();
        let path = self.path_for(self.next);
        self.next += 1;
        path
    }
}

/// Writes a run of frames into a directory as individual FITS files.
pub struct FitsSequenceWriter {
    sequence: FileSequence,
    meta: FitsMetadata,
    frames: u64,
    bytes: u64,
    /// The first index this run was given, so a run that skipped over
    /// existing files can say so.
    first_index: u64,
    skipped_existing: bool,
    last_written: Option<PathBuf>,
}

impl FitsSequenceWriter {
    /// Create the directory if needed and prepare to write.
    pub fn create(template: impl AsRef<Path>, meta: FitsMetadata) -> Result<FitsSequenceWriter> {
        let sequence = FileSequence::from_template(template)?;
        std::fs::create_dir_all(sequence.directory()).map_err(|e| {
            Error::other(format!(
                "cannot use {}: {e}",
                sequence.directory().display()
            ))
        })?;

        // Work out where the run will start now, so the caller can be told up
        // front rather than after the first frame is written.
        let requested = sequence.next_index();
        let first_index = sequence.peek_free();

        Ok(FitsSequenceWriter {
            sequence,
            meta,
            frames: 0,
            bytes: 0,
            first_index,
            skipped_existing: first_index != requested,
            last_written: None,
        })
    }

    pub fn directory(&self) -> &Path {
        self.sequence.directory()
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    pub fn bytes_written(&self) -> u64 {
        self.bytes
    }

    /// True when existing files pushed the first frame past the number the
    /// template asked for. Worth telling the user, since their frames will
    /// not be numbered where they expected.
    pub fn skipped_existing(&self) -> bool {
        self.skipped_existing
    }

    pub fn first_index(&self) -> u64 {
        self.first_index
    }

    /// The path the most recent frame went to.
    pub fn last_written(&self) -> Option<&Path> {
        self.last_written.as_deref()
    }

    /// The path the next frame will go to.
    pub fn next_path(&self) -> PathBuf {
        self.sequence.path_for(self.sequence.peek_free())
    }

    /// Write one frame, and report where it went.
    pub fn write_frame(&mut self, frame: &Frame) -> Result<PathBuf> {
        let path = self.sequence.next_free();
        // The frame carries its own exposure, gain and geometry; `meta` only
        // supplies what a frame cannot know about itself.
        write_fits(&path, frame, &self.meta)?;

        self.frames += 1;
        self.bytes += frame.data.len() as u64;
        self.last_written = Some(path.clone());
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_numbered_template_sets_the_width_and_the_start() {
        let sequence = FileSequence::from_template("/tmp/run/image_001.fits").unwrap();
        assert_eq!(sequence.directory(), Path::new("/tmp/run"));
        assert_eq!(sequence.path_for(1), Path::new("/tmp/run/image_001.fits"));
        assert_eq!(sequence.path_for(2), Path::new("/tmp/run/image_002.fits"));
        // Past the width, the number simply grows rather than failing.
        assert_eq!(
            sequence.path_for(1000),
            Path::new("/tmp/run/image_1000.fits")
        );
    }

    #[test]
    fn counting_starts_where_the_template_says() {
        let sequence = FileSequence::from_template("/tmp/m42_0042.fit").unwrap();
        assert_eq!(sequence.next_index(), 42);
        assert_eq!(sequence.path_for(42), Path::new("/tmp/m42_0042.fit"));
        assert_eq!(sequence.path_for(43), Path::new("/tmp/m42_0043.fit"));
    }

    #[test]
    fn a_template_without_digits_gets_a_number_appended() {
        let sequence = FileSequence::from_template("/tmp/light.fits").unwrap();
        assert_eq!(sequence.path_for(1), Path::new("/tmp/light_0001.fits"));

        // A separator already there is not doubled.
        let sequence = FileSequence::from_template("/tmp/light_.fits").unwrap();
        assert_eq!(sequence.path_for(7), Path::new("/tmp/light_0007.fits"));
    }

    #[test]
    fn a_template_without_an_extension_gets_fits() {
        let sequence = FileSequence::from_template("/tmp/light_01").unwrap();
        assert_eq!(sequence.path_for(1), Path::new("/tmp/light_01.fits"));
    }

    #[test]
    fn a_bare_name_lands_in_the_current_directory() {
        let sequence = FileSequence::from_template("image_001.fits").unwrap();
        assert_eq!(sequence.directory(), Path::new("."));
    }

    #[test]
    fn an_all_digit_stem_still_counts() {
        let sequence = FileSequence::from_template("/tmp/0005.fits").unwrap();
        assert_eq!(sequence.next_index(), 5);
        assert_eq!(sequence.path_for(6), Path::new("/tmp/0006.fits"));
    }

    #[test]
    fn existing_files_are_skipped_rather_than_overwritten() {
        let directory = std::env::temp_dir().join(format!("firstlight-seq-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        for index in 1..=3 {
            std::fs::write(directory.join(format!("image_{index:03}.fits")), b"taken").unwrap();
        }

        let mut sequence = FileSequence::from_template(directory.join("image_001.fits")).unwrap();
        let next = sequence.next_free();
        assert_eq!(next, directory.join("image_004.fits"));
        // And the frames that were already there are untouched.
        assert_eq!(
            std::fs::read(directory.join("image_001.fits")).unwrap(),
            b"taken"
        );
        std::fs::remove_dir_all(&directory).ok();
    }
}
