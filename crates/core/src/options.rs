use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::{Error, Result};

/// Cooperative cancellation flag. Clones share the same flag.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// A fresh, not-yet-cancelled token.
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation. Safe to call from any thread or a signal handler thread.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Whether [`cancel`](Self::cancel) has been called.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

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
}

impl Limits {
    /// No limits at all. Only for archives you fully trust.
    pub const fn unlimited() -> Self {
        Limits {
            max_total_bytes: u64::MAX,
            max_entries: u64::MAX,
            max_ratio: 0,
            ratio_min_bytes: 0,
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_total_bytes: 64 << 30,
            max_entries: 10_000_000,
            max_ratio: 500,
            ratio_min_bytes: 256 << 20,
        }
    }
}

/// Settings shared by list, extract and create.
#[derive(Clone)]
pub struct Options {
    /// Bomb limits (extraction only).
    pub limits: Limits,
    /// Worker threads. `0` means one per available CPU.
    pub threads: usize,
    /// Compression level; the valid range depends on the format. `None` uses its default.
    pub level: Option<i32>,
    /// Replace existing files instead of failing.
    pub overwrite: bool,
    /// Reject names that Windows cannot create (reserved device names, `:` and so on).
    /// On by default only on Windows.
    pub windows_names: bool,
    /// Cancellation flag, polled between chunks of data.
    pub cancel: CancelToken,
    /// Optional progress callback.
    pub progress: Option<ProgressFn>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            limits: Limits::default(),
            threads: 0,
            level: None,
            overwrite: false,
            windows_names: cfg!(windows),
            cancel: CancelToken::new(),
            progress: None,
        }
    }
}

impl Options {
    /// The effective worker-thread count (at least 1).
    pub fn effective_threads(&self) -> usize {
        if self.threads > 0 {
            self.threads
        } else {
            std::thread::available_parallelism().map_or(1, |n| n.get())
        }
    }

    pub(crate) fn check_cancel(&self) -> Result<()> {
        if self.cancel.is_cancelled() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }
}
