//! Optional timing of one compress or extract operation.
//!
//! Off by default: without a [`Stats`] in [`Options`](crate::Options) nothing is timed and the
//! adapters pass every call straight through. When on, source reads, output writes and a few
//! other spans add to relaxed atomics, with no lock anywhere on the hot path. The numbers only
//! describe the operation; nothing in the engine reads them back.
//!
//! What each number means:
//!
//! * `read` / `read_bytes`: time inside `read` calls on files opened through
//!   [`Options::open_source`](crate::Options) and bytes they returned. Compressing: the source
//!   files. Extracting: the archive itself. The OS may serve a read from its cache, so this is
//!   the time the caller waited, not necessarily disk time.
//! * `write` / `write_bytes`: time inside `write` calls on the output and bytes handed to
//!   them. Compressing: the archive file (zip also seeks back, so this can exceed its final
//!   size). Extracting: the extracted files. A write that only reaches the OS cache is cheap;
//!   the disk catches up later and shows as a slower following write or as the final close.
//! * `codec_cpu`: only the multi-threaded gzip path records it: the sum, over its compression
//!   threads, of the time spent deflating blocks. It can exceed wall time.
//! * `op`: time inside the format handler call on the calling thread.
//! * [`StatsSnapshot::other`]: `op - read - write` on that thread. With one thread it is the
//!   compression or decompression time plus archive bookkeeping. With several threads
//!   `read` and `write` are sums across threads and may exceed `op`, so `other` is only a
//!   rough figure there.
//! * `parked`: time workers slept because of the throughput limit or because the thread limit
//!   asked them to wait (see [`Control`](crate::Control)). Summed across threads.
//! * `threads_pool` / `threads_start`: workers created and workers allowed at the start, as
//!   the engine granted them; a format that is single-threaded by nature still reports these.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

#[derive(Debug, Default)]
struct Counters {
    read_ns: AtomicU64,
    read_bytes: AtomicU64,
    write_ns: AtomicU64,
    write_bytes: AtomicU64,
    codec_cpu_ns: AtomicU64,
    op_ns: AtomicU64,
    threads_pool: AtomicU64,
    threads_start: AtomicU64,
}

/// Counters for one operation. Clones share them.
#[derive(Debug, Clone, Default)]
pub struct Stats(Arc<Counters>);

/// What a [`Stats`] holds, plus the parked time of the [`Control`](crate::Control).
/// `threads_pool` and `threads_start` stay 0 for a format that never asks for workers: it runs
/// on the calling thread alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StatsSnapshot {
    pub read: Duration,
    pub read_bytes: u64,
    pub write: Duration,
    pub write_bytes: u64,
    pub codec_cpu: Duration,
    pub op: Duration,
    pub parked: Duration,
    pub threads_pool: u64,
    pub threads_start: u64,
}

impl StatsSnapshot {
    /// `op - read - write`, never negative. See the module docs for what it means.
    pub fn other(&self) -> Duration {
        self.op.saturating_sub(self.read + self.write)
    }
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

impl Stats {
    pub fn new() -> Self {
        Self::default()
    }

    /// The totals so far. `parked` comes from the control, which counts it.
    pub fn snapshot(&self, parked: Duration) -> StatsSnapshot {
        let c = &*self.0;
        StatsSnapshot {
            read: Duration::from_nanos(c.read_ns.load(Relaxed)),
            read_bytes: c.read_bytes.load(Relaxed),
            write: Duration::from_nanos(c.write_ns.load(Relaxed)),
            write_bytes: c.write_bytes.load(Relaxed),
            codec_cpu: Duration::from_nanos(c.codec_cpu_ns.load(Relaxed)),
            op: Duration::from_nanos(c.op_ns.load(Relaxed)),
            parked,
            threads_pool: c.threads_pool.load(Relaxed),
            threads_start: c.threads_start.load(Relaxed),
        }
    }

    pub(crate) fn add_read(&self, d: Duration, bytes: u64) {
        self.0.read_ns.fetch_add(nanos(d), Relaxed);
        self.0.read_bytes.fetch_add(bytes, Relaxed);
    }

    pub(crate) fn add_write(&self, d: Duration, bytes: u64) {
        self.0.write_ns.fetch_add(nanos(d), Relaxed);
        self.0.write_bytes.fetch_add(bytes, Relaxed);
    }

    #[cfg_attr(not(feature = "gzip"), allow(dead_code))]
    pub(crate) fn add_codec_cpu(&self, d: Duration) {
        self.0.codec_cpu_ns.fetch_add(nanos(d), Relaxed);
    }

    pub(crate) fn add_op(&self, d: Duration) {
        self.0.op_ns.fetch_add(nanos(d), Relaxed);
    }

    pub(crate) fn note_threads(&self, pool: usize, start: usize) {
        self.0.threads_pool.fetch_max(pool as u64, Relaxed);
        self.0.threads_start.fetch_max(start as u64, Relaxed);
    }
}

/// Runs `f` and adds its duration to the operation time, when stats are on.
pub(crate) fn timed_op<T>(stats: Option<&Stats>, f: impl FnOnce() -> T) -> T {
    let Some(s) = stats else { return f() };
    let start = Instant::now();
    let value = f();
    s.add_op(start.elapsed());
    value
}

/// Passes every call through to `inner` and, when stats are on, times it: `read` calls count
/// as source reads, `write` calls as output writes. With stats off the only cost is one
/// branch on an `Option`.
/// Like [`timed_op`] for one output write of `bytes`; the time counts even if it fails.
pub(crate) fn timed_write(
    stats: Option<&Stats>,
    bytes: u64,
    f: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    let Some(s) = stats else { return f() };
    let start = Instant::now();
    let r = f();
    s.add_write(start.elapsed(), bytes);
    r
}

#[derive(Debug)]
pub(crate) struct Timed<T> {
    inner: T,
    stats: Option<Stats>,
}

impl<T> Timed<T> {
    pub(crate) fn new(inner: T, stats: Option<Stats>) -> Self {
        Timed { inner, stats }
    }
}

impl<T: Read> Read for Timed<T> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let Some(stats) = &self.stats else {
            return self.inner.read(buf);
        };
        let start = Instant::now();
        let r = self.inner.read(buf);
        stats.add_read(start.elapsed(), *r.as_ref().unwrap_or(&0) as u64);
        r
    }
}

impl<T: Write> Write for Timed<T> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let Some(stats) = &self.stats else {
            return self.inner.write(buf);
        };
        let start = Instant::now();
        let r = self.inner.write(buf);
        stats.add_write(start.elapsed(), *r.as_ref().unwrap_or(&0) as u64);
        r
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<T: Seek> Seek for Timed<T> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

/// A source file as the formats read it: a plain `File` with its reads timed.
pub(crate) type Source = Timed<File>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn off_means_no_counting_and_unchanged_data() {
        let mut r = Timed::new(Cursor::new(b"abcdef".to_vec()), None);
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"abcdef");
        let mut w = Timed::new(Vec::new(), None);
        w.write_all(b"xyz").unwrap();
        assert_eq!(w.inner, b"xyz");
    }

    #[test]
    fn reads_and_writes_land_in_their_own_counters() {
        let stats = Stats::new();
        let mut r = Timed::new(Cursor::new(vec![7u8; 1000]), Some(stats.clone()));
        let mut buf = [0u8; 64];
        let mut total = 0u64;
        loop {
            match r.read(&mut buf).unwrap() {
                0 => break,
                n => total += n as u64,
            }
        }
        let mut w = Timed::new(Vec::new(), Some(stats.clone()));
        w.write_all(&[1u8; 300]).unwrap();
        w.write_all(&[2u8; 5]).unwrap();
        let s = stats.snapshot(Duration::ZERO);
        assert_eq!((s.read_bytes, total), (1000, 1000));
        assert_eq!(s.write_bytes, 305);
        assert_eq!(w.inner.len(), 305);
    }

    #[test]
    fn one_thread_sums_are_consistent() {
        // A reader that takes a known time: the time counted must cover it and the operation
        // time must cover the counted time, so `other` is never negative nor above `op`.
        struct Slow(u32);
        impl Read for Slow {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.0 == 0 {
                    return Ok(0);
                }
                self.0 -= 1;
                std::thread::sleep(Duration::from_millis(5));
                buf[0] = 0;
                Ok(1)
            }
        }
        let stats = Stats::new();
        let mut r = Timed::new(Slow(4), Some(stats.clone()));
        let mut sink = Timed::new(io::sink(), Some(stats.clone()));
        timed_op(Some(&stats), || {
            let mut buf = [0u8; 8];
            loop {
                match r.read(&mut buf).unwrap() {
                    0 => break,
                    n => sink.write_all(&buf[..n]).unwrap(),
                }
            }
        });
        let s = stats.snapshot(Duration::ZERO);
        assert!(s.read >= Duration::from_millis(20), "{s:?}");
        assert!(s.read + s.write <= s.op, "{s:?}");
        assert!(s.other() <= s.op);
        assert_eq!((s.read_bytes, s.write_bytes), (4, 4));
    }

    #[test]
    fn counters_add_up_across_threads() {
        let stats = Stats::new();
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    for _ in 0..100 {
                        stats.add_read(Duration::from_micros(10), 3);
                        stats.add_write(Duration::from_micros(20), 5);
                    }
                });
            }
        });
        let s = stats.snapshot(Duration::from_millis(1));
        assert_eq!(s.read_bytes, 1200);
        assert_eq!(s.write_bytes, 2000);
        assert_eq!(s.read, Duration::from_micros(4000));
        assert_eq!(s.write, Duration::from_micros(8000));
        assert_eq!(s.parked, Duration::from_millis(1));
    }

    #[test]
    fn threads_keep_the_largest_value_seen() {
        let stats = Stats::new();
        stats.note_threads(8, 2);
        stats.note_threads(1, 4);
        let s = stats.snapshot(Duration::ZERO);
        assert_eq!((s.threads_pool, s.threads_start), (8, 4));
    }
}
