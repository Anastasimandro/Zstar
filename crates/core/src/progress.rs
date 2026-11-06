use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::options::{Options, Progress};
use crate::sources::Totals;

/// Thread-safe counters behind the progress callback.
pub(crate) struct Tracker<'a> {
    opts: &'a Options,
    bytes: AtomicU64,
    entries: AtomicU64,
    bytes_total: Option<u64>,
    entries_total: Option<u64>,
    /// Totals that appear while a tree is still being walked; they replace the fixed ones.
    totals: Option<Arc<Totals>>,
}

impl<'a> Tracker<'a> {
    pub(crate) fn new(
        opts: &'a Options,
        bytes_total: Option<u64>,
        entries_total: Option<u64>,
    ) -> Self {
        Tracker {
            opts,
            bytes: AtomicU64::new(0),
            entries: AtomicU64::new(0),
            bytes_total,
            entries_total,
            totals: None,
        }
    }

    /// A tracker whose totals are unknown until the walk over the sources has finished.
    #[cfg_attr(not(any(feature = "zip", feature = "tar")), allow(dead_code))]
    pub(crate) fn streaming(opts: &'a Options, totals: Arc<Totals>) -> Self {
        Tracker {
            totals: Some(totals),
            ..Tracker::new(opts, None, None)
        }
    }

    /// Adds processed bytes, reports progress, and returns the new running total. This is
    /// also where the throughput limit applies: it may sleep, between chunks.
    pub(crate) fn add_bytes(&self, n: u64, current: &str) -> u64 {
        let total = self.bytes.fetch_add(n, Ordering::Relaxed) + n;
        self.report(total, self.entries.load(Ordering::Relaxed), current);
        self.opts.control.throttle(n);
        total
    }

    pub(crate) fn entry_done(&self, current: &str) {
        let done = self.entries.fetch_add(1, Ordering::Relaxed) + 1;
        self.report(self.bytes.load(Ordering::Relaxed), done, current);
    }

    pub(crate) fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    fn report(&self, bytes_done: u64, entries_done: u64, current: &str) {
        if let Some(cb) = &self.opts.progress {
            let (bytes_total, entries_total) = match &self.totals {
                Some(t) => t.known().unzip(),
                None => (self.bytes_total, self.entries_total),
            };
            cb(&Progress {
                bytes_done,
                bytes_total,
                entries_done,
                entries_total,
                current,
            });
        }
    }
}
