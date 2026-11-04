//! tar, optionally wrapped in a streaming compressor (see [`Codec`]).
//!
//! Everything is streamed through fixed buffers: memory use does not depend on archive or
//! entry size. tar itself is sequential, so only zstd and xz compression use extra threads.

use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tar::{EntryType, Header};

use crate::error::{Error, Result};
use crate::extract::{Session, Summary};
use crate::format::{ArchiveFormat, EntryInfo, EntryKind, Format, ReadSeek, WriteSeek};
use crate::formats::codec::Codec;
use crate::formats::stream_len;
use crate::options::Options;
use crate::progress::Tracker;
use crate::sources::SourceEntry;
use crate::stats::Source;

const BUF: usize = 64 * 1024;
/// Longest hard-link or symlink target we accept.
const MAX_LINK_TARGET: usize = 4096;

pub(crate) struct TarFormat {
    /// `None` is plain, uncompressed tar.
    codec: Option<Codec>,
    format: Format,
}

impl TarFormat {
    pub(crate) const PLAIN: TarFormat = TarFormat {
        codec: None,
        format: Format::Tar,
    };
    #[cfg(feature = "gzip")]
    pub(crate) const GZIP: TarFormat = TarFormat {
        codec: Some(Codec::Gzip),
        format: Format::TarGz,
    };
    #[cfg(feature = "zstd")]
    pub(crate) const ZSTD: TarFormat = TarFormat {
        codec: Some(Codec::Zstd),
        format: Format::TarZst,
    };

    #[cfg(feature = "bzip2")]
    pub(crate) const BZIP2: TarFormat = TarFormat {
        codec: Some(Codec::Bzip2),
        format: Format::TarBz2,
    };
    #[cfg(feature = "xz")]
    pub(crate) const XZ: TarFormat = TarFormat {
        codec: Some(Codec::Xz),
        format: Format::TarXz,
    };
    #[cfg(feature = "lz4")]
    pub(crate) const LZ4: TarFormat = TarFormat {
        codec: Some(Codec::Lz4),
        format: Format::TarLz4,
    };

    /// Runs `f` on the decompressed byte stream.
    fn with_reader<T>(
        &self,
        input: &mut dyn ReadSeek,
        max_memory: u64,
        f: impl FnOnce(&mut dyn Read) -> Result<T>,
    ) -> Result<T> {
        match self.codec {
            None => f(&mut BufReader::with_capacity(BUF, input)),
            Some(codec) => codec.with_reader(input, max_memory, f),
        }
    }
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn entry_kind(ty: EntryType) -> EntryKind {
    match ty {
        EntryType::Regular | EntryType::Continuous | EntryType::GNUSparse => EntryKind::File,
        EntryType::Directory => EntryKind::Dir,
        EntryType::Symlink => EntryKind::Symlink,
        EntryType::Link => EntryKind::Hardlink,
        _ => EntryKind::Other,
    }
}

fn mtime_of(header: &Header) -> Option<SystemTime> {
    header
        .mtime()
        .ok()
        .filter(|&t| t > 0)
        .map(|t| UNIX_EPOCH + Duration::from_secs(t))
}

fn describe<R: Read>(entry: &tar::Entry<'_, R>) -> EntryInfo {
    let header = entry.header();
    EntryInfo {
        name: lossy(&entry.path_bytes()),
        kind: entry_kind(header.entry_type()),
        size: entry.size(),
        compressed_size: None,
        mode: header.mode().ok().map(|m| m & 0o7777),
        mtime: mtime_of(header),
        link_target: entry.link_name_bytes().map(|b| lossy(&b)),
    }
}

/// Fails with `UnexpectedEof` if the stream ends before the size promised by the header,
/// so a truncated archive can never produce a short file that gets renamed into place.
struct Exact<R> {
    inner: R,
    left: u64,
}

impl<R: Read> Read for Exact<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 {
            return Ok(0);
        }
        let max = buf
            .len()
            .min(usize::try_from(self.left).unwrap_or(usize::MAX));
        let n = self.inner.read(&mut buf[..max])?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated tar entry",
            ));
        }
        self.left -= n as u64;
        Ok(n)
    }
}

/// A file reader that honours cancellation and reports progress.
struct Counting<'a, 'o> {
    file: Source,
    opts: &'a Options,
    tracker: &'a Tracker<'o>,
    name: &'a str,
}

impl Read for Counting<'_, '_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.opts.check_cancel().is_err() {
            return Err(io::Error::other("cancelled"));
        }
        let n = self.file.read(buf)?;
        if n > 0 {
            self.tracker.add_bytes(n as u64, self.name);
        }
        Ok(n)
    }
}

fn write_tar<W: Write>(w: W, sources: &[SourceEntry], opts: &Options) -> Result<W> {
    let total = sources.iter().map(|s| s.size).sum();
    let tracker = Tracker::new(opts, Some(total), Some(sources.len() as u64));
    let mut builder = tar::Builder::new(BufWriter::with_capacity(BUF, w));
    for s in sources {
        opts.check_cancel()?;
        let mut header = Header::new_gnu();
        header.set_mtime(
            s.mtime
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs()),
        );
        match s.kind {
            EntryKind::File => {
                header.set_entry_type(EntryType::Regular);
                header.set_mode(s.mode.unwrap_or(0o644));
                header.set_size(s.size);
                let reader = Counting {
                    file: opts.open_source(&s.path)?,
                    opts,
                    tracker: &tracker,
                    name: &s.name,
                };
                builder.append_data(&mut header, &s.name, reader)?;
            }
            EntryKind::Dir => {
                header.set_entry_type(EntryType::Directory);
                header.set_mode(s.mode.unwrap_or(0o755));
                header.set_size(0);
                builder.append_data(&mut header, format!("{}/", s.name), io::empty())?;
            }
            EntryKind::Symlink => {
                header.set_entry_type(EntryType::Symlink);
                header.set_mode(s.mode.unwrap_or(0o777));
                header.set_size(0);
                builder.append_link(
                    &mut header,
                    &s.name,
                    s.link_target.as_deref().unwrap_or_default(),
                )?;
            }
            _ => continue,
        }
        tracker.entry_done(&s.name);
    }
    let buffered = builder.into_inner()?;
    buffered.into_inner().map_err(|e| Error::Io(e.into_error()))
}

impl ArchiveFormat for TarFormat {
    fn format(&self) -> Format {
        self.format
    }

    fn list(
        &self,
        input: &mut dyn ReadSeek,
        opts: &Options,
        visit: &mut dyn FnMut(EntryInfo),
    ) -> Result<()> {
        super::no_password(opts, self.format)?;
        self.with_reader(input, opts.limits.max_memory_bytes, |reader| {
            let mut archive = tar::Archive::new(reader);
            for entry in archive.entries()? {
                opts.check_cancel()?;
                visit(describe(&entry?));
            }
            Ok(())
        })
    }

    fn extract(&self, input: &mut dyn ReadSeek, dest: &Path, opts: &Options) -> Result<Summary> {
        super::no_password(opts, self.format)?;
        let input_len = stream_len(input)?;
        self.with_reader(input, opts.limits.max_memory_bytes, |reader| {
            let session = Session::new(dest, opts, input_len, None, None)?;
            let mut archive = tar::Archive::new(reader);
            let mut buf = vec![0u8; opts.io_buffer()];
            for entry in archive.entries()? {
                let mut entry = entry?;
                let ty = entry.header().entry_type();
                if matches!(ty, EntryType::XGlobalHeader | EntryType::XHeader) {
                    continue;
                }
                session.begin_entry()?;
                let name = lossy(&entry.path_bytes());
                let rel = session.rel_path(&entry.path_bytes())?;
                match entry_kind(ty) {
                    EntryKind::Dir => session.make_dir(&rel)?,
                    EntryKind::File => {
                        let mode = entry.header().mode().ok();
                        let mtime = mtime_of(entry.header());
                        let left = entry.size();
                        let mut exact = Exact {
                            inner: &mut entry,
                            left,
                        };
                        session.write_file(&rel, &name, &mut exact, mode, mtime, &mut buf)?;
                    }
                    kind @ (EntryKind::Symlink | EntryKind::Hardlink) => {
                        let raw = entry
                            .link_name_bytes()
                            .map(|b| b.into_owned())
                            .unwrap_or_default();
                        if raw.len() > MAX_LINK_TARGET {
                            return Err(Error::UnsafeLink {
                                name,
                                target: "<too long>".into(),
                            });
                        }
                        if kind == EntryKind::Symlink {
                            session.defer_symlink(rel, raw, &name)?;
                        } else {
                            let target = session.rel_path(&raw)?;
                            session.hard_link(&rel, &target, &name, &lossy(&raw))?;
                        }
                    }
                    EntryKind::Other => session.skip_entry(),
                }
            }
            session.finish()
        })
    }

    fn create(
        &self,
        sources: &[SourceEntry],
        out: &mut dyn WriteSeek,
        opts: &Options,
    ) -> Result<()> {
        super::no_password(opts, self.format)?;
        let result = match self.codec {
            None if opts.level.is_some() => Err(Error::InvalidOption(
                "plain tar has no compression level".into(),
            )),
            None => write_tar(out, sources, opts).map(drop),
            Some(codec) => {
                let level = codec.level(opts.level)?;
                codec.with_writer(out, level, opts.workers(), |w| {
                    write_tar(w, sources, opts).map(drop)
                })
            }
        };
        // A cancelled read surfaces as an I/O error from deep inside the tar builder.
        result.map_err(|e| {
            if opts.control.is_cancelled() {
                Error::Cancelled
            } else {
                e
            }
        })
    }
}
