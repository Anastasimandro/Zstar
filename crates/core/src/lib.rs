//! Zstar core: the archive engine.
//!
//! No UI and no OS-specific code beyond permission bits and symlinks. The primary API works
//! on streams ([`ReadSeek`], [`WriteSeek`]); the `*_file` functions are thin path-based
//! wrappers around it.
//!
//! * [`detect`] identifies a format by its magic bytes.
//! * [`handler`] returns the [`ArchiveFormat`] implementation of a [`Format`].
//! * [`Options`] carries limits, thread count, progress reporting and the speed [`Profile`].
//! * [`Control`] steers a running operation: cancel, threads, priority, throughput limit.
//!
//! Extraction is safe by construction: entry names are sanitised, files are written to
//! `<name>.part` and renamed on success, and decompression-bomb limits are enforced on the
//! bytes actually written. See `extract.rs` for the full list of guarantees.

#![forbid(unsafe_code)]
// Without any format feature the engine has nothing to drive; keep that build warning-free.
#![cfg_attr(
    not(any(
        feature = "zip",
        feature = "sevenz",
        feature = "tar",
        feature = "gzip",
        feature = "zstd",
        feature = "bzip2",
        feature = "xz",
        feature = "lz4",
        feature = "brotli"
    )),
    allow(dead_code)
)]

pub mod config;
mod control;
mod detect;
mod error;
mod extract;
mod format;
mod formats;
mod govern;
mod hygiene;
mod options;
mod path;
#[cfg(feature = "zip")]
mod pipeline;
mod profile;
mod progress;
mod sources;
mod stats;
#[cfg(feature = "zip")]
mod timeconv;
mod tune;

use std::fs::{self, File};
use std::path::{Path, PathBuf};

pub use control::{Control, Priority, WorkerHook};
pub use detect::{detect, detect_named};
pub use error::{Error, LimitKind, Result};
pub use extract::Summary;
pub use format::{ArchiveFormat, EntryInfo, EntryKind, Format, ReadSeek, WriteSeek, handler};
pub use govern::{
    Governor, Running, Signals, SignalsHook, TICK, apply as apply_knobs, start as start_governor,
};
pub use hygiene::{CacheHook, CacheOp};
pub use options::{Limits, OpenHook, Options, Password, Progress, ProgressFn, ZipMethod};
pub use path::sanitize as sanitize_entry_name;
pub use profile::{DriveKind, Profile, Settings, settings_for};
pub use sources::{Listed, SourceEntry, SourceStream, Totals, collect_sources};
pub use stats::{Stage, Stats, StatsSnapshot};
pub use tune::{Cause, Decision, Knob, Knobs, Mode, Reason, Sample, TuneBounds, Tuner};

/// Lists the entries of an archive, detecting its format first.
///
/// Brotli has no signature, so it is only recognised through [`Options::name_hint`].
pub fn list(
    input: &mut dyn ReadSeek,
    opts: &Options,
    visit: &mut dyn FnMut(EntryInfo),
) -> Result<Format> {
    let format = detect::detect_named_limited(
        input,
        opts.name_hint.as_deref(),
        opts.limits.max_memory_bytes,
    )?;
    handler(format)?.list(input, opts, visit)?;
    Ok(format)
}

/// Extracts an archive into `dest`, detecting its format first.
///
/// Brotli has no signature, so it is only recognised through [`Options::name_hint`].
pub fn extract(input: &mut dyn ReadSeek, dest: &Path, opts: &Options) -> Result<Summary> {
    let format = detect::detect_named_limited(
        input,
        opts.name_hint.as_deref(),
        opts.limits.max_memory_bytes,
    )?;
    handler(format)?.extract(input, dest, opts)
}

/// `opts` with [`Options::name_hint`] set to the file name of `path`, unless already set.
fn named_after(path: &Path, opts: &Options) -> Options {
    let mut opts = opts.clone();
    if opts.name_hint.is_none() {
        opts.name_hint = path.file_name().and_then(|n| n.to_str()).map(str::to_owned);
    }
    opts
}

/// Lists the archive at `path`.
pub fn list_file(path: &Path, opts: &Options, visit: &mut dyn FnMut(EntryInfo)) -> Result<Format> {
    let opts = named_after(path, opts);
    list(&mut opts.open_source(path)?, &opts, visit)
}

/// Extracts the archive at `path` into `dest`.
pub fn extract_file(path: &Path, dest: &Path, opts: &Options) -> Result<Summary> {
    let opts = named_after(path, opts);
    stats::timed_op(opts.stats.as_ref(), || {
        let mut input = opts.open_source(path)?;
        extract(&mut input, dest, &opts)
    })
}

/// Archives `inputs` into a new file at `output`, written as `<output>.part` and renamed
/// when complete. `output` itself is never archived, even when it lies inside an input.
pub fn create_file(
    format: Format,
    inputs: &[PathBuf],
    output: &Path,
    opts: &Options,
) -> Result<()> {
    stats::timed_op(opts.stats.as_ref(), || {
        create_file_inner(format, inputs, output, opts)
    })
}

fn create_file_inner(
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
    let st = opts.stats.as_ref();
    let written = stats::fs(st, stats::FsOp::CreateOut, || File::create(&part))
        .map_err(Error::from)
        .and_then(|file| {
            let mut file = hygiene::Hygienic::open(file, opts, true);
            // The tree is walked on a helper thread while the format writes.
            sources::with_walk(inputs, &[output_abs, part_abs], opts, |stream| {
                handler.create_stream(stream, &mut file, opts)
            })?;
            // Closing is where the OS may make us wait; it hands the data over, no fsync.
            stats::timed(st, stats::Class::Finish, || drop(file));
            Ok(())
        });
    let renamed = written.and_then(|()| {
        stats::fs(st, stats::FsOp::RenameOut, || fs::rename(&part, output)).map_err(Error::from)
    });
    if renamed.is_err() {
        let _ = fs::remove_file(&part);
    }
    renamed
}
