//! Zstar core: the archive engine.
//!
//! No UI and no OS-specific code beyond permission bits and symlinks. The primary API works
//! on streams ([`ReadSeek`], [`WriteSeek`]); the `*_file` functions are thin path-based
//! wrappers around it.
//!
//! * [`detect`] identifies a format by its magic bytes.
//! * [`handler`] returns the [`ArchiveFormat`] implementation of a [`Format`].
//! * [`Options`] carries limits, thread count, progress reporting and cancellation.
//!
//! Extraction is safe by construction: entry names are sanitised, files are written to
//! `<name>.part` and renamed on success, and decompression-bomb limits are enforced on the
//! bytes actually written. See `extract.rs` for the full list of guarantees.

#![forbid(unsafe_code)]
// Without any format feature the engine has nothing to drive; keep that build warning-free.
#![cfg_attr(not(any(feature = "zip", feature = "tar")), allow(dead_code))]

mod detect;
mod error;
mod extract;
mod format;
mod formats;
mod options;
mod path;
mod progress;
mod sources;
#[cfg(feature = "zip")]
mod timeconv;

use std::fs::{self, File};
use std::path::{Path, PathBuf};

pub use detect::detect;
pub use error::{Error, LimitKind, Result};
pub use extract::Summary;
pub use format::{ArchiveFormat, EntryInfo, EntryKind, Format, ReadSeek, WriteSeek, handler};
pub use options::{CancelToken, Limits, Options, Progress, ProgressFn};
pub use path::sanitize as sanitize_entry_name;
pub use sources::{SourceEntry, collect_sources};

/// Lists the entries of an archive, detecting its format first.
pub fn list(
    input: &mut dyn ReadSeek,
    opts: &Options,
    visit: &mut dyn FnMut(EntryInfo),
) -> Result<Format> {
    let format = detect(input)?;
    handler(format)?.list(input, opts, visit)?;
    Ok(format)
}

/// Extracts an archive into `dest`, detecting its format first.
pub fn extract(input: &mut dyn ReadSeek, dest: &Path, opts: &Options) -> Result<Summary> {
    let format = detect(input)?;
    handler(format)?.extract(input, dest, opts)
}

/// Lists the archive at `path`.
pub fn list_file(path: &Path, opts: &Options, visit: &mut dyn FnMut(EntryInfo)) -> Result<Format> {
    list(&mut File::open(path)?, opts, visit)
}

/// Extracts the archive at `path` into `dest`.
pub fn extract_file(path: &Path, dest: &Path, opts: &Options) -> Result<Summary> {
    extract(&mut File::open(path)?, dest, opts)
}

/// Archives `inputs` into a new file at `output`, written as `<output>.part` and renamed
/// when complete. `output` itself is never archived, even when it lies inside an input.
pub fn create_file(
    format: Format,
    inputs: &[PathBuf],
    output: &Path,
    opts: &Options,
) -> Result<()> {
    if fs::symlink_metadata(output).is_ok() && !opts.overwrite {
        return Err(Error::AlreadyExists(output.into()));
    }
    let handler = handler(format)?;
    if inputs.is_empty() {
        return Err(Error::InvalidOption("nothing to archive".into()));
    }
    let output_abs = match (
        output.parent().filter(|p| !p.as_os_str().is_empty()),
        output.file_name(),
    ) {
        (Some(dir), Some(name)) => fs::canonicalize(dir)?.join(name),
        (None, Some(name)) => fs::canonicalize(".")?.join(name),
        _ => return Err(Error::InvalidOption("invalid output path".into())),
    };
    let part = extract::part_path(output);
    let part_abs = extract::part_path(&output_abs);
    let sources = collect_sources(inputs, &[output_abs, part_abs])?;

    let written = File::create(&part)
        .map_err(Error::from)
        .and_then(|mut file| handler.create(&sources, &mut file, opts));
    let renamed = written.and_then(|()| fs::rename(&part, output).map_err(Error::from));
    if renamed.is_err() {
        let _ = fs::remove_file(&part);
    }
    renamed
}
