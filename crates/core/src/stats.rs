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
//! * `op`: time inside `create_file` / `extract_file` on the calling thread: finding the
//!   sources, opening, the format handler, closing and the final rename.
//! * [`StatsSnapshot::other`]: `op - read - write` on that thread. With one thread it is the
//!   compression or decompression time plus archive bookkeeping. With several threads
//!   `read` and `write` are sums across threads and may exceed `op`, so `other` is only a
//!   rough figure there.
//! * `parked`: time workers slept because of the throughput limit or because the thread limit
//!   asked them to wait (see [`Control`](crate::Control)). Summed across threads.
//! * `threads_pool` / `threads_start`: workers created and workers allowed at the start, as
//!   the engine granted them; a format that is single-threaded by nature still reports these.
//!
//! # Where the time of each thread went
//!
//! Every thread working for the operation (the calling thread, plus each worker a format
//! spawns) keeps a private tally while it runs and adds it to the shared counters once, when
//! it ends. Its time is split into exclusive parts, so for one thread
//! `busy + wait_input + wait_output + wait_workers + finish + parked` is its running time,
//! and all of them are summed across threads (thread-seconds, so they can exceed wall time):
//!
//! * `busy`: the thread was running code of its own: compressing, decompressing, checksums,
//!   bookkeeping. It is what is left of the thread's time after the other parts are taken
//!   out, so it also holds anything not timed on purpose (path checks, progress callbacks).
//! * `wait_input`: blocked getting input: reads of source files or of the archive, opening
//!   and inspecting source files, and waiting for the lock of the zip reader that all
//!   extraction workers share. A read served from the OS cache costs almost nothing here.
//! * `wait_output`: blocked on output: `write` calls, and creating files and directories or
//!   checking whether a destination exists. A write that only reaches the OS cache is cheap;
//!   the disk catches up later and shows in `finish` or in a later, slower write.
//! * `wait_workers`: a thread idle because it handed work to others and waits for them
//!   (the calling thread during a gzip wave or a zip batch, or at the end of zip extraction).
//! * `finish`: closing output files, renaming `<name>.part` to its final name, and the final
//!   flush of an archive or compressed stream (zip central directory, tar trailer, encoder
//!   end). **No `fsync` is issued anywhere, so this is the time to hand the data to the OS,
//!   not the time until it is on the disk**; the stats never present it as physical
//!   completion. Writes made inside a finishing step count here and not in `wait_output`.
//!
//! The thread that walks the source tree while archiving runs beside the format and is counted
//! like any other, except for the time it sits idle because the queue to the format is full:
//! that time is taken out and counted nowhere.
//!
//! Threads inside third-party codecs (7z LZMA2, zstd and xz multi-thread) are not seen: the
//! thread that drives them shows their time as `busy`.
//!
//! [`StatsSnapshot::dominant`] names the largest of those parts, so every operation reports
//! one dominant stage.
//!
//! # Counts and latency
//!
//! * `opens`, `stat_calls`, `mkdirs`, `renames`: file-system calls the engine made. `opens`
//!   counts source files and archive opened for reading and output files created. `stat_calls`
//!   is one per source entry when archiving (a directory listing, `readlink` and `canonicalize`
//!   are not counted) and the destination checks when extracting. `mkdirs` counts calls to
//!   create directories whether or not they already existed. `renames` is one per `.part`
//!   file. Symlinks made at the end of an extraction are not counted.
//! * `files`: regular files completed (archived or extracted). Directories and links are not
//!   counted. Divide by wall time for files per second.
//! * `latency_p50` / `latency_p95`: the time one file took from the moment a thread started
//!   on it until it was done. Extracting: create, copy, close and rename, including the time
//!   the archive reader needed to produce the data. Archiving: reading the source and
//!   compressing it (zip batches count until the entry is ready, not until it is appended).
//!   Values are the upper edge of a bucket of a log-linear histogram (8 buckets per power of
//!   two), so they are at most 12.5% above the true percentile. Formats that do not handle
//!   files one by one (7z creation) report none.
//! * `mem_budget_bytes` / `mem_encoder_bytes` / `mem_reserved_peak_bytes`: zip creation: the
//!   memory it was allowed for entries in flight and compressor state together, the part of it
//!   set aside for the state of the compressors (every worker's and the calling thread's), and
//!   the most it reserved for entries at once. `n/a` for formats without that pipeline.
//! * `active_peak`: the most threads busy at once inside the zip pipeline; it must not exceed
//!   the thread limit of the profile (`threads_start`, or the lower limit after a switch).
//! * `queue_avg` / `queue_max`: how many items were handed to the workers each time a gzip
//!   wave or a zip batch started. Sampled at that moment, not time-weighted. Formats without
//!   a queue report none.

use std::cell::Cell;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use crate::error::Result;

/// Log-linear histogram: values below 8 ns get a bucket each, then 8 buckets per power of
/// two up to 2^42 ns (over an hour); anything longer lands in the last one.
const BUCKETS: usize = 320;

#[derive(Debug)]
struct Histogram([AtomicU64; BUCKETS]);

impl Default for Histogram {
    fn default() -> Self {
        Histogram(std::array::from_fn(|_| AtomicU64::new(0)))
    }
}

fn bucket_of(ns: u64) -> usize {
    if ns < 8 {
        return ns as usize;
    }
    let msb = 63 - ns.leading_zeros() as usize;
    let sub = ((ns >> (msb - 3)) & 7) as usize;
    ((msb - 2) * 8 + sub).min(BUCKETS - 1)
}

/// The first value above bucket `idx`.
fn bucket_upper(idx: usize) -> u64 {
    if idx < 8 {
        return idx as u64 + 1;
    }
    let (msb, sub) = (idx / 8 + 2, (idx % 8) as u64);
    (9 + sub) << (msb - 3)
}

impl Histogram {
    fn record(&self, ns: u64) {
        self.0[bucket_of(ns)].fetch_add(1, Relaxed);
    }

    /// Upper edge of the bucket holding the `p` quantile (0 < p <= 1); `None` when empty.
    fn quantile(&self, p: f64) -> Option<Duration> {
        let counts: Vec<u64> = self.0.iter().map(|b| b.load(Relaxed)).collect();
        let total: u64 = counts.iter().sum();
        if total == 0 {
            return None;
        }
        let rank = ((p * total as f64).ceil() as u64).clamp(1, total);
        let mut seen = 0;
        for (idx, n) in counts.iter().enumerate() {
            seen += n;
            if seen >= rank {
                return Some(Duration::from_nanos(bucket_upper(idx)));
            }
        }
        None
    }
}

#[derive(Debug, Default)]
struct Counters {
    read_ns: AtomicU64,
    read_bytes: AtomicU64,
    write_ns: AtomicU64,
    write_bytes: AtomicU64,
    /// `read` and `write` calls on the timed adapters.
    io_ops: AtomicU64,
    codec_cpu_ns: AtomicU64,
    op_ns: AtomicU64,
    threads_pool: AtomicU64,
    threads_start: AtomicU64,
    busy_ns: AtomicU64,
    wait_input_ns: AtomicU64,
    wait_output_ns: AtomicU64,
    wait_workers_ns: AtomicU64,
    finish_ns: AtomicU64,
    opens: AtomicU64,
    stat_calls: AtomicU64,
    mkdirs: AtomicU64,
    renames: AtomicU64,
    files: AtomicU64,
    queue_samples: AtomicU64,
    queue_sum: AtomicU64,
    queue_max: AtomicU64,
    mem_budget: AtomicU64,
    mem_encoder: AtomicU64,
    mem_reserved_peak: AtomicU64,
    active_peak: AtomicU64,
    latency: Histogram,
}

/// One thread's tally of the parts of its time, in nanoseconds. Plain integers in a
/// thread-local cell: nothing is shared until the thread ends (see [`Lane`]).
#[derive(Clone, Copy, Default)]
struct Acc {
    input: u64,
    output: u64,
    workers: u64,
    finish: u64,
    parked: u64,
    idle: u64,
}

thread_local! {
    static ACC: Cell<Acc> = const { Cell::new(Acc { input: 0, output: 0, workers: 0, finish: 0, parked: 0, idle: 0 }) };
    /// A timed span is open on this thread and will add its whole duration itself, so the
    /// adapters inside it must not add theirs a second time.
    static IN_SPAN: Cell<bool> = const { Cell::new(false) };
}

/// Which part of a thread's time a span belongs to.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Class {
    Input,
    Output,
    #[cfg_attr(not(any(feature = "zip", feature = "gzip")), allow(dead_code))]
    Workers,
    Finish,
    /// A helper thread idle because its queue is full: taken out of its time and counted
    /// nowhere, so a walker that is ahead of the format does not look like waiting.
    #[cfg_attr(not(feature = "zip"), allow(dead_code))]
    Idle,
}

fn add_acc(class: Class, ns: u64) {
    ACC.with(|c| {
        let mut a = c.get();
        match class {
            Class::Input => a.input += ns,
            Class::Output => a.output += ns,
            Class::Workers => a.workers += ns,
            Class::Finish => a.finish += ns,
            Class::Idle => a.idle += ns,
        }
        c.set(a);
    });
}

/// Like `add_acc` for the adapters, which stay silent inside a span.
fn add_acc_unless_span(class: Class, ns: u64) {
    if !IN_SPAN.with(Cell::get) {
        add_acc(class, ns);
    }
}

/// Called by the control when a worker sleeps. Parked time is rare and the call sleeps for
/// milliseconds anyway, so it is not gated on stats.
pub(crate) fn note_parked(ns: u64) {
    ACC.with(|c| {
        let mut a = c.get();
        a.parked += ns;
        c.set(a);
    });
}

/// File-system calls that are counted and timed.
#[derive(Clone, Copy)]
pub(crate) enum FsOp {
    OpenSrc,
    StatSrc,
    CreateOut,
    StatOut,
    MkdirOut,
    RenameOut,
}

impl FsOp {
    fn class(self) -> Class {
        match self {
            FsOp::OpenSrc | FsOp::StatSrc => Class::Input,
            FsOp::CreateOut | FsOp::StatOut | FsOp::MkdirOut => Class::Output,
            FsOp::RenameOut => Class::Finish,
        }
    }
}

/// Runs `f` and adds its duration to `class` of the calling thread, when stats are on.
pub(crate) fn timed<T>(stats: Option<&Stats>, class: Class, f: impl FnOnce() -> T) -> T {
    if stats.is_none() {
        return f();
    }
    let start = Instant::now();
    let outer = IN_SPAN.with(|s| s.replace(true));
    let value = f();
    IN_SPAN.with(|s| s.set(outer));
    add_acc(class, nanos(start.elapsed()));
    value
}

/// Runs one file-system call, counting it and timing it as part of its class.
pub(crate) fn fs<T>(stats: Option<&Stats>, op: FsOp, f: impl FnOnce() -> T) -> T {
    if let Some(s) = stats {
        s.count(op, 1);
    }
    timed(stats, op.class(), f)
}

/// How often a working thread hands what it tallied to the shared counters, so that a reader
/// sees the parts of thread time while the operation runs and not only when a thread ends.
const LIVE_EVERY: Duration = Duration::from_millis(25);

/// The share of the time of one thread that is added to the counters, in pieces while it
/// works (see [`flush_live`]) and the rest when the guard drops. A thread keeps one for as
/// long as it works; the calling thread's is [`timed_op`].
///
/// The state lives in a thread-local stack, so the safe points that have no handle to the lane
/// can still flush it. The guard stays on its thread.
pub(crate) struct Lane(bool, std::marker::PhantomData<*const ()>);

struct LaneState {
    stats: Stats,
    /// Start of the part not yet added to the counters.
    since: Instant,
    /// What `ACC` held at `since`.
    base: Acc,
}

impl LaneState {
    /// Adds the part since the last flush to the counters and starts the next one.
    fn flush(&mut self, now: Instant) {
        let (acc, c) = (ACC.with(Cell::get), &*self.stats.0);
        let part = |a: u64, b: u64| a.saturating_sub(b);
        let (input, output) = (
            part(acc.input, self.base.input),
            part(acc.output, self.base.output),
        );
        let (workers, finish) = (
            part(acc.workers, self.base.workers),
            part(acc.finish, self.base.finish),
        );
        let parked = part(acc.parked, self.base.parked);
        let idle = part(acc.idle, self.base.idle);
        let elapsed = nanos(now.saturating_duration_since(self.since));
        let busy = elapsed.saturating_sub(input + output + workers + finish + parked + idle);
        c.busy_ns.fetch_add(busy, Relaxed);
        c.wait_input_ns.fetch_add(input, Relaxed);
        c.wait_output_ns.fetch_add(output, Relaxed);
        c.wait_workers_ns.fetch_add(workers, Relaxed);
        c.finish_ns.fetch_add(finish, Relaxed);
        self.since = now;
        self.base = acc;
    }
}

thread_local! {
    /// The lanes of this thread, innermost last.
    static LANES: std::cell::RefCell<Vec<LaneState>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Starts the tally of the calling thread. With stats off it is one branch and no clock.
pub(crate) fn lane(stats: Option<&Stats>) -> Lane {
    let Some(s) = stats else {
        return Lane(false, std::marker::PhantomData);
    };
    LANES.with(|l| {
        l.borrow_mut().push(LaneState {
            stats: s.clone(),
            since: Instant::now(),
            base: ACC.with(Cell::get),
        });
    });
    Lane(true, std::marker::PhantomData)
}

impl Drop for Lane {
    fn drop(&mut self) {
        if self.0 {
            LANES.with(|l| {
                if let Some(mut state) = l.borrow_mut().pop() {
                    state.flush(Instant::now());
                }
            });
        }
    }
}

/// Hands the calling thread's tally to the counters if the last time was long enough ago.
/// Call it at a safe point, between spans (it does nothing inside one, which has not added its
/// time yet). With no lane on the thread it is one thread-local read.
pub(crate) fn flush_live() {
    if IN_SPAN.with(Cell::get) {
        return;
    }
    LANES.with(|l| {
        let mut lanes = l.borrow_mut();
        if lanes.is_empty() {
            return;
        }
        let now = Instant::now();
        for state in lanes.iter_mut() {
            if now.saturating_duration_since(state.since) >= LIVE_EVERY {
                state.flush(now);
            }
        }
    });
}

/// Which part of the operation took the most thread time. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Threads were computing: compressing, decompressing, checksums.
    Cpu,
    /// Threads were waiting for source data or the archive.
    WaitInput,
    /// Threads were waiting for writes, file creation or directory creation.
    WaitOutput,
    /// A thread was idle waiting for the workers it handed work to.
    WaitWorkers,
    /// Closing files, renaming and the final flush (not physical completion).
    Finish,
    /// Threads slept because of the throughput or thread limit.
    Parked,
}

impl Stage {
    /// Name as printed by `--stats`.
    pub fn name(self) -> &'static str {
        match self {
            Stage::Cpu => "cpu",
            Stage::WaitInput => "wait_input",
            Stage::WaitOutput => "wait_output",
            Stage::WaitWorkers => "wait_workers",
            Stage::Finish => "finish",
            Stage::Parked => "parked",
        }
    }
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
    /// `read` and `write` calls behind `read` and `write`.
    pub io_ops: u64,
    pub codec_cpu: Duration,
    pub op: Duration,
    pub parked: Duration,
    pub threads_pool: u64,
    pub threads_start: u64,
    /// The parts of thread time, summed over threads. See the module docs.
    pub busy: Duration,
    pub wait_input: Duration,
    pub wait_output: Duration,
    pub wait_workers: Duration,
    pub finish: Duration,
    pub opens: u64,
    pub stat_calls: u64,
    pub mkdirs: u64,
    pub renames: u64,
    /// Regular files completed.
    pub files: u64,
    pub queue_samples: u64,
    pub queue_sum: u64,
    pub queue_max: u64,
    /// Memory budget of the zip pipeline when the operation started, in bytes (0: none).
    pub mem_budget: u64,
    /// Part of it that went to the compressors' own state, in bytes.
    pub mem_encoder: u64,
    /// Most bytes the pipeline had reserved at once for entries in flight.
    pub mem_reserved_peak: u64,
    /// Most threads busy at the same time inside the zip pipeline (workers in a job, and the
    /// calling thread while it handles an entry itself); 0 when it did not run.
    pub active_peak: u64,
    pub latency_p50: Option<Duration>,
    pub latency_p95: Option<Duration>,
}

impl StatsSnapshot {
    /// `op - read - write`, never negative. See the module docs for what it means.
    pub fn other(&self) -> Duration {
        self.op.saturating_sub(self.read + self.write)
    }

    /// Mean number of items handed to the workers per wave or batch; `None` without a queue.
    pub fn queue_avg(&self) -> Option<f64> {
        (self.queue_samples > 0).then(|| self.queue_sum as f64 / self.queue_samples as f64)
    }

    /// The part that took the most thread time and its share of the total (0 to 1), or
    /// `None` when nothing was timed. Ties go to the earlier stage.
    pub fn dominant(&self) -> Option<(Stage, f64)> {
        let parts = [
            (Stage::Cpu, self.busy),
            (Stage::WaitInput, self.wait_input),
            (Stage::WaitOutput, self.wait_output),
            (Stage::WaitWorkers, self.wait_workers),
            (Stage::Finish, self.finish),
            (Stage::Parked, self.parked),
        ];
        let total: Duration = parts.iter().map(|p| p.1).sum();
        if total.is_zero() {
            return None;
        }
        let best = parts
            .iter()
            .rev()
            .max_by_key(|p| p.1)
            .copied()
            .unwrap_or(parts[0]);
        Some((best.0, best.1.as_secs_f64() / total.as_secs_f64()))
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
            io_ops: c.io_ops.load(Relaxed),
            codec_cpu: Duration::from_nanos(c.codec_cpu_ns.load(Relaxed)),
            op: Duration::from_nanos(c.op_ns.load(Relaxed)),
            parked,
            threads_pool: c.threads_pool.load(Relaxed),
            threads_start: c.threads_start.load(Relaxed),
            busy: Duration::from_nanos(c.busy_ns.load(Relaxed)),
            wait_input: Duration::from_nanos(c.wait_input_ns.load(Relaxed)),
            wait_output: Duration::from_nanos(c.wait_output_ns.load(Relaxed)),
            wait_workers: Duration::from_nanos(c.wait_workers_ns.load(Relaxed)),
            finish: Duration::from_nanos(c.finish_ns.load(Relaxed)),
            opens: c.opens.load(Relaxed),
            stat_calls: c.stat_calls.load(Relaxed),
            mkdirs: c.mkdirs.load(Relaxed),
            renames: c.renames.load(Relaxed),
            files: c.files.load(Relaxed),
            queue_samples: c.queue_samples.load(Relaxed),
            queue_sum: c.queue_sum.load(Relaxed),
            queue_max: c.queue_max.load(Relaxed),
            mem_budget: c.mem_budget.load(Relaxed),
            mem_encoder: c.mem_encoder.load(Relaxed),
            mem_reserved_peak: c.mem_reserved_peak.load(Relaxed),
            active_peak: c.active_peak.load(Relaxed),
            latency_p50: c.latency.quantile(0.50),
            latency_p95: c.latency.quantile(0.95),
        }
    }

    pub(crate) fn add_read(&self, d: Duration, bytes: u64) {
        self.0.read_ns.fetch_add(nanos(d), Relaxed);
        self.0.read_bytes.fetch_add(bytes, Relaxed);
        self.0.io_ops.fetch_add(1, Relaxed);
        add_acc_unless_span(Class::Input, nanos(d));
    }

    pub(crate) fn add_write(&self, d: Duration, bytes: u64) {
        self.0.write_ns.fetch_add(nanos(d), Relaxed);
        self.0.write_bytes.fetch_add(bytes, Relaxed);
        self.0.io_ops.fetch_add(1, Relaxed);
        add_acc_unless_span(Class::Output, nanos(d));
    }

    pub(crate) fn count(&self, op: FsOp, n: u64) {
        let c = match op {
            FsOp::OpenSrc | FsOp::CreateOut => &self.0.opens,
            FsOp::StatSrc | FsOp::StatOut => &self.0.stat_calls,
            FsOp::MkdirOut => &self.0.mkdirs,
            FsOp::RenameOut => &self.0.renames,
        };
        c.fetch_add(n, Relaxed);
    }

    /// Adds the time since `since` to `class` of the calling thread.
    #[cfg_attr(not(feature = "zip"), allow(dead_code))]
    pub(crate) fn add_since(&self, class: Class, since: Instant) {
        let _ = self;
        add_acc(class, nanos(since.elapsed()));
    }

    /// One sample of how many items a wave or batch hands to the workers.
    #[cfg_attr(not(any(feature = "zip", feature = "gzip")), allow(dead_code))]
    pub(crate) fn note_queue(&self, len: usize) {
        self.0.queue_samples.fetch_add(1, Relaxed);
        self.0.queue_sum.fetch_add(len as u64, Relaxed);
        self.0.queue_max.fetch_max(len as u64, Relaxed);
    }

    /// The memory budget of a pipeline and what of it the compressors' state takes.
    #[cfg_attr(not(feature = "zip"), allow(dead_code))]
    pub(crate) fn note_memory(&self, budget: u64, encoder: u64) {
        self.0.mem_budget.fetch_max(budget, Relaxed);
        self.0.mem_encoder.fetch_max(encoder, Relaxed);
    }

    #[cfg_attr(not(feature = "zip"), allow(dead_code))]
    pub(crate) fn note_reserved(&self, bytes: u64) {
        self.0.mem_reserved_peak.fetch_max(bytes, Relaxed);
    }

    #[cfg_attr(not(feature = "zip"), allow(dead_code))]
    pub(crate) fn note_active(&self, threads: usize) {
        self.0.active_peak.fetch_max(threads as u64, Relaxed);
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

/// Runs `f` as the calling thread's whole share of the operation: adds its duration to the
/// operation time and tallies how that thread's time was spent. Does nothing with stats off.
pub(crate) fn timed_op<T>(stats: Option<&Stats>, f: impl FnOnce() -> T) -> T {
    let Some(s) = stats else { return f() };
    let _lane = lane(stats);
    let start = Instant::now();
    let value = f();
    s.add_op(start.elapsed());
    value
}

/// Runs `f` for one regular file and, when it succeeds, counts the file and records how
/// long it took.
pub(crate) fn time_file<T>(stats: Option<&Stats>, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let Some(s) = stats else { return f() };
    let start = Instant::now();
    let r = f();
    if r.is_ok() {
        s.0.files.fetch_add(1, Relaxed);
        s.0.latency.record(nanos(start.elapsed()));
    }
    r
}

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

/// Passes every call through to `inner` and, when stats are on, times it: `read` calls count
/// as source reads, `write` calls as output writes. With stats off the only cost is one
/// branch on an `Option`.
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
pub(crate) type Source = crate::hygiene::Hygienic<Timed<File>>;

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
    fn histogram_buckets_cover_every_value_and_the_error_is_bounded() {
        let mut last = 0;
        for ns in [
            0u64,
            1,
            7,
            8,
            9,
            15,
            16,
            17,
            100,
            1_000,
            12_345,
            1 << 20,
            3_000_000_000,
        ] {
            let b = bucket_of(ns);
            assert!(b >= last, "buckets must not go down: {ns}");
            last = b;
            let upper = bucket_upper(b);
            assert!(
                upper > ns,
                "{ns} must lie below its bucket's upper edge {upper}"
            );
            if ns >= 8 {
                // 8 buckets per power of two: at most 12.5% above.
                assert!(upper as f64 <= ns as f64 * 1.126 + 1.0, "{ns} -> {upper}");
            }
        }
        assert_eq!(bucket_of(u64::MAX), BUCKETS - 1);
    }

    #[test]
    fn quantiles_pick_the_right_bucket() {
        let h = Histogram::default();
        assert_eq!(h.quantile(0.5), None);
        for _ in 0..90 {
            h.record(1_000);
        }
        for _ in 0..10 {
            h.record(1_000_000);
        }
        let p50 = h.quantile(0.50).unwrap();
        let p95 = h.quantile(0.95).unwrap();
        assert!(p50 >= Duration::from_nanos(1_000) && p50 < Duration::from_nanos(1_200));
        assert!(p95 >= Duration::from_nanos(1_000_000) && p95 < Duration::from_nanos(1_200_000));
    }

    #[test]
    fn a_lane_splits_its_time_into_exclusive_parts() {
        let stats = Stats::new();
        {
            let _lane = lane(Some(&stats));
            let mut r = Timed::new(Cursor::new(vec![0u8; 8]), Some(stats.clone()));
            let mut buf = [0u8; 8];
            r.read_exact(&mut buf).unwrap();
            // A slow input counts as waiting for input.
            timed(Some(&stats), Class::Input, || {
                std::thread::sleep(Duration::from_millis(30))
            });
            // Time not claimed by any part is the thread's own work.
            std::thread::sleep(Duration::from_millis(30));
            // Writes inside a finishing span belong to `finish`, once.
            let mut w = Timed::new(Vec::new(), Some(stats.clone()));
            timed(Some(&stats), Class::Finish, || {
                w.write_all(b"tail").unwrap();
                std::thread::sleep(Duration::from_millis(20));
            });
            // Time the control made this thread sleep is not its own work either.
            std::thread::sleep(Duration::from_millis(10));
            note_parked(10_000_000);
        }
        let s = stats.snapshot(Duration::ZERO);
        assert!(s.wait_input >= Duration::from_millis(30), "{s:?}");
        assert!(s.busy >= Duration::from_millis(25), "{s:?}");
        assert!(s.finish >= Duration::from_millis(20), "{s:?}");
        assert!(s.wait_output < Duration::from_millis(5), "{s:?}");
        assert_eq!(s.write_bytes, 4);
        // No part is counted twice: together they do not exceed what the thread ran for.
        let all = s.busy + s.wait_input + s.wait_output + s.finish + Duration::from_millis(10);
        assert!(all <= Duration::from_millis(200), "{s:?}");
    }

    #[test]
    fn worker_lanes_add_up_across_threads() {
        let stats = Stats::new();
        std::thread::scope(|scope| {
            for _ in 0..3 {
                scope.spawn(|| {
                    let _lane = lane(Some(&stats));
                    timed(Some(&stats), Class::Output, || {
                        std::thread::sleep(Duration::from_millis(20))
                    });
                });
            }
        });
        let s = stats.snapshot(Duration::ZERO);
        assert!(s.wait_output >= Duration::from_millis(60), "{s:?}");
    }

    #[test]
    fn spans_and_lanes_do_nothing_when_stats_are_off() {
        let _lane = lane(None);
        assert_eq!(timed(None, Class::Finish, || 7), 7);
        assert_eq!(fs(None, FsOp::MkdirOut, || 8), 8);
        let r: Result<u8> = time_file(None, || Ok(9));
        assert_eq!(r.unwrap(), 9);
    }

    #[test]
    fn only_successful_files_are_counted_and_the_dominant_stage_is_the_largest() {
        let stats = Stats::new();
        let ok: Result<()> = time_file(Some(&stats), || Ok(()));
        let failed: Result<()> = time_file(Some(&stats), || Err(io::Error::other("no").into()));
        assert!(ok.is_ok() && failed.is_err());
        let s = stats.snapshot(Duration::ZERO);
        assert_eq!(s.files, 1);
        assert_eq!(s.dominant(), None);

        let s = StatsSnapshot {
            busy: Duration::from_millis(10),
            wait_output: Duration::from_millis(30),
            parked: Duration::from_millis(10),
            ..StatsSnapshot::default()
        };
        let (stage, share) = s.dominant().unwrap();
        assert_eq!(stage, Stage::WaitOutput);
        assert!((share - 0.6).abs() < 1e-9);
        // A tie goes to the earlier stage.
        let s = StatsSnapshot {
            busy: Duration::from_millis(5),
            wait_input: Duration::from_millis(5),
            ..StatsSnapshot::default()
        };
        assert_eq!(s.dominant().unwrap().0, Stage::Cpu);
    }

    #[test]
    fn counts_and_queue_samples_land_in_their_own_counters() {
        let stats = Stats::new();
        fs(Some(&stats), FsOp::OpenSrc, || ());
        fs(Some(&stats), FsOp::CreateOut, || ());
        fs(Some(&stats), FsOp::StatOut, || ());
        fs(Some(&stats), FsOp::MkdirOut, || ());
        fs(Some(&stats), FsOp::RenameOut, || ());
        stats.count(FsOp::StatSrc, 4);
        stats.note_queue(4);
        stats.note_queue(10);
        let s = stats.snapshot(Duration::ZERO);
        assert_eq!((s.opens, s.stat_calls, s.mkdirs, s.renames), (2, 5, 1, 1));
        assert_eq!((s.queue_samples, s.queue_max), (2, 10));
        assert_eq!(s.queue_avg(), Some(7.0));
    }

    /// Not a test: the method behind docs/reports/stats-overhead-v2.md. Run with
    /// `cargo test --release -p zstar-core --lib stats::tests::span_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "measurement, not a check"]
    fn span_cost() {
        use std::hint::black_box;
        const N: u32 = 5_000_000;
        let per = |label: &str, f: &mut dyn FnMut()| {
            f(); // warm up
            let t = Instant::now();
            for _ in 0..N {
                f();
            }
            let ns = t.elapsed().as_nanos() as f64 / f64::from(N);
            println!("{label:<34} {ns:6.1} ns");
        };
        let on = Stats::new();
        let _lane = lane(Some(&on));
        per("empty call (reference)", &mut || {
            black_box(());
        });
        per("fs span, stats off", &mut || {
            black_box(fs(None, FsOp::StatOut, || black_box(1)));
        });
        per("fs span, stats on", &mut || {
            black_box(fs(Some(&on), FsOp::StatOut, || black_box(1)));
        });
        per("timed_write, stats off", &mut || {
            let _ = timed_write(None, 8, || Ok(()));
        });
        per("timed_write, stats on", &mut || {
            let _ = timed_write(Some(&on), 8, || Ok(()));
        });
        per("time_file, stats off", &mut || {
            let _ = time_file(None, || Ok(()));
        });
        per("time_file, stats on", &mut || {
            let _ = time_file(Some(&on), || Ok(()));
        });
        per("flush_live, not due", &mut || {
            flush_live();
        });
        let mut b = [0u8; 1];
        let mut plain = Timed::new(io::repeat(0), None);
        per("Timed::read 1 byte, stats off", &mut || {
            let _ = black_box(plain.read(&mut b));
        });
        let mut timed_reader = Timed::new(io::repeat(0), Some(on.clone()));
        per("Timed::read 1 byte, stats on", &mut || {
            let _ = black_box(timed_reader.read(&mut b));
        });
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
