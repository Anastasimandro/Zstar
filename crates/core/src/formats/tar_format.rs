//! tar, optionally wrapped in a streaming compressor (gzip, zstd).
//!
//! Everything is streamed through fixed buffers: memory use does not depend on archive or
//! entry size. tar itself is sequential, so only zstd compression uses extra threads.

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tar::{EntryType, Header};

use crate::error::{Error, Result};
use crate::extract::{CHUNK, Session, Summary};
use crate::format::{ArchiveFormat, EntryInfo, EntryKind, Format, ReadSeek, WriteSeek};
use crate::formats::stream_len;
use crate::options::Options;
use crate::progress::Tracker;
use crate::sources::SourceEntry;

const BUF: usize = 64 * 1024;
/// Longest hard-link or symlink target we accept.
const MAX_LINK_TARGET: usize = 4096;

#[derive(Clone, Copy)]
enum Codec {
    None,
    #[cfg(feature = "gzip")]
    Gzip,
    #[cfg(feature = "zstd")]
    Zstd,
}

pub(crate) struct TarFormat {
    codec: Codec,
    format: Format,
}

impl TarFormat {
    pub(crate) const PLAIN: TarFormat = TarFormat {
        codec: Codec::None,
        format: Format::Tar,
    };
    #[cfg(feature = "gzip")]
    pub(crate) const GZIP: TarFormat = TarFormat {
        codec: Codec::Gzip,
        format: Format::TarGz,
    };
    #[cfg(feature = "zstd")]
    pub(crate) const ZSTD: TarFormat = TarFormat {
        codec: Codec::Zstd,
        format: Format::TarZst,
    };

    /// Runs `f` on the decompressed byte stream.
    fn with_reader<T>(
        &self,
        input: &mut dyn ReadSeek,
        f: impl FnOnce(&mut dyn Read) -> Result<T>,
    ) -> Result<T> {
        match self.codec {
            Codec::None => f(&mut BufReader::with_capacity(BUF, input)),
            #[cfg(feature = "gzip")]
            Codec::Gzip => {
                let buffered = BufReader::with_capacity(BUF, input);
                f(&mut flate2::bufread::MultiGzDecoder::new(buffered))
            }
            #[cfg(feature = "zstd")]
            Codec::Zstd => f(&mut zstd::stream::read::Decoder::new(input)?),
        }
    }

    /// Validates `opts.level` for this codec and returns it, or the codec's default.
    fn level(&self, opts: &Options) -> Result<i32> {
        let (default, range) = match self.codec {
            Codec::None => {
                return match opts.level {
                    None => Ok(0),
                    Some(_) => Err(Error::InvalidOption(
                        "plain tar has no compression level".into(),
                    )),
                };
            }
            #[cfg(feature = "gzip")]
            Codec::Gzip => (6, 0..=9),
            #[cfg(feature = "zstd")]
            Codec::Zstd => (3, 1..=22),
        };
        match opts.level {
            None => Ok(default),
            Some(l) if range.contains(&l) => Ok(l),
            Some(l) => Err(Error::InvalidOption(format!(
                "{} compression level must be {}-{}, got {l}",
                self.format.name(),
                range.start(),
                range.end()
            ))),
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

/// A file reader that honours cancellation and reports progress.
struct Counting<'a, 'o> {
    file: File,
    opts: &'a Options,
    tracker: &'a Tracker<'o>,
    name: &'a str,
}

impl Read for Counting<'_, '_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.opts.cancel.is_cancelled() {
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
                    file: File::open(&s.path)?,
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
        self.with_reader(input, |reader| {
            let mut archive = tar::Archive::new(reader);
            for entry in archive.entries()? {
                opts.check_cancel()?;
                visit(describe(&entry?));
            }
            Ok(())
        })
    }

    fn extract(&self, input: &mut dyn ReadSeek, dest: &Path, opts: &Options) -> Result<Summary> {
        let input_len = stream_len(input)?;
        self.with_reader(input, |reader| {
            let session = Session::new(dest, opts, input_len, None, None)?;
            let mut archive = tar::Archive::new(reader);
            let mut buf = vec![0u8; CHUNK];
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
                        session.write_file(&rel, &name, &mut entry, mode, mtime, &mut buf)?;
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
        let level = self.level(opts)?;
        let result = match self.codec {
            Codec::None => write_tar(out, sources, opts).map(drop),
            #[cfg(feature = "gzip")]
            Codec::Gzip => {
                let encoder =
                    flate2::write::GzEncoder::new(out, flate2::Compression::new(level as u32));
                write_tar(encoder, sources, opts).and_then(|e| Ok(e.finish().map(drop)?))
            }
            #[cfg(feature = "zstd")]
            Codec::Zstd => (|| {
                let mut encoder = zstd::stream::write::Encoder::new(out, level)?;
                encoder.include_checksum(true)?;
                let threads = opts.effective_threads();
                if threads > 1 {
                    encoder.multithread(threads as u32)?;
                }
                let encoder = write_tar(encoder, sources, opts)?;
                encoder.finish()?;
                Ok(())
            })(),
        };
        // A cancelled read surfaces as an I/O error from deep inside the tar builder.
        result.map_err(|e| {
            if opts.cancel.is_cancelled() {
                Error::Cancelled
            } else {
                e
            }
        })
    }
}
