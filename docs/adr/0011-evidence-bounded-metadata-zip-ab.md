# 0011: Better evidence, bounded metadata memory, zip upgrade as an A/B

- Status: Accepted (Part 1 only; Parts 2 and 3 are added by the commits that do them)
- Date: 2026-11-05
- Changes: section 5 (engine) of [`../DECISIONS.md`](../DECISIONS.md); builds on [0010](0010-measure-first-adaptive-io.md)

## Context

A code review found four things worth fixing or measuring: zip extraction reads through a
shared reader and a fixed 64 KiB buffer; encrypted zip creation and zip entries over 1 MiB are
serial; gzip works in waves where reading, compressing and writing do not overlap; and the
whole source tree and the whole ZIP plan live in memory. The only real measurement so far is a
2-vCPU VM where 1 GiB gzip was CPU-bound. There are no HDD, NVMe or Android numbers. Nothing
below claims a gain that has not been measured.

## Part 1: better evidence (`--stats` version 2)

Decisions:

- **Thread time is split into exclusive parts**: `busy`, `wait_input`, `wait_output`,
  `wait_workers`, `finish` (and `parked`, from the control). Each thread tallies in a
  thread-local cell and adds to the shared counters once, when it ends. No lock, relaxed
  atomics, nothing per read or write beyond what version 1 already did. `busy` is what is left
  of the thread's time, so untimed work lands there on purpose.
- **Every operation reports its dominant stage** (`dominant_stage`, `dominant_share_pct`),
  tested for every format, create and extract.
- **Buffered is not complete.** No `fsync` is issued anywhere. Closing a file, the final flush
  and the rename are `finish`; the block ends with `write_completion: buffered`, and
  `stats.rs` says so next to every number it applies to. A physical-completion number would
  need `fsync` or unbuffered I/O, which is out of scope.
- **Counts and latency**: `fs_opens`, `fs_stats`, `fs_mkdirs`, `fs_renames` (calls, as defined
  in `stats.rs`), `files`, `files_per_s`, per-file p50 and p95 from a log-linear histogram (8
  buckets per power of two, so at most 12.5% above the true value, 2.5 KiB per operation), and
  queue occupancy (average and maximum) sampled when a gzip wave or a zip batch starts.
- **Compatibility**: `stats_version: 2` only appends keys; the 19 keys of version 1 keep their
  meaning and order.
- **Cost with stats off** is the check of an `Option` per instrumented call (about 11 per file
  when extracting): 0.6 ns measured per call. With stats on it is about 75 to 80 ns per call.

Numbers: [stats-overhead-v2.md](../reports/stats-overhead-v2.md). On 1 GiB gzip and zip create
no difference is visible against the baseline; on zip extract of 20,000 tiny files the run-to-run
spread (0.6 to 2.9 s) is larger than any effect, so only the bound by counting stands (at most
about 3% with stats on, on the cheapest files).

Consequences and limits:

- Threads inside third-party codecs (7z LZMA2, zstd and xz multi-thread) are not seen; the
  thread that drives them shows their time as `busy`.
- Queue occupancy is sampled at the start of a wave or batch, not time-weighted, and only gzip
  and zip have a queue.
- 7z creation is solid-block based, so it reports no per-file latency.
- The new numbers showed that zip extraction reads 380 times the archive size from the OS on
  10,000 tiny entries (a fixed 64 KiB buffer refilled per entry). Recorded, not changed here:
  it belongs to Part 3, where the read buffer is a variable.
