//! 7z, through `sevenz-rust2`.
//!
//! Files are stored in solid LZMA2 blocks (so similar files compress together, as 7-Zip does)
//! and read lazily, so memory depends on the dictionary and thread count, never on file
//! sizes or counts. Unix permissions and symlinks use the p7zip convention: the high 16 bits
//! of the attributes hold `st_mode`, flagged by `0x8000`.
//!
//! Methods supported: LZMA, LZMA2, bzip2, Copy and the BCJ/Delta filters. PPMd and Deflate
//! are not built in, and an archive that needs them fails with a clear error.

use std::io::{self, Cursor, Read};
use std::path::Path;
use std::time::SystemTime;

use sevenz_rust2::encoder_options::{EncoderOptions, Lzma2Options};
use sevenz_rust2::{
    ArchiveEntry, ArchiveReader, ArchiveWriter, EncoderConfiguration, EncoderMethod, NtTime,
    Password as SevenZPassword, SourceReader,
};

use crate::error::{Error, LimitKind, Result};
use crate::extract::{Session, Summary};
use crate::format::{ArchiveFormat, EntryInfo, EntryKind, Format, ReadSeek, WriteSeek};
use crate::formats::stream_len;
use crate::options::Options;
use crate::progress::Tracker;
use crate::sources::SourceEntry;
use crate::stats::Source;

pub(crate) struct SevenZFormat;

const ATTR_DIRECTORY: u32 = 0x10;
const ATTR_ARCHIVE: u32 = 0x20;
/// Marks that the high 16 bits of the attributes are a Unix `st_mode`.
const ATTR_UNIX_EXTENSION: u32 = 0x8000;
const S_IFMT: u32 = 0o170_000;
const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFLNK: u32 = 0o120_000;
/// Longest symlink target we accept.
const MAX_LINK_TARGET: u64 = 4096;
/// A solid block ends after this many uncompressed bytes or files.
const SOLID_BLOCK_BYTES: u64 = 256 << 20;
const SOLID_BLOCK_FILES: usize = 50_000;
/// Independent chunk size of multithreaded LZMA2 (clamped up to the dictionary size).
const MT_CHUNK: u64 = 32 << 20;

const LZMA_ID: &[u8] = &[0x03, 0x01, 0x01];
const LZMA2_ID: &[u8] = &[0x21];

fn convert(e: sevenz_rust2::Error) -> Error {
    use sevenz_rust2::Error as E;
    match e {
        E::Io(e, _) | E::FileOpen(e, _) => e.into(),
        E::MaxMemLimited { .. } => Error::LimitExceeded(LimitKind::Memory),
        E::PasswordRequired => Error::PasswordRequired,
        E::MaybeBadPassword(_) => Error::WrongPassword,
        E::UnsupportedCompressionMethod(m) => {
            Error::UnsupportedFormat(format!("7z compression method {m} is not supported"))
        }
        other => Error::InvalidArchive(other.to_string()),
    }
}

/// Dictionary size a coder asks its decoder to allocate, for the methods where the stream
/// chooses it.
fn dictionary_size(id: &[u8], props: &[u8]) -> Option<u64> {
    if id == LZMA_ID && props.len() >= 5 {
        Some(u64::from(u32::from_le_bytes([
            props[1], props[2], props[3], props[4],
        ])))
    } else if id == LZMA2_ID && !props.is_empty() {
        let b = u32::from(props[0]);
        Some(match b {
            0..=39 => u64::from(2 | (b & 1)) << (b / 2 + 11),
            _ => u64::from(u32::MAX),
        })
    } else {
        None
    }
}

/// With a password set, a stream that fails to decode means the key was wrong far more often
/// than the data was damaged (7-Zip reports it the same way). Real I/O errors stay as they are.
fn wrong_password_if_decoding(e: Error, opts: &Options) -> Error {
    match e {
        Error::Io(ref io)
            if opts.password.is_some()
                && matches!(
                    io.kind(),
                    io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof
                ) =>
        {
            Error::WrongPassword
        }
        other => other,
    }
}

fn mode_of(entry: &ArchiveEntry) -> Option<u32> {
    let attrs = entry.windows_attributes();
    (entry.has_windows_attributes && attrs & ATTR_UNIX_EXTENSION != 0).then_some(attrs >> 16)
}

fn kind_of(entry: &ArchiveEntry) -> EntryKind {
    if entry.is_directory() {
        return EntryKind::Dir;
    }
    match mode_of(entry).map(|m| m & S_IFMT) {
        None | Some(S_IFREG) => EntryKind::File,
        Some(S_IFLNK) => EntryKind::Symlink,
        Some(S_IFDIR) => EntryKind::Dir,
        Some(_) => EntryKind::Other,
    }
}

fn mtime_of(entry: &ArchiveEntry) -> Option<SystemTime> {
    entry
        .has_last_modified_date
        .then(|| SystemTime::from(entry.last_modified_date()))
}

/// Opens the archive and checks everything that can be checked before decoding a byte.
fn open<'a>(
    input: &'a mut dyn ReadSeek,
    opts: &Options,
) -> Result<ArchiveReader<&'a mut dyn ReadSeek>> {
    let secret = match opts.secret()? {
        Some(p) => SevenZPassword::new(p),
        None => SevenZPassword::empty(),
    };
    let mut reader = ArchiveReader::new(input, secret).map_err(convert)?;
    reader.set_thread_count(opts.effective_threads() as u32);
    let archive = reader.archive();
    if archive.files.len() as u64 > opts.limits.max_entries {
        return Err(Error::LimitExceeded(LimitKind::Entries));
    }
    for block in &archive.blocks {
        for coder in &block.coders {
            let dict = dictionary_size(coder.encoder_method_id(), coder.properties());
            if dict.is_some_and(|d| d > opts.limits.max_memory_bytes) {
                return Err(Error::LimitExceeded(LimitKind::Memory));
            }
        }
    }
    Ok(reader)
}

fn describe(entry: &ArchiveEntry) -> EntryInfo {
    EntryInfo {
        name: entry.name().to_owned(),
        kind: kind_of(entry),
        size: entry.size(),
        compressed_size: None,
        mode: mode_of(entry).map(|m| m & 0o7777),
        mtime: mtime_of(entry),
        link_target: None,
    }
}

/// A source file that is opened on first read and closed at its end, so a solid block of
/// thousands of files never holds thousands of file descriptors. Honours cancellation and
/// reports progress.
struct LazyFile<'a, 'o> {
    path: &'a Path,
    name: &'a str,
    file: Option<Source>,
    done: bool,
    opts: &'a Options,
    tracker: &'a Tracker<'o>,
}

impl Read for LazyFile<'_, '_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.opts.check_cancel().is_err() {
            return Err(io::Error::other("cancelled"));
        }
        if self.done {
            return Ok(0);
        }
        if self.file.is_none() {
            self.file = Some(self.opts.open_source(self.path)?);
        }
        let n = self.file.as_mut().map_or(Ok(0), |f| f.read(buf))?;
        if n == 0 {
            self.file = None;
            self.done = true;
        } else {
            self.tracker.add_bytes(n as u64, self.name);
        }
        Ok(n)
    }
}

fn entry_for(s: &SourceEntry) -> ArchiveEntry {
    let (dos, format_bits, default_mode) = match s.kind {
        EntryKind::Dir => (ATTR_DIRECTORY, S_IFDIR, 0o755),
        EntryKind::Symlink => (ATTR_ARCHIVE, S_IFLNK, 0o777),
        _ => (ATTR_ARCHIVE, S_IFREG, 0o644),
    };
    let mode = s.mode.unwrap_or(default_mode) & 0o7777;
    let mut entry = ArchiveEntry {
        name: s.name.clone(),
        is_directory: s.kind == EntryKind::Dir,
        has_stream: false,
        has_windows_attributes: true,
        windows_attributes: dos | ATTR_UNIX_EXTENSION | ((format_bits | mode) << 16),
        ..ArchiveEntry::default()
    };
    if let Some(t) = s.mtime
        && let Ok(nt) = NtTime::try_from(t)
    {
        entry.has_last_modified_date = true;
        entry.last_modified_date = nt;
    }
    entry
}

/// Reads a symlink target (the data of a symlink entry), bounded.
fn read_target(data: &mut dyn Read, name: &str) -> Result<Vec<u8>> {
    let mut target = Vec::new();
    data.take(MAX_LINK_TARGET + 1).read_to_end(&mut target)?;
    if target.len() as u64 > MAX_LINK_TARGET {
        return Err(Error::UnsafeLink {
            name: name.into(),
            target: "<too long>".into(),
        });
    }
    Ok(target)
}

impl ArchiveFormat for SevenZFormat {
    fn format(&self) -> Format {
        Format::SevenZ
    }

    fn list(
        &self,
        input: &mut dyn ReadSeek,
        opts: &Options,
        visit: &mut dyn FnMut(EntryInfo),
    ) -> Result<()> {
        let reader = open(input, opts)?;
        for entry in &reader.archive().files {
            opts.check_cancel()?;
            visit(describe(entry));
        }
        Ok(())
    }

    fn extract(&self, input: &mut dyn ReadSeek, dest: &Path, opts: &Options) -> Result<Summary> {
        let input_len = stream_len(input)?;
        let mut reader = open(input, opts)?;
        let total: u64 = reader.archive().files.iter().map(|f| f.size()).sum();
        let count = reader.archive().files.len() as u64;
        let session = Session::new(dest, opts, input_len, Some(total), Some(count))?;
        let mut buf = vec![0u8; opts.io_buffer()];

        let mut failure: Option<Error> = None;
        let result = reader.for_each_entries(|entry, data| {
            let step = (|| -> Result<()> {
                session.begin_entry()?;
                if entry.is_anti_item() {
                    session.skip_entry();
                    return Ok(());
                }
                let name = entry.name();
                let rel = session.rel_path(name.as_bytes())?;
                match kind_of(entry) {
                    EntryKind::Dir => session.make_dir(&rel)?,
                    EntryKind::File => {
                        let mode = mode_of(entry).map(|m| m & 0o7777);
                        session.write_file(&rel, name, data, mode, mtime_of(entry), &mut buf)?;
                    }
                    EntryKind::Symlink => {
                        let target = read_target(data, name)?;
                        session.defer_symlink(rel, target, name)?;
                    }
                    EntryKind::Hardlink | EntryKind::Other => session.skip_entry(),
                }
                Ok(())
            })();
            match step {
                Ok(()) => Ok(true),
                Err(e) => {
                    failure = Some(e);
                    Err(sevenz_rust2::Error::Other("stopped".into()))
                }
            }
        });
        if let Some(e) = failure {
            return Err(wrong_password_if_decoding(e, opts));
        }
        result.map_err(convert)?;
        session.finish()
    }

    fn create(
        &self,
        sources: &[SourceEntry],
        out: &mut dyn WriteSeek,
        opts: &Options,
    ) -> Result<()> {
        let level = match opts.level {
            None => 6,
            Some(l) if (0..=9).contains(&l) => l,
            Some(l) => {
                return Err(Error::InvalidOption(format!(
                    "7z compression level must be 0-9, got {l}"
                )));
            }
        };
        let secret = opts.secret()?;
        #[cfg(not(feature = "sevenz-aes"))]
        if secret.is_some() {
            return Err(Error::InvalidOption(
                "7z encryption is not enabled in this build (feature sevenz-aes)".into(),
            ));
        }
        let threads = opts.effective_threads() as u32;
        let lzma2 = if threads > 1 {
            Lzma2Options::from_level_mt(level as u32, threads, MT_CHUNK)
        } else {
            Lzma2Options::from_level(level as u32)
        };

        let total = sources.iter().map(|s| s.size).sum();
        let tracker = Tracker::new(opts, Some(total), Some(sources.len() as u64));
        let result = (|| -> Result<()> {
            let mut writer = ArchiveWriter::new(out).map_err(convert)?;
            let mut methods = Vec::with_capacity(2);
            // Encryption comes first in the list: 7z applies the coders from the last to the
            // first when writing, so data is compressed and then encrypted. The header is
            // encrypted too (the library default), which hides the file names.
            #[cfg(feature = "sevenz-aes")]
            if let Some(p) = secret {
                methods.push(
                    sevenz_rust2::encoder_options::AesEncoderOptions::new(SevenZPassword::new(p))
                        .into(),
                );
            }
            methods.push(
                EncoderConfiguration::new(EncoderMethod::LZMA2)
                    .with_options(EncoderOptions::Lzma2(lzma2)),
            );
            writer.set_content_methods(methods);

            // Directories and empty files carry no data: they go first, then the solid blocks.
            for s in sources {
                opts.check_cancel()?;
                let empty_file = s.kind == EntryKind::File && s.size == 0;
                if s.kind == EntryKind::Dir || empty_file {
                    writer
                        .push_archive_entry(entry_for(s), None::<&[u8]>)
                        .map_err(convert)?;
                    tracker.entry_done(&s.name);
                }
            }

            let mut entries: Vec<ArchiveEntry> = Vec::new();
            let mut readers: Vec<SourceReader<Box<dyn Read + '_>>> = Vec::new();
            let mut bytes = 0u64;
            for s in sources {
                opts.check_cancel()?;
                let reader: Box<dyn Read + '_> = match s.kind {
                    EntryKind::File if s.size > 0 => Box::new(LazyFile {
                        path: &s.path,
                        name: &s.name,
                        file: None,
                        done: false,
                        opts,
                        tracker: &tracker,
                    }),
                    EntryKind::Symlink => Box::new(Cursor::new(
                        s.link_target.clone().unwrap_or_default().into_bytes(),
                    )),
                    _ => continue,
                };
                let mut entry = entry_for(s);
                entry.has_stream = true;
                bytes += s.size.max(1);
                entries.push(entry);
                readers.push(SourceReader::new(reader));
                if bytes >= SOLID_BLOCK_BYTES || entries.len() >= SOLID_BLOCK_FILES {
                    writer
                        .push_archive_entries(
                            std::mem::take(&mut entries),
                            std::mem::take(&mut readers),
                        )
                        .map_err(convert)?;
                    bytes = 0;
                }
            }
            if !entries.is_empty() {
                writer
                    .push_archive_entries(entries, readers)
                    .map_err(convert)?;
            }
            writer.finish()?;
            Ok(())
        })();
        result.map_err(|e| {
            if opts.control.is_cancelled() {
                Error::Cancelled
            } else {
                e
            }
        })
    }
}
