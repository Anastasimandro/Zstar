//! RAR and RAR5, extract only, through `zstar-rar` (vendored libarchive).
//!
//! RAR can never be created: its compressor is proprietary and its licence forbids recreating
//! it, so `create_stream` always refuses. There is no password support either: libarchive can
//! only detect RAR encryption, so an encrypted archive fails with
//! [`Error::EncryptedUnsupported`] instead of asking for a password.
//!
//! Extraction is a single pass over the stream. Names go through the shared sanitiser and
//! limits, files are written to `.part` and renamed by [`Session`], and the declared size of
//! each file is checked before a byte of it is read. The total is not known up front (finding
//! it would mean decoding a solid archive twice), so progress has no percentage.

use std::io::{self, Read, Seek};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use zstar_rar::{Entry, EntryKind as RarKind, RarArchive, RarError};

use crate::error::{Error, LimitKind, Result};
use crate::extract::{Session, Summary};
use crate::format::{ArchiveFormat, EntryInfo, EntryKind, Format, ReadSeek, WriteSeek};
use crate::formats::stream_len;
use crate::options::Options;
use crate::sources::SourceStream;

pub(crate) struct RarFormat;

fn convert(e: RarError) -> Error {
    match e {
        RarError::Io(e) => e.into(),
        RarError::Encrypted => Error::EncryptedUnsupported(Format::Rar),
        RarError::MultiVolume => Error::MultiVolumeUnsupported(Format::Rar),
        RarError::Unsupported(m) => Error::UnsupportedFormat(format!("RAR: {m}")),
        RarError::NotRar => Error::InvalidArchive("not a RAR archive".into()),
        other => Error::InvalidArchive(other.to_string()),
    }
}

/// RAR has no password support; refusing is clearer than ignoring one.
fn refuse_password(opts: &Options) -> Result<()> {
    if opts.password.is_some() {
        return Err(Error::InvalidOption(
            "RAR passwords are not supported".into(),
        ));
    }
    Ok(())
}

fn kind_of(k: RarKind) -> EntryKind {
    match k {
        RarKind::File => EntryKind::File,
        RarKind::Dir => EntryKind::Dir,
        RarKind::Symlink => EntryKind::Symlink,
        RarKind::Hardlink => EntryKind::Hardlink,
        RarKind::Other => EntryKind::Other,
    }
}

fn mtime_of(secs: Option<i64>) -> Option<SystemTime> {
    let secs = secs?;
    let d = Duration::from_secs(secs.unsigned_abs());
    if secs >= 0 {
        UNIX_EPOCH.checked_add(d)
    } else {
        UNIX_EPOCH.checked_sub(d)
    }
}

fn describe(e: &Entry) -> EntryInfo {
    EntryInfo {
        name: e.name.clone(),
        kind: kind_of(e.kind),
        size: e.size.unwrap_or(0),
        compressed_size: None,
        mode: e.mode.map(|m| m & 0o7777),
        mtime: mtime_of(e.mtime),
        link_target: e.link_target.clone(),
    }
}

/// The size and ratio limits, applied to what the header declares before reading the file.
/// Bytes actually written are checked again by the session, since a header can lie.
fn check_declared(size: Option<u64>, opts: &Options, input_len: u64) -> Result<()> {
    let Some(size) = size else { return Ok(()) };
    let limits = &opts.limits;
    if size > limits.max_total_bytes {
        return Err(Error::LimitExceeded(LimitKind::TotalSize));
    }
    if limits.max_ratio > 0
        && size > limits.ratio_min_bytes
        && size / input_len.max(1) > limits.max_ratio
    {
        return Err(Error::LimitExceeded(LimitKind::Ratio));
    }
    Ok(())
}

/// The data of the current entry as a `Read`. A RAR error is parked here so that it can be
/// returned as itself, not as the `io::Error` the copy loop saw.
struct EntryData<'a, R: Read + Seek> {
    archive: &'a mut RarArchive<R>,
    error: Option<RarError>,
}

impl<R: Read + Seek> Read for EntryData<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.archive.read_data(buf).map_err(|e| {
            self.error = Some(e);
            io::Error::other("RAR data error")
        })
    }
}

impl ArchiveFormat for RarFormat {
    fn format(&self) -> Format {
        Format::Rar
    }

    fn list(
        &self,
        input: &mut dyn ReadSeek,
        opts: &Options,
        visit: &mut dyn FnMut(EntryInfo),
    ) -> Result<()> {
        refuse_password(opts)?;
        let mut archive = RarArchive::open(input).map_err(convert)?;
        let mut count = 0u64;
        while let Some(entry) = archive.next_entry().map_err(convert)? {
            opts.check_cancel()?;
            count += 1;
            if count > opts.limits.max_entries {
                return Err(Error::LimitExceeded(LimitKind::Entries));
            }
            visit(describe(&entry));
        }
        Ok(())
    }

    fn extract(&self, input: &mut dyn ReadSeek, dest: &Path, opts: &Options) -> Result<Summary> {
        refuse_password(opts)?;
        let input_len = stream_len(input)?;
        let mut archive = RarArchive::open(input).map_err(convert)?;
        let session = Session::new(dest, opts, input_len, None, None)?;
        let mut buf = vec![0u8; opts.io_buffer()];

        while let Some(entry) = archive.next_entry().map_err(convert)? {
            session.begin_entry()?;
            let rel = session.rel_path(entry.name.as_bytes())?;
            match entry.kind {
                RarKind::Dir => session.make_dir(&rel)?,
                RarKind::File => {
                    check_declared(entry.size, opts, input_len)?;
                    let mut data = EntryData {
                        archive: &mut archive,
                        error: None,
                    };
                    let mode = entry.mode.map(|m| m & 0o7777);
                    let written = session.write_file(
                        &rel,
                        &entry.name,
                        &mut data,
                        mode,
                        mtime_of(entry.mtime),
                        &mut buf,
                    );
                    if let Some(e) = data.error.take() {
                        return Err(convert(e));
                    }
                    written?;
                }
                RarKind::Symlink => match entry.link_target {
                    Some(target) => session.defer_symlink(rel, target.into_bytes(), &entry.name)?,
                    None => session.skip_entry(),
                },
                RarKind::Hardlink | RarKind::Other => session.skip_entry(),
            }
        }
        session.finish()
    }

    fn create_stream(
        &self,
        _sources: &mut dyn SourceStream,
        _out: &mut dyn WriteSeek,
        _opts: &Options,
    ) -> Result<()> {
        Err(Error::UnsupportedFormat(
            "RAR can only be extracted, never created".into(),
        ))
    }
}
