use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::Arc;

use crate::control::{Control, WorkerHook, Workers};
use crate::error::{Error, LimitKind, Result};
use crate::profile::{DriveKind, Profile, Settings, cpus, settings_for};
use crate::stats::{Source, Stats, StatsSnapshot, Timed};

/// A snapshot of how far an operation has got.
#[derive(Debug, Clone, Copy)]
pub struct Progress<'a> {
    /// Uncompressed bytes processed so far.
    pub bytes_done: u64,
    /// Total uncompressed bytes, when known up front.
    pub bytes_total: Option<u64>,
    /// Entries finished so far.
    pub entries_done: u64,
    /// Total entries, when known up front.
    pub entries_total: Option<u64>,
    /// Name of the entry being processed.
    pub current: &'a str,
}

/// Opens a source file for reading, ideally with a sequential-access hint. Core makes no OS
/// calls: the caller supplies this, like [`WorkerHook`].
pub type OpenHook = Arc<dyn Fn(&Path) -> io::Result<File> + Send + Sync>;

/// Progress callback. It may be called from several threads at once.
pub type ProgressFn = Arc<dyn Fn(&Progress<'_>) + Send + Sync>;

/// Decompression-bomb limits applied while extracting.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Maximum total uncompressed bytes. `u64::MAX` disables the check.
    pub max_total_bytes: u64,
    /// Maximum number of entries. `u64::MAX` disables the check.
    pub max_entries: u64,
    /// Maximum uncompressed-to-archive size ratio. `0` disables the check.
    pub max_ratio: u64,
    /// The ratio is only checked once this many bytes have been written, so that small,
    /// highly compressible files do not trip it.
    pub ratio_min_bytes: u64,
    /// Largest memory a decoder may ask for, in bytes (the dictionary of an xz or 7z/LZMA
    /// stream). A few bytes of header can otherwise demand gigabytes. `u64::MAX` disables
    /// the check.
    pub max_memory_bytes: u64,
    /// Longest entry name, in bytes, when creating archives and when extracting them.
    /// `u64::MAX` disables the check.
    pub max_path_bytes: u64,
    /// Deepest entry, counted in name components, when creating and when extracting.
    /// `u64::MAX` disables the check.
    pub max_depth: u64,
    /// Most metadata one operation may hold in memory: the directory listings of the walk
    /// over the sources, the compact plan of a zip extraction, and what zip and 7z writers
    /// keep per entry until they finish (estimated as name, link target and 128 bytes).
    /// `u64::MAX` disables the check.
    pub max_metadata_bytes: u64,
}

impl Limits {
    /// No limits at all. Only for archives you fully trust.
    pub const fn unlimited() -> Self {
        Limits {
            max_total_bytes: u64::MAX,
            max_entries: u64::MAX,
            max_ratio: 0,
            ratio_min_bytes: 0,
            max_memory_bytes: u64::MAX,
            max_path_bytes: u64::MAX,
            max_depth: u64::MAX,
            max_metadata_bytes: u64::MAX,
        }
    }

    /// Fails when a name of `len` bytes or `depth` components is over the limits.
    pub(crate) fn check_name(&self, len: usize, depth: usize) -> Result<()> {
        if len as u64 > self.max_path_bytes {
            return Err(Error::LimitExceeded(LimitKind::PathLength));
        }
        if depth as u64 > self.max_depth {
            return Err(Error::LimitExceeded(LimitKind::Depth));
        }
        Ok(())
    }
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_total_bytes: 64 << 30,
            max_entries: 10_000_000,
            max_ratio: 500,
            ratio_min_bytes: 256 << 20,
            max_memory_bytes: 1 << 30,
            max_path_bytes: 32 << 10,
            max_depth: 1024,
            max_metadata_bytes: 1 << 30,
        }
    }
}

/// How the entries of a new zip archive are compressed.
///
/// LZMA and Deflate64 are not listed: the zip library reads them but cannot write them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ZipMethod {
    /// No compression.
    Stored,
    /// Deflate: what every zip tool reads. The default.
    Deflate,
    /// Bzip2 (needs the `zip-bzip2` feature).
    Bzip2,
    /// Zstandard (needs the `zip-zstd` feature; older unzip tools cannot read it).
    Zstd,
}

impl ZipMethod {
    /// Name as used on the command line.
    pub fn name(self) -> &'static str {
        match self {
            ZipMethod::Stored => "store",
            ZipMethod::Deflate => "deflate",
            ZipMethod::Bzip2 => "bzip2",
            ZipMethod::Zstd => "zstd",
        }
    }

    /// Parses a name as returned by [`ZipMethod::name`].
    pub fn from_name(name: &str) -> Option<ZipMethod> {
        [
            ZipMethod::Stored,
            ZipMethod::Deflate,
            ZipMethod::Bzip2,
            ZipMethod::Zstd,
        ]
        .into_iter()
        .find(|m| m.name() == name)
    }
}

/// A password for encrypted archives. It never appears in `Debug` output or error messages.
#[cfg_attr(not(any(feature = "zip", feature = "sevenz")), allow(dead_code))]
#[derive(Clone)]
pub struct Password(Arc<str>);

impl Password {
    /// Wraps `password`. An empty password is rejected by the formats that use it.
    pub fn new(password: &str) -> Self {
        Password(password.into())
    }

    #[cfg_attr(not(any(feature = "zip", feature = "sevenz")), allow(dead_code))]
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Password {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Password(<redacted>)")
    }
}

/// Settings shared by list, extract and create.
#[derive(Clone)]
pub struct Options {
    /// Bomb limits (extraction only).
    pub limits: Limits,
    /// Worker threads. `0` means what the profile says (one per CPU for `Fast`). A number
    /// given here wins over the profile and is the most that can ever run at once.
    pub threads: usize,
    /// Compression level; the valid range depends on the format. `None` uses its default.
    pub level: Option<i32>,
    /// Compression method for zip entries. `None` is deflate.
    pub zip_method: Option<ZipMethod>,
    /// Replace existing files instead of failing.
    pub overwrite: bool,
    /// Reject names that Windows cannot create (reserved device names, `:` and so on).
    /// On by default only on Windows.
    pub windows_names: bool,
    /// Live control: cancellation, thread limit, priority, throughput limit and profile
    /// switch. Workers read it between chunks, never inside one. Clones share it, so keep a
    /// clone to steer the operation from another thread.
    pub control: Control,
    /// How hard to work. `Auto` is resolved from [`drive`](Self::drive) when the operation
    /// starts; see [`Control::set_profile`] to switch while it runs.
    pub profile: Profile,
    /// The kind of drive being worked on. The caller finds it out; core never detects
    /// hardware, and `Unknown` takes the HDD-safe I/O rules.
    pub drive: DriveKind,
    /// Called by every thread working for the operation when it starts and whenever it sees
    /// the priority change. Core makes no OS calls: this is where a priority gets applied.
    pub worker_hook: Option<WorkerHook>,
    /// How source files and archives are opened for reading. `None` is `File::open`. If the
    /// hook fails the file is opened plainly, so a hint that cannot be given never fails an
    /// operation.
    pub open_hook: Option<OpenHook>,
    /// Timing counters for one operation; `None` (the default) measures nothing. Clones share
    /// them, so keep one to read the totals afterwards (see [`stats_snapshot`](Self::stats_snapshot)).
    pub stats: Option<Stats>,
    /// Optional progress callback.
    pub progress: Option<ProgressFn>,
    /// File name of the archive being read. A single-file format (gz, zst) has no name of its
    /// own, so it extracts to this name minus the extension. The `*_file` functions fill it in;
    /// callers of the stream API may set it, otherwise the output is called `data`.
    pub name_hint: Option<String>,
    /// Password to decrypt with (extract, list) or to encrypt with (create). Only zip
    /// (WinZip AES-256) and 7z (AES-256) support it; other formats reject it.
    pub password: Option<Password>,
}

// Not derivable: `windows_names` defaults to true on Windows. Clippy only sees the Linux value.
#[allow(clippy::derivable_impls)]
impl Default for Options {
    fn default() -> Self {
        Options {
            limits: Limits::default(),
            threads: 0,
            level: None,
            zip_method: None,
            overwrite: false,
            windows_names: cfg!(windows),
            control: Control::new(),
            profile: Profile::default(),
            drive: DriveKind::default(),
            worker_hook: None,
            open_hook: None,
            stats: None,
            progress: None,
            name_hint: None,
            password: None,
        }
    }
}

impl Options {
    /// What [`profile`](Self::profile) and [`drive`](Self::drive) come to on this machine.
    pub fn settings(&self) -> Settings {
        settings_for(self.profile, self.drive, cpus())
    }

    /// The worker-thread count to start with (at least 1).
    pub fn effective_threads(&self) -> usize {
        self.workers().active()
    }

    /// Size of the buffer files are copied through.
    pub(crate) fn io_buffer(&self) -> usize {
        self.settings().buffer
    }

    /// The workers of an operation. The pool is every CPU (or `threads`), whatever the
    /// profile, so a switch from `Gentle` to `Fast` while running has threads to grow into;
    /// the profile only sets how many of them may run at the start.
    pub(crate) fn workers(&self) -> Workers {
        let workers = self.build_workers();
        self.note_threads(&workers);
        workers
    }

    fn build_workers(&self) -> Workers {
        let s = self.settings();
        let (pool, limit) = match self.threads {
            0 => (cpus(), s.threads),
            n => (n, n),
        };
        Workers::new(
            self.control.clone(),
            self.worker_hook.clone(),
            pool,
            limit,
            s.priority,
            self.stats.clone(),
        )
    }

    fn note_threads(&self, workers: &Workers) {
        if let Some(stats) = &self.stats {
            stats.note_threads(workers.pool, workers.active());
        }
    }

    #[cfg_attr(not(feature = "zip"), allow(dead_code))]
    /// Like [`workers`](Self::workers) for extracting many files: a single worker unless the
    /// drive kind allows parallel extraction.
    pub(crate) fn file_workers(&self) -> Workers {
        let mut workers = self.build_workers();
        if !self.settings().parallel_files {
            workers.pool = 1;
        }
        self.note_threads(&workers);
        workers
    }

    /// Opens `path` for reading through [`open_hook`](Self::open_hook), with its reads timed
    /// when stats are on.
    pub(crate) fn open_source(&self, path: &Path) -> io::Result<Source> {
        let file = crate::stats::fs(
            self.stats.as_ref(),
            crate::stats::FsOp::OpenSrc,
            || match &self.open_hook {
                Some(hook) => hook(path).or_else(|_| File::open(path)),
                None => File::open(path),
            },
        )?;
        Ok(Timed::new(file, self.stats.clone()))
    }

    /// The totals of [`stats`](Self::stats) so far, with the time workers spent parked by the
    /// control. `None` when stats are off.
    pub fn stats_snapshot(&self) -> Option<StatsSnapshot> {
        self.stats
            .as_ref()
            .map(|s| s.snapshot(self.control.parked()))
    }

    /// The password, or an error if it is empty.
    #[cfg_attr(not(any(feature = "zip", feature = "sevenz")), allow(dead_code))]
    pub(crate) fn secret(&self) -> Result<Option<&str>> {
        match &self.password {
            None => Ok(None),
            Some(p) if p.expose().is_empty() => {
                Err(Error::InvalidOption("the password is empty".into()))
            }
            Some(p) => Ok(Some(p.expose())),
        }
    }

    /// The safe point every loop passes: fails once cancelled, and lets the hook know the
    /// priority of the calling thread when there is one.
    pub(crate) fn check_cancel(&self) -> Result<()> {
        if self.control.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if let Some(hook) = &self.worker_hook {
            self.control.sync_priority(hook, self.settings().priority);
        }
        Ok(())
    }
}
