//! ZIP: central-directory based, so extraction can run on several threads.
//!
//! Every worker opens its own `ZipArchive` over a shared reader. The reader is locked only
//! for the moment bytes are fetched; inflating and writing happen outside the lock.

use std::hash::{BuildHasher, Hasher, RandomState};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Instant, SystemTime};

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
use crate::pipeline::{self, Item as Slot, Out};
use crate::progress::Tracker;
use crate::sources::{MetaBudget, SourceEntry, SourceStream};
use crate::stats::{self, Class, Stats};
use crate::timeconv::{Civil, civil_to_system, system_to_civil};

const BUF: usize = 64 * 1024;
/// Prepared entries up to this size are written to the compressor in one call; bigger ones in
/// `io_buffer()` pieces, exactly as the streaming path reads them, so both give the same bytes.
const WHOLE_WRITE: u64 = 1 << 20;
/// The most memory the in-flight compression window may reserve, and the least.
const WINDOW_MAX: u64 = 128 << 20;
const WINDOW_MIN: u64 = 8 << 20;
/// Largest file compressed whole on a worker. Bigger ones stream on the calling thread.
const SMALL_MAX: u64 = 32 << 20;
/// Bookkeeping reserved per entry on top of the data: the entry, its header, the result.
const PER_ENTRY: u64 = 16 << 10;
/// Most entries between the file walker and the archive writer.
const WINDOW_ENTRIES: usize = 4096;
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
    stats: Option<Stats>,
}

impl<'a, 'b> Shared<'a, 'b> {
    /// Waiting here is time a worker cannot read its input: it counts as `wait_input`.
    fn lock(&self) -> std::sync::MutexGuard<'a, &'b mut (dyn ReadSeek + 'b)> {
        stats::timed(self.stats.as_ref(), Class::Input, || {
            self.inner.lock().unwrap_or_else(PoisonError::into_inner)
        })
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
    stats: Option<&Stats>,
) -> Result<ZipArchive<SharedReader<'a, 'b>>> {
    let reader = BufReader::with_capacity(
        BUF,
        Shared {
            inner: shared,
            pos: 0,
            stats: stats.cloned(),
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

/// One kept entry of the plan: its place in the archive and where its name is stored.
#[derive(Clone, Copy)]
struct Item {
    index: usize,
    name_at: u64,
    name_len: u32,
}

/// Everything extraction will do, decided before anything is written. Stored compactly: every
/// name exists once, in `names`, and paths, modes and times are derived again when an entry
/// is extracted (the same checks run again), so the plan costs about the length of each name
/// plus 24 bytes per entry, whatever the archive holds.
struct Plan {
    names: Vec<u8>,
    dirs: Vec<Item>,
    files: Vec<Item>,
    links: Vec<Item>,
    total_bytes: u64,
    entries: u64,
    /// Two names differ only by case: on a case-insensitive file system they would collide,
    /// so extraction must not run in parallel.
    case_collision: bool,
}

/// Key of a path for collision detection: a keyed 64-bit hash of its lowercase form. Equal
/// lowercase paths always give equal keys; two different ones giving the same key (about
/// 2^-64 per pair) only makes extraction serial, which is always safe.
fn collision_key(state: &RandomState, rel: &Path, scratch: &mut String) -> u64 {
    let text = rel.to_string_lossy();
    scratch.clear();
    if text.is_ascii() {
        scratch.extend(text.bytes().map(|b| char::from(b.to_ascii_lowercase())));
    } else {
        scratch.push_str(&text.to_lowercase());
    }
    let mut hasher = state.build_hasher();
    hasher.write(scratch.as_bytes());
    hasher.finish()
}

impl Plan {
    fn name(&self, item: &Item) -> Result<&str> {
        let at = item.name_at as usize;
        self.names
            .get(at..at + item.name_len as usize)
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .ok_or_else(|| Error::InvalidArchive("entry name is not valid UTF-8".into()))
    }

    fn build<R: Read + Seek>(archive: &ZipArchive<R>, opts: &Options) -> Result<Plan> {
        let entries = archive.len() as u64;
        if entries > opts.limits.max_entries {
            return Err(Error::LimitExceeded(LimitKind::Entries));
        }
        let mut plan = Plan {
            names: Vec::new(),
            dirs: Vec::new(),
            files: Vec::new(),
            links: Vec::new(),
            total_bytes: 0,
            entries,
            case_collision: false,
        };
        let mut meta = MetaBudget::new(opts.limits.max_metadata_bytes);
        let state = RandomState::new();
        let mut keys: Vec<u64> = Vec::new();
        let mut scratch = String::new();
        for index in 0..archive.len() {
            opts.check_cancel()?;
            let entry = archive.by_index_data(index)?;
            let kind = kind_of(entry.is_dir(), entry.unix_mode());
            let name = entry.name()?;
            // Zip-slip barrier: one bad name rejects the whole archive before any write.
            let rel = crate::path::sanitize(name.as_bytes(), opts.windows_names)?;
            opts.limits
                .check_name(name.len(), rel.components().count())?;
            // An empty path (the destination itself) never collides with anything.
            if matches!(kind, EntryKind::File | EntryKind::Dir) && !rel.as_os_str().is_empty() {
                keys.push(collision_key(&state, &rel, &mut scratch));
            }
            if !matches!(kind, EntryKind::Dir | EntryKind::File | EntryKind::Symlink) {
                continue;
            }
            // The name, its item and its collision key: what this entry costs while planned.
            meta.charge_bytes(name.len() as u64 + size_of::<Item>() as u64 + 8)?;
            let item = Item {
                index,
                name_at: plan.names.len() as u64,
                name_len: u32::try_from(name.len())
                    .map_err(|_| Error::LimitExceeded(LimitKind::PathLength))?,
            };
            plan.names.extend_from_slice(name.as_bytes());
            match kind {
                EntryKind::Dir => plan.dirs.push(item),
                EntryKind::File => {
                    plan.total_bytes = plan.total_bytes.saturating_add(entry.size());
                    plan.files.push(item);
                }
                _ => plan.links.push(item),
            }
        }
        if plan.total_bytes > opts.limits.max_total_bytes {
            return Err(Error::LimitExceeded(LimitKind::TotalSize));
        }
        keys.sort_unstable();
        plan.case_collision = keys.windows(2).any(|w| w[0] == w[1]);
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
    plan: &Plan,
    session: &Session<'_>,
    next: &AtomicUsize,
    secret: Option<&str>,
    workers: &Workers,
    index: usize,
) -> Result<()> {
    let mut archive = open(shared, workers.stats.as_ref())?;
    let mut buf = vec![0u8; session.io_buffer()];
    loop {
        workers.checkpoint(
            index,
            || session.check_stop(),
            || next.load(Ordering::Relaxed) >= plan.files.len(),
        )?;
        let Some(item) = plan.files.get(next.fetch_add(1, Ordering::Relaxed)) else {
            return Ok(());
        };
        let name = plan.name(item)?;
        let rel = session.rel_path(name.as_bytes())?;
        let mut file = entry(&mut archive, item.index, secret)?;
        let mode = file.unix_mode().map(|m| m & 0o7777);
        let mtime = to_system_time(file.last_modified());
        session.write_file(&rel, name, &mut file, mode, mtime, &mut buf)?;
    }
}

fn run_workers(
    shared: &Mutex<&mut dyn ReadSeek>,
    plan: &Plan,
    session: &Session<'_>,
    threads: usize,
    secret: Option<&str>,
    workers: &Workers,
) -> Result<()> {
    let next = AtomicUsize::new(0);
    let first_error: Mutex<Option<Error>> = Mutex::new(None);
    let work = |index: usize| {
        if let Err(e) = worker(shared, plan, session, &next, secret, workers, index) {
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
        let mut main_done = None;
        std::thread::scope(|scope| {
            for index in 1..threads {
                scope.spawn(move || {
                    let _lane = stats::lane(workers.stats.as_ref());
                    work(index)
                });
            }
            work(0);
            main_done = workers.stats.as_ref().map(|_| Instant::now());
        });
        // From here to the end of the scope the calling thread only waited for the others.
        if let (Some(since), Some(s)) = (main_done, workers.stats.as_ref()) {
            s.add_since(Class::Workers, since);
        }
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

/// Compresses a whole file on a worker. Returns the entry and the bytes it keeps meanwhile
/// (an upper bound: the compressed data is not longer than the source, give or take).
fn compress_small(
    s: &SourceEntry,
    plan: Compression,
    opts: &Options,
    tracker: &Tracker<'_>,
    cap: u64,
    piece: usize,
) -> Result<(PreparedZipFile, u64)> {
    opts.check_cancel()?;
    stats::time_file(opts.stats.as_ref(), || {
        let mut data = Vec::with_capacity(s.size as usize);
        opts.open_source(&s.path)?
            .take(cap + 1)
            .read_to_end(&mut data)?;
        if data.len() as u64 > cap {
            return Err(io::Error::other(format!(
                "{} grew while being archived",
                s.path.display()
            ))
            .into());
        }
        let mut builder = ZipFileBuilder::new(&s.name, file_options(s, plan, None))?;
        if data.len() as u64 <= WHOLE_WRITE {
            builder.write_all(&data)?;
        } else {
            for chunk in data.chunks(piece) {
                opts.check_cancel()?;
                builder.write_all(chunk)?;
            }
        }
        tracker.add_bytes(data.len() as u64, &s.name);
        Ok((builder.finish()?, data.len() as u64 + PER_ENTRY))
    })
}

struct Creator<'a, 'o> {
    zw: ZipWriter<BufWriter<&'a mut dyn WriteSeek>>,
    opts: &'o Options,
    plan: Compression,
    secret: Option<&'o str>,
    tracker: &'o Tracker<'o>,
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
        let mut archive = open(&shared, opts.stats.as_ref())?;
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

        for item in &plan.dirs {
            session.make_dir(&session.rel_path(plan.name(item)?.as_bytes())?)?;
        }
        let workers = opts.file_workers();
        let threads = if plan.case_collision {
            1
        } else {
            workers.pool.min(plan.files.len()).max(1)
        };
        run_workers(&shared, &plan, &session, threads, secret, &workers)?;

        for link in &plan.links {
            let name = plan.name(link)?;
            let rel = session.rel_path(name.as_bytes())?;
            let mut target = Vec::new();
            entry(&mut archive, link.index, secret)?
                .take(MAX_LINK_TARGET + 1)
                .read_to_end(&mut target)?;
            if target.len() as u64 > MAX_LINK_TARGET {
                return Err(Error::UnsafeLink {
                    name: name.to_owned(),
                    target: "<too long>".into(),
                });
            }
            session.defer_symlink(rel, target, name)?;
        }
        session.finish()
    }

    fn create_stream(
        &self,
        sources: &mut dyn SourceStream,
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
        let tracker = Tracker::streaming(opts, sources.totals());
        let mut c = Creator {
            zw: ZipWriter::new(BufWriter::with_capacity(BUF, out)),
            opts,
            plan,
            secret,
            tracker: &tracker,
            buf: vec![0u8; opts.io_buffer()],
        };
        let workers = opts.workers();
        let piece = opts.io_buffer();
        let budget = (opts.limits.max_memory_bytes / 8).clamp(WINDOW_MIN, WINDOW_MAX);
        let small = SMALL_MAX.min(budget / 4);
        let cfg = pipeline::Config {
            workers: &workers,
            budget,
            max_items: WINDOW_ENTRIES,
        };
        // The zip writer keeps a central-directory record per entry until it finishes.
        let mut meta = MetaBudget::new(opts.limits.max_metadata_bytes);
        let source = || {
            let Some(s) = sources.next_entry()? else {
                return Ok(None);
            };
            meta.charge(&s)?;
            // Read afresh for every entry, so a change of limit reaches the next one. The zip
            // library cannot encrypt outside a ZipWriter, so encrypted archives are serial.
            let parallel = secret.is_none() && workers.active() > 1;
            let file = s.kind == EntryKind::File;
            Ok(Some(if file && parallel && s.size <= small {
                let cost = 2 * s.size + PER_ENTRY;
                Slot::Work { job: s, cost }
            } else {
                // Only a file the sink streams for a long while is worth reading ahead of.
                Slot::Inline {
                    defer: file && parallel,
                    item: s,
                }
            }))
        };
        let work = |s: SourceEntry| {
            let (prepared, kept) = compress_small(&s, plan, opts, &tracker, small, piece)?;
            Ok(((s, prepared), kept))
        };
        pipeline::run(&cfg, &|| opts.check_cancel(), source, &work, |out| {
            match out {
                Out::Done((s, prepared)) => {
                    c.zw.add_prepared_file(prepared)?;
                    c.tracker.entry_done(&s.name);
                }
                Out::Inline(s) => {
                    if s.kind == EntryKind::File {
                        stats::time_file(opts.stats.as_ref(), || c.add_streamed(&s))?;
                    } else {
                        c.add_other(&s)?;
                    }
                    c.tracker.entry_done(&s.name);
                }
            }
            Ok(())
        })?;
        // Central directory and the last buffered bytes: handed to the OS, not synced.
        stats::timed(opts.stats.as_ref(), Class::Finish, || {
            c.zw.finish()?.flush().map_err(Error::from)
        })?;
        Ok(())
    }
}
