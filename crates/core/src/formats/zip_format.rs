//! ZIP: central-directory based, so extraction can run on several threads.
//!
//! Every worker opens its own `ZipArchive` over a shared reader. The reader is locked only
//! for the moment bytes are fetched; inflating and writing happen outside the lock.

use std::collections::HashSet;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::SystemTime;

use zip::read::ZipFile;
use zip::result::ZipResult;
use zip::write::{FileOptions, PreparedZipFile, SimpleFileOptions, ZipFileBuilder};
use zip::{CompressionMethod, DateTime, ZipArchive, ZipWriter};

use crate::control::Workers;
use crate::error::{Error, LimitKind, Result};
use crate::extract::{Session, Summary};
use crate::format::{ArchiveFormat, EntryInfo, EntryKind, Format, ReadSeek, WriteSeek};
use crate::formats::stream_len;
use crate::options::{Options, ZipMethod};
use crate::progress::Tracker;
use crate::sources::SourceEntry;
use crate::timeconv::{Civil, civil_to_system, system_to_civil};

const BUF: usize = 64 * 1024;
/// Files up to this size are compressed whole, in memory, on worker threads.
const SMALL_FILE: u64 = 1 << 20;
/// Longest symlink target we accept.
const MAX_LINK_TARGET: u64 = 4096;

const S_IFMT: u32 = 0o170_000;
const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFLNK: u32 = 0o120_000;

pub(crate) struct ZipFormat;

/// A reader shared by several `ZipArchive`s. Each handle keeps its own position and seeks
/// the shared stream to it before every read.
struct Shared<'a, 'b> {
    inner: &'a Mutex<&'b mut (dyn ReadSeek + 'b)>,
    pos: u64,
}

impl<'a, 'b> Shared<'a, 'b> {
    fn lock(&self) -> std::sync::MutexGuard<'a, &'b mut (dyn ReadSeek + 'b)> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Read for Shared<'_, '_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut inner = self.lock();
        inner.seek(SeekFrom::Start(self.pos))?;
        let n = inner.read(buf)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for Shared<'_, '_> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.pos = match from {
            SeekFrom::Start(p) => p,
            SeekFrom::Current(d) => self.pos.checked_add_signed(d).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "seek before start of stream")
            })?,
            SeekFrom::End(d) => self.lock().seek(SeekFrom::End(d))?,
        };
        Ok(self.pos)
    }
}

type SharedReader<'a, 'b> = BufReader<Shared<'a, 'b>>;

fn open<'a, 'b>(
    shared: &'a Mutex<&'b mut (dyn ReadSeek + 'b)>,
) -> Result<ZipArchive<SharedReader<'a, 'b>>> {
    let reader = BufReader::with_capacity(
        BUF,
        Shared {
            inner: shared,
            pos: 0,
        },
    );
    Ok(ZipArchive::new(reader)?)
}

fn kind_of(is_dir: bool, mode: Option<u32>) -> EntryKind {
    if is_dir {
        return EntryKind::Dir;
    }
    match mode.map(|m| m & S_IFMT) {
        None | Some(0 | S_IFREG) => EntryKind::File,
        Some(S_IFDIR) => EntryKind::Dir,
        Some(S_IFLNK) => EntryKind::Symlink,
        Some(_) => EntryKind::Other,
    }
}

fn to_system_time(dt: Option<DateTime>) -> Option<SystemTime> {
    let dt = dt.filter(DateTime::is_valid)?;
    civil_to_system(Civil {
        year: i64::from(dt.year()),
        month: u32::from(dt.month()),
        day: u32::from(dt.day()),
        hour: u32::from(dt.hour()),
        minute: u32::from(dt.minute()),
        second: u32::from(dt.second()),
    })
}

fn to_dos_time(t: Option<SystemTime>) -> Option<DateTime> {
    let c = system_to_civil(t?)?;
    let year = u16::try_from(c.year).ok()?;
    DateTime::from_date_and_time(
        year,
        c.month as u8,
        c.day as u8,
        c.hour as u8,
        c.minute as u8,
        c.second as u8,
    )
    .ok()
}

fn describe(entry: &zip::read::ZipFileEntry<'_>) -> Result<EntryInfo> {
    let mode = entry.unix_mode();
    Ok(EntryInfo {
        name: entry.name()?.into_owned(),
        kind: kind_of(entry.is_dir(), mode),
        size: entry.size(),
        compressed_size: Some(entry.compressed_size()),
        mode: mode.map(|m| m & 0o7777),
        mtime: to_system_time(entry.last_modified()),
        link_target: None,
    })
}

struct FileItem {
    index: usize,
    rel: PathBuf,
    name: String,
    mode: Option<u32>,
    mtime: Option<SystemTime>,
}

struct LinkItem {
    index: usize,
    rel: PathBuf,
    name: String,
}

/// Everything extraction will do, decided before anything is written.
struct Plan {
    dirs: Vec<PathBuf>,
    files: Vec<FileItem>,
    links: Vec<LinkItem>,
    total_bytes: u64,
    entries: u64,
    /// Two names differ only by case: on a case-insensitive file system they would collide,
    /// so extraction must not run in parallel.
    case_collision: bool,
}

impl Plan {
    fn build<R: Read + Seek>(archive: &ZipArchive<R>, opts: &Options) -> Result<Plan> {
        let entries = archive.len() as u64;
        if entries > opts.limits.max_entries {
            return Err(Error::LimitExceeded(LimitKind::Entries));
        }
        let mut plan = Plan {
            dirs: Vec::new(),
            files: Vec::new(),
            links: Vec::new(),
            total_bytes: 0,
            entries,
            case_collision: false,
        };
        let mut seen = HashSet::new();
        for index in 0..archive.len() {
            opts.check_cancel()?;
            let entry = archive.by_index_data(index)?;
            let info = describe(&entry)?;
            // Zip-slip barrier: one bad name rejects the whole archive before any write.
            let rel = crate::path::sanitize(info.name.as_bytes(), opts.windows_names)?;
            if matches!(info.kind, EntryKind::File | EntryKind::Dir)
                && !seen.insert(rel.to_string_lossy().to_lowercase())
                && !rel.as_os_str().is_empty()
            {
                plan.case_collision = true;
            }
            match info.kind {
                EntryKind::Dir => plan.dirs.push(rel),
                EntryKind::File => {
                    plan.total_bytes = plan.total_bytes.saturating_add(info.size);
                    plan.files.push(FileItem {
                        index,
                        rel,
                        name: info.name,
                        mode: info.mode,
                        mtime: info.mtime,
                    });
                }
                EntryKind::Symlink => plan.links.push(LinkItem {
                    index,
                    rel,
                    name: info.name,
                }),
                _ => {}
            }
        }
        if plan.total_bytes > opts.limits.max_total_bytes {
            return Err(Error::LimitExceeded(LimitKind::TotalSize));
        }
        Ok(plan)
    }
}

/// Opens an entry, decrypting it when a secret is given (it is ignored for plain entries).
fn entry<'a, R: Read + Seek>(
    archive: &'a mut ZipArchive<R>,
    index: usize,
    secret: Option<&str>,
) -> ZipResult<ZipFile<'a, R>> {
    match secret {
        Some(p) => archive.by_index_decrypt(index, p.as_bytes()),
        None => archive.by_index(index),
    }
}

fn worker(
    shared: &Mutex<&mut dyn ReadSeek>,
    files: &[FileItem],
    session: &Session<'_>,
    next: &AtomicUsize,
    secret: Option<&str>,
    workers: &Workers,
    index: usize,
) -> Result<()> {
    let mut archive = open(shared)?;
    let mut buf = vec![0u8; session.io_buffer()];
    loop {
        workers.checkpoint(
            index,
            || session.check_stop(),
            || next.load(Ordering::Relaxed) >= files.len(),
        )?;
        let Some(item) = files.get(next.fetch_add(1, Ordering::Relaxed)) else {
            return Ok(());
        };
        let mut file = entry(&mut archive, item.index, secret)?;
        session.write_file(
            &item.rel, &item.name, &mut file, item.mode, item.mtime, &mut buf,
        )?;
    }
}

fn run_workers(
    shared: &Mutex<&mut dyn ReadSeek>,
    files: &[FileItem],
    session: &Session<'_>,
    threads: usize,
    secret: Option<&str>,
    workers: &Workers,
) -> Result<()> {
    let next = AtomicUsize::new(0);
    let first_error: Mutex<Option<Error>> = Mutex::new(None);
    let work = |index: usize| {
        if let Err(e) = worker(shared, files, session, &next, secret, workers, index) {
            // Record the cause before aborting, so the others' "cancelled" never wins.
            first_error
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get_or_insert(e);
            session.abort();
        }
    };
    if threads <= 1 {
        work(0);
    } else {
        std::thread::scope(|scope| {
            for index in 1..threads {
                scope.spawn(move || work(index));
            }
            work(0);
        });
    }
    match first_error
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner)
    {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// How entries are compressed: validated once, then copied to every worker.
#[derive(Clone, Copy)]
struct Compression {
    method: CompressionMethod,
    level: Option<i64>,
}

fn compression(opts: &Options) -> Result<Compression> {
    let method = opts.zip_method.unwrap_or(ZipMethod::Deflate);
    let range = |lo: i32, hi: i32| match opts.level {
        None => Ok(None),
        Some(l) if (lo..=hi).contains(&l) => Ok(Some(i64::from(l))),
        Some(l) => Err(Error::InvalidOption(format!(
            "zip {} compression level must be {lo}-{hi}, got {l}",
            method.name()
        ))),
    };
    Ok(match method {
        ZipMethod::Stored => {
            if opts.level.is_some() {
                return Err(Error::InvalidOption(
                    "stored zip entries have no compression level".into(),
                ));
            }
            Compression {
                method: CompressionMethod::Stored,
                level: None,
            }
        }
        // Level 0 has always meant "store" for zip.
        ZipMethod::Deflate => match range(0, 9)? {
            Some(0) => Compression {
                method: CompressionMethod::Stored,
                level: None,
            },
            level => Compression {
                method: CompressionMethod::Deflated,
                level,
            },
        },
        #[cfg(feature = "zip-bzip2")]
        ZipMethod::Bzip2 => Compression {
            method: CompressionMethod::Bzip2,
            level: range(1, 9)?,
        },
        #[cfg(feature = "zip-zstd")]
        ZipMethod::Zstd => Compression {
            method: CompressionMethod::Zstd,
            level: range(1, 22)?,
        },
        #[allow(unreachable_patterns)]
        other => {
            return Err(Error::InvalidOption(format!(
                "zip method {} is not enabled in this build",
                other.name()
            )));
        }
    })
}

fn file_options<'k>(
    s: &SourceEntry,
    plan: Compression,
    secret: Option<&'k str>,
) -> FileOptions<'k, 'static, ()> {
    let mut o: FileOptions<'k, 'static, ()> = SimpleFileOptions::default()
        .compression_method(plan.method)
        .compression_level(plan.level)
        .large_file(s.size >= u64::from(u32::MAX));
    #[cfg(feature = "zip-aes")]
    if let Some(p) = secret {
        o = o.with_aes_encryption(zip::AesMode::Aes256, p);
    }
    #[cfg(not(feature = "zip-aes"))]
    let _ = secret;
    if let Some(mode) = s.mode {
        o = o.unix_permissions(mode);
    }
    if let Some(dt) = to_dos_time(s.mtime) {
        o = o.last_modified_time(dt);
    }
    o
}

fn compress_small(
    s: &SourceEntry,
    plan: Compression,
    opts: &Options,
    tracker: &Tracker<'_>,
) -> Result<PreparedZipFile> {
    opts.check_cancel()?;
    let mut data = Vec::with_capacity(s.size as usize);
    opts.open_source(&s.path)?
        .take(SMALL_FILE + 1)
        .read_to_end(&mut data)?;
    if data.len() as u64 > SMALL_FILE {
        return Err(
            io::Error::other(format!("{} grew while being archived", s.path.display())).into(),
        );
    }
    let mut builder = ZipFileBuilder::new(&s.name, file_options(s, plan, None))?;
    builder.write_all(&data)?;
    tracker.add_bytes(data.len() as u64, &s.name);
    Ok(builder.finish()?)
}

struct Creator<'a, 'o> {
    zw: ZipWriter<BufWriter<&'a mut dyn WriteSeek>>,
    opts: &'o Options,
    plan: Compression,
    secret: Option<&'o str>,
    tracker: Tracker<'o>,
    buf: Vec<u8>,
}

impl Creator<'_, '_> {
    fn add_streamed(&mut self, s: &SourceEntry) -> Result<()> {
        self.zw
            .start_file(&s.name, file_options(s, self.plan, self.secret))?;
        let mut file = self.opts.open_source(&s.path)?;
        loop {
            self.opts.check_cancel()?;
            let n = match file.read(&mut self.buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            };
            self.zw.write_all(&self.buf[..n])?;
            self.tracker.add_bytes(n as u64, &s.name);
        }
        Ok(())
    }

    fn add_other(&mut self, s: &SourceEntry) -> Result<()> {
        // Directories and link targets carry no file data; they stay unencrypted.
        let options = file_options(s, self.plan, None);
        match s.kind {
            EntryKind::Dir => self.zw.add_directory(&s.name, options)?,
            EntryKind::Symlink => {
                let target = s.link_target.as_deref().unwrap_or_default();
                self.zw.add_symlink(&s.name, target, options)?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Compresses a run of small files on `threads` workers, then appends them in order.
    fn flush_batch(&mut self, batch: &mut Vec<&SourceEntry>, threads: usize) -> Result<()> {
        let slots: Vec<Mutex<Option<Result<PreparedZipFile>>>> =
            batch.iter().map(|_| Mutex::new(None)).collect();
        let next = AtomicUsize::new(0);
        let (opts, plan, tracker) = (self.opts, self.plan, &self.tracker);
        let workers = opts.workers();
        std::thread::scope(|scope| {
            for _ in 0..threads.min(batch.len()) {
                scope.spawn(|| {
                    workers.sync_priority();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&s) = batch.get(i) else { break };
                        let done = compress_small(s, plan, opts, tracker);
                        *slots[i].lock().unwrap_or_else(PoisonError::into_inner) = Some(done);
                    }
                });
            }
        });
        for (s, slot) in batch.drain(..).zip(slots) {
            let prepared = slot
                .into_inner()
                .unwrap_or_else(PoisonError::into_inner)
                .unwrap_or(Err(Error::Cancelled))?;
            self.zw.add_prepared_file(prepared)?;
            self.tracker.entry_done(&s.name);
        }
        Ok(())
    }
}

impl ArchiveFormat for ZipFormat {
    fn format(&self) -> Format {
        Format::Zip
    }

    fn list(
        &self,
        input: &mut dyn ReadSeek,
        opts: &Options,
        visit: &mut dyn FnMut(EntryInfo),
    ) -> Result<()> {
        let archive = ZipArchive::new(BufReader::with_capacity(BUF, input))?;
        for index in 0..archive.len() {
            opts.check_cancel()?;
            visit(describe(&archive.by_index_data(index)?)?);
        }
        Ok(())
    }

    fn extract(&self, input: &mut dyn ReadSeek, dest: &Path, opts: &Options) -> Result<Summary> {
        let secret = opts.secret()?;
        let input_len = stream_len(input)?;
        let shared = Mutex::new(input);
        let mut archive = open(&shared)?;
        if archive.has_duplicate_names() {
            return Err(Error::InvalidArchive("duplicate entry names".into()));
        }
        let plan = Plan::build(&archive, opts)?;
        let session = Session::new(
            dest,
            opts,
            input_len,
            Some(plan.total_bytes),
            Some(plan.entries),
        )?;

        for rel in &plan.dirs {
            session.make_dir(rel)?;
        }
        let workers = opts.file_workers();
        let threads = if plan.case_collision {
            1
        } else {
            workers.pool.min(plan.files.len()).max(1)
        };
        run_workers(&shared, &plan.files, &session, threads, secret, &workers)?;

        for link in &plan.links {
            let mut target = Vec::new();
            entry(&mut archive, link.index, secret)?
                .take(MAX_LINK_TARGET + 1)
                .read_to_end(&mut target)?;
            if target.len() as u64 > MAX_LINK_TARGET {
                return Err(Error::UnsafeLink {
                    name: link.name.clone(),
                    target: "<too long>".into(),
                });
            }
            session.defer_symlink(link.rel.clone(), target, &link.name)?;
        }
        session.finish()
    }

    fn create(
        &self,
        sources: &[SourceEntry],
        out: &mut dyn WriteSeek,
        opts: &Options,
    ) -> Result<()> {
        let plan = compression(opts)?;
        let secret = opts.secret()?;
        #[cfg(not(feature = "zip-aes"))]
        if secret.is_some() {
            return Err(Error::InvalidOption(
                "zip encryption is not enabled in this build (feature zip-aes)".into(),
            ));
        }
        let total = sources.iter().map(|s| s.size).sum();
        let mut c = Creator {
            zw: ZipWriter::new(BufWriter::with_capacity(BUF, out)),
            opts,
            plan,
            secret,
            tracker: Tracker::new(opts, Some(total), Some(sources.len() as u64)),
            buf: vec![0u8; opts.io_buffer()],
        };
        let workers = opts.workers();
        let mut batch: Vec<&SourceEntry> = Vec::new();
        for s in sources {
            opts.check_cancel()?;
            // Read afresh for every entry: a batch is the unit a change of limit waits for.
            let threads = workers.active();
            // The zip library cannot encrypt outside a ZipWriter, so encrypted archives are serial.
            if s.kind == EntryKind::File && s.size <= SMALL_FILE && threads > 1 && secret.is_none()
            {
                batch.push(s);
                if batch.len() >= threads * 16 {
                    c.flush_batch(&mut batch, threads)?;
                }
                continue;
            }
            // Keep archive order: whatever is queued goes in before this entry.
            c.flush_batch(&mut batch, threads)?;
            if s.kind == EntryKind::File {
                c.add_streamed(s)?;
            } else {
                c.add_other(s)?;
            }
            c.tracker.entry_done(&s.name);
        }
        c.flush_batch(&mut batch, workers.active())?;
        c.zw.finish()?.flush()?;
        Ok(())
    }
}
