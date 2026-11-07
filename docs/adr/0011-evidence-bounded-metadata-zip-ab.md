# 0011: Better evidence, bounded metadata memory, zip upgrade as an A/B

- Status: Accepted (Parts 1 to 3)
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

## Part 2: bounded metadata memory

Decisions:

- **The source tree is streamed.** A walker thread produces entries in the same order as before
  (inputs in order, parents before children, siblings sorted per directory) and hands them to
  the format through a bounded queue (1,024 entries plus two batches of 128 in flight). The
  format reads `SourceStream::next_entry`; `ArchiveFormat::create` stays as a default method
  over a slice for callers that have a list. A walker that dies is never mistaken for the end
  of the tree. Only one directory's sorted names are held per level of depth.
- **Entries travel in batches of 128.** The first version sent one entry per message and cost
  +20% wall time on 100,000 files (paired run, same machine state): every hand-over woke the
  other thread. Batching brought it to +1 to +5% on zip and 7z, and tar.gz is faster than
  before.
- **ZIP extraction keeps the whole preflight before the first write** (zip-slip, names, entry
  and size limits, case collisions) but stores each name once in an arena with 24 bytes per
  entry. Paths, modes and times are derived again at extraction (the same checks run twice
  on purpose). Collision keys are 64-bit keyed hashes of the lowercase path; a hash collision
  between two different paths only forces serial extraction, which is always safe, so it
  cannot hide a real collision.
- **Four limits, all errors when exceeded, never a skipped check**: `max_entries` (existing),
  and new `max_path_bytes` (32 KiB), `max_depth` (1,024) and `max_metadata_bytes` (1 GiB). The
  last one counts the walker's directory listings, the zip plan, and what the zip and 7z
  writers keep until they finish (name, link target and an estimated 128 bytes per entry).
  They apply to creating and to extracting, have CLI flags (`--max-path`, `--max-depth`,
  `--max-metadata`), config keys under `[limits]`, and `--no-limits` turns them off with the
  others. **The defaults are my choice, not the owner's**: they are well above any real
  tree, below anything that exhausts a desktop, and easy to change.
- **Progress totals are unknown while the tree is walked** (`None`), and known when the walk
  ends. A progress bar with a percentage cannot show it before then. The bytes done and
  entries done are exact throughout.
- **Cancelling during the walk now works** (3 s to 5 ms at 1M entries).

Numbers: [metadata-streaming.md](../reports/metadata-streaming.md). At 1M entries peak RSS
falls from 449 to 28 MiB for tar.gz creation and from 585 to 259 MiB for zip; zip extraction
from 568 to 405 MiB; time to first progress from seconds to milliseconds; archives are
byte-identical to before; `tests/security.rs` is unchanged and passes.

Consequences and limits:

- **Memory is still O(entries) in four places**: the zip writer (about 230 to 260 bytes per
  entry), the 7z writer and its list (7z needs every entry before its first byte), the `zip`
  crate's copy of the central directory held by each extraction worker (the 400 MiB left in
  zip extraction), and a single huge directory (its names are held to sort them). The first
  and the third are for Task 3.
- Allocation calls in zip extraction rise 13% to 23% from deriving paths again; user CPU did
  not rise. Extraction wall time on the test VM cannot be resolved (the same binary takes 94 s
  or 141 s on the same input), so none is claimed.
- The walker is a second thread; it shows in `--stats` thread-seconds as its own lane. Time it
  spends waiting for a full queue is classed as idle and is not counted anywhere.
- Not measured: spinning disks, cold cache, Windows, macOS, Android.

## Part 3: the zip crate upgrade, A/B

Decisions:

- **`zip` stays pinned at `=9.1.3`.** 9.4.1 (newest, one to two days old when tested) was run
  on identical inputs in branch `zip-upgrade-ab` (not merged): same archives (plain ones byte
  for byte), verified by 7-Zip, Python and extraction compared with `diff -r`; no speed gain
  (within 1%, Deflate +1.5% to +3.8% on two workloads), no memory change, and a second
  `lzma-rust2` in the lock file. All tests, including `tests/security.rs`, pass on it.
- **The `rayon` feature and upstream's `par_append_*` / `par_extract` are not used.** They are
  the primitive Zstar already uses plus rayon, and they break four rules: memory (a 4 GiB file
  peaks at 2.4 GiB against 14 MiB), cancellation and live control, streaming (they take the
  whole list), and deterministic order (completion order; output is not identical between
  runs). They cannot be wrapped. The prepared-entry API cannot encrypt, so encrypted creation
  stays serial.
- **Measured in Zstar's own code, not changed here**: creation of mixed sizes compresses in
  waves that keep about one thread busy (w3 Deflate 15.4 s against 8.7 s for upstream's
  continuous pipeline); encrypted creation of 10,000 small files takes 10.2 s against 0.61 s
  plain on one core; a 1 MiB read buffer for zip extraction doubles the cost on small entries;
  independent readers per worker are worth 2 to 6% on a warm cache.

Numbers, method and what was not measured: [zip-upgrade.md](../reports/zip-upgrade.md).

Consequences: no code or dependency change from this part. Three follow-ups need the owner's
decision: a continuous ordered-window pipeline for zip creation (the zip counterpart of the
postponed pargz pipeline), writing AES entries ourselves to parallelise encryption (a format
and security decision), and a reader factory for zip extraction (only with HDD or network
numbers). Not measured: HDD, NVMe, cold cache, Windows, macOS, Android, Zstd in any third-party
reader (7-Zip 23.01 does not support the method).
