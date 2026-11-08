# zip 9.1.3 -> 9.4.1: A/B, and what the new parallel API is worth to Zstar

Branch `zip-upgrade-ab` (not merged). Task 3 of ADR 0011. **Recommendation at the end.**

## Versions and facts checked

- Pinned today: `zip = "=9.1.3"`. Newest on crates.io (checked 5 to 7 Nov 2026): **9.4.1**,
  published 1 to 2 days before the runs; 9.2.0 (31 Oct) is the one that added parallel
  compression, batch append, multi-threaded extraction and the index-free reader
  (`ZipIterable`). Licence unchanged (MIT). I read the 9.4.1 source and `CHANGELOG.md` from the
  crate tarball; I could not open the GitHub release pages from here.
- 9.4.1 passes the whole test suite unchanged, including `tests/security.rs`.
- What it adds to `Cargo.lock`: a second `lzma-rust2` (0.25 next to the 0.23 that `sevenz-rust2`
  uses; `cargo deny` only warns on duplicates) and, **only if the `rayon` feature is turned on**,
  `rayon`, `rayon-core`, `crossbeam-deque`, `crossbeam-epoch`, `crossbeam-utils` and `either`.
  The branch turns it on so the benchmark can run; that is a new dependency, which is the
  owner's decision, and the branch must not be merged with it unasked. `cargo deny` was **not
  run** (not installed here; CI runs it).
- What the new API is, from the source:
  - `ZipFileBuilder` / `PreparedZipFile` (compress a whole entry into memory, append it later)
    is what Zstar already uses for small files in 9.1.3. `par_append_from_paths` is that same
    primitive plus rayon.
  - **It cannot encrypt**: `ZipFileBuilder::new` returns `"Encrypted files cannot be compressed
    outside of a ZipWriter"`. The serial encrypted path stays.
  - `PreparedZipFile` holds the whole compressed entry in a `Box<[u8]>`.
  - `par_append_from_paths` takes the whole list up front and appends in **completion order**.
  - `ZipArchive::par_extract` writes files by itself.

## Method

- Same machine as the Task 2 report (2 vCPU VM, warm cache, no HDD/NVMe/Android), and the runs
  are **interleaved** A/B so both builds see the same machine state. The VM was moved between
  hosts during the work, so only numbers from the same table are compared.
- Real CLI (`zstar compress/extract --drive ssd`, two threads), wall time, CPU time
  (user+sys, `wait4`) and peak RSS (`ru_maxrss`) of the whole process.
- Workloads (`scratchpad` generator, deterministic, about 2 to 3x compressible):
  **w1** 10,000 files of 4 KiB; **w3** mixed, 2,173 files and 851 MiB (2,000 of 1 to 64 KiB,
  150 of 0.1 to 1 MiB, 20 of 4 to 32 MiB, 3 of 100 to 128 MiB); **w2** one 4 GiB file.
  Each with Deflate and Zstd, plain and AES-256. 5 repetitions for w1, 3 for w3, 1 to 2 for w2.

## Output is the same, and verified

All 16 archives of w1 and w3 (Deflate/Zstd, plain/AES, both versions): extracted by **both**
Zstar builds and compared with `diff -r` against the source: all equal. 7-Zip 23.01 (`7z t`)
and Python (`zipfile`, `pyzipper` for AES) accept every Deflate archive, plain and AES.
**7-Zip 23.01 does not support the Zstd method** (it fails the same way on 9.1.3 and 9.4.1
archives), so Zstd archives were verified only by Zstar. Plain archives from both versions are
**byte for byte identical** (`cmp`). The 4 GiB Deflate archive (Zip64): `7z t` Everything is
Ok, Python `testzip()` None, extraction and `cmp` with the source equal.

## Creation: the upgrade alone changes nothing

Same Zstar code, only the crate differs. Wall seconds, median.

| workload | method | plain 9.1.3 -> 9.4.1 | AES-256 9.1.3 -> 9.4.1 | peak RSS MiB (plain) |
|---|---|---|---|---|
| w1 10,000 x 4 KiB | Deflate | 0.61 -> 0.63 | 10.18 -> 10.21 | 14 |
| w1 | Zstd | 0.47 -> 0.45 | 10.10 -> 10.22 | 16 -> 17 |
| w3 mixed 851 MiB | Deflate | 15.12 -> 15.70 (+3.8%) | 18.94 -> 18.96 | 21 |
| w3 | Zstd | 6.27 -> 6.34 | 9.21 -> 9.30 | 35 -> 32 |
| w2 1 x 4 GiB | Deflate | 79.5 -> 80.7 (+1.5%) | 82.6 -> 83.0 | 14 |
| w2 | Zstd | 33.1 -> 33.4 | 36.1 -> 36.3 | 14 |

Within 1% except Deflate on w3 (+3.8%, three runs each, consistent) and on w2 (+1.5%). No memory
change. **No gain anywhere.**

Two things the table shows that matter more than the upgrade:

- **Encrypted creation is serial and 17x slower for small files**: w1 AES 10.2 s against 0.61 s
  plain, with CPU time equal to wall time (one core). Consistent with WinZip-AES deriving a key
  per entry (PBKDF2, about 1 ms each: 10,000 entries is about 10 s); I did not profile it.
  The new API does not remove it (it cannot encrypt outside the writer). Encrypted
  *extraction* is parallel: w3 AES extracts in 3.4 s against 18.9 s to create it.
- **The 4 GiB single file is one thread** (CPU = wall) and uses 14 MiB: bounded, and nothing in
  the crate can parallelise one entry.

## Upstream's parallel creation, measured (benchmark only)

`par_append_from_paths` over the same files, no Zstar code around it (`examples/zip_par.rs`):

| workload | Zstar 9.1.3 wall / RSS | upstream parallel wall / RSS |
|---|---|---|
| w1 Deflate | 0.61 s / 14 MiB | 0.60 s / 10 MiB |
| w1 Zstd | 0.47 s / 16 MiB | 0.43 s / 12 MiB |
| w3 Deflate | 15.4 s / 20 MiB | **8.7 s** / 199 to 214 MiB |
| w3 Zstd | 6.3 s / 30 MiB | **3.6 s** / 171 MiB |
| w2 (4 GiB) Deflate | 79.5 s / 14 MiB | 83.2 s / **2,465 MiB** |

- On many small files it is the same speed. On mixed sizes it is 1.7x faster, and the reason is
  **not** the crate version: it runs a continuous pipeline, while Zstar compresses in *waves*.
  Zstar's `--stats` on w3 (run with the limit at 64 MiB and batches of 2) show 9.99 s of the
  16.7 s wall spent waiting for workers while those workers were busy for about 10.7
  thread-seconds: roughly one thread at work inside the waves, because a batch waits for its
  slowest entry and the 20 mid-size files are far apart in name order. Raising Zstar's
  small-file limit from 1 MiB to 8, 32 and 64 MiB with a byte budget per batch, or shrinking
  batches to 2, 4 or 8 entries, did **not** help (w3 Deflate 14.8 to 15.3 s against 15.2 s):
  the barrier is the problem, not the limit.
- **It breaks four of Zstar's rules**: memory is not bounded (the 4 GiB file is held compressed:
  2.4 GiB peak); there is no cancellation, priority or live thread control inside it; the list
  of entries must exist in full (undoing the streaming of Task 2); and the archive order is the
  order in which entries finish, so output is not deterministic or identical to before
  (9,997 to 9,999 of 10,000 entries in a different position than Zstar's order, different on
  every run, in the w1 test). So it cannot be wrapped: it can only be **not used**.

## Experiment: a larger small-file limit with a byte budget per batch

Zstar compresses entries up to 1 MiB whole on worker threads, in batches of 32 (`SMALL_FILE`,
`flush_batch`). The experiment raised the limit to 8, 32 and 64 MiB, flushed a batch when the
source bytes in it reached a budget (128, 256, 512 MiB), and streamed each file into the
compressor in 128 KiB chunks so that only the compressed bytes are held. Median of 3
interleaved runs, 2 threads, `--drive ssd`.

| workload | build | wall s | CPU s | peak RSS MiB |
|---|---|---|---|---|
| w3 Deflate | main (9.1.3) | 15.37 | 16.29 | 20 |
| w3 Deflate | limit 1 MiB (control, new read loop) | 15.47 | 16.37 | 15 |
| w3 Deflate | limit 8 MiB, budget 128 MiB | 15.23 | 16.15 | 19 |
| w3 Deflate | limit 32 MiB, budget 256 MiB | 15.06 | 16.25 | 79 |
| w3 Deflate | limit 64 MiB, budget 512 MiB | 14.82 | 16.11 | 77 |
| w3 Zstd | main (9.1.3) | 6.29 | 6.58 | 30 |
| w3 Zstd | limit 1 MiB (control) | 6.45 | 6.84 | 23 |
| w3 Zstd | limit 8 MiB, budget 128 MiB | 6.30 | 6.61 | 27 |
| w3 Zstd | limit 32 MiB, budget 256 MiB | 6.15 | 6.57 | 75 |
| w3 Zstd | limit 64 MiB, budget 512 MiB | 6.18 | 6.63 | 78 |
| w1 Deflate (10,000 x 4 KiB) | all five builds | 0.64 to 0.67 | 1.06 to 1.10 | 14 |

Limit 64 MiB with smaller batches (entries per batch 2, 4, 8 instead of 32), w3 Deflate:
15.27, 15.13 and 14.83 s (RSS 84, 82, 76 MiB); w3 Zstd: 6.27, 6.16 and 6.04 s.

Reading: **raising the limit gains at most 3.6% (15.37 to 14.82 s) and costs about 4 times the
memory (20 to 77 MiB); smaller batches do not help either.** CPU time is the same in every
row, so the extra core is not being used: wave compression keeps about one worker busy
whatever the limit, because a batch ends when its slowest entry ends. The threshold is not the
problem; the barrier between batches is. The threshold change is therefore not shipped; this
result is input to the continuous pipeline (Task A, below). Note: the control row's archive is
55 bytes smaller than main's, because the experimental read loop streams into the builder; the
shipped pipeline keeps the exact current bytes.

## Extraction: independent readers and buffer size

Experiment in the branch (environment switches, not a proposal for the API): one reader shared
behind a `Mutex` (today) against one reader per worker that opens the archive again; read buffer
64 KiB (today), the profile's 128 KiB (SSD), 1 MiB. Median of 3 interleaved runs, wall seconds
(CPU seconds in brackets, which is steadier).

| archive | shared 64K | shared profile | shared 1M | factory 64K | factory profile | factory 1M |
|---|---|---|---|---|---|---|
| w1 (10,000 x 4 KiB) | 1.51 (2.86) | 1.48 (2.71) | 2.88 (4.43) | 1.38 (2.62) | 1.47 (2.72) | 2.22 (4.27) |
| w3 (851 MiB, 2,173 files) | 2.39 (4.59) | 2.33 (4.54) | 2.50 (4.86) | 2.29 (4.44) | 2.22 (4.35) | 2.57 (5.00) |
| w3 AES-256 | 3.47 (6.80) | 3.57 (6.90) | 3.77 (7.21) | 3.38 (6.61) | 3.51 (6.77) | 3.79 (7.43) |
| t100k (100,000 x 64 B) | 20.3 (31.4) | 18.5 (28.4) | 33.5 (51.6) | 24.4 (37.0) | 14.2 (22.1) | 27.7 (48.3) |

- **1 MiB is clearly worse** on many small entries (w1 +90% wall, t100k +60% CPU): every entry
  seeks, the buffer is thrown away and refilled (the 380x read amplification found in Task 1).
- **Shared against independent readers: 2 to 6% on w1 and w3** (best: factory + profile on w3,
  2.22 s against 2.39 s). Small, and the cache is warm; a real disk or a network file is where
  it could matter and it was not measured.
- **t100k cannot be ranked on this VM**: the same configuration took from 5.5 to 38 s between
  runs (nearly all of it kernel time creating 100,000 files); extra runs at 8, 16 and 32 KiB
  were just as scattered. No claim is made for it.
- On w3, buffers of 8, 16, 32 and 64 KiB with either reader all fall between 2.11 and 2.35 s:
  inside the noise.
- Each worker still holds its own copy of the central directory (the ~400 MiB at 1M entries in
  the Task 2 report). Independent readers do not change that; `ZipIterable` (no per-entry
  metadata) could, but needs a worker design of its own. Not tried.
- A source that cannot be reopened (a future Android SAF descriptor) must keep the shared
  reader; the experiment does not touch that path.

## Memory per in-flight entry

Zstar today: entries up to 1 MiB are read whole and compressed into memory, at most 32 per
batch: peak RSS 14 to 35 MiB in every workload above. In the branch, with limits raised to 64
MiB and a 512 MiB budget per batch, peak RSS was 64 to 84 MiB on w3 (the biggest in-flight
entry is the unit, as expected). Upstream's own parallel path: 2.4 GiB on the 4 GiB file.
Nothing in the crate bounds a prepared entry by a memory limit; Zstar would have to.

## What was not measured

HDD, NVMe, cold cache, Windows, macOS, Android; Zstd verified only by Zstar (no third-party
reader available); `par_extract` (rejected by the rules, not run); the `ZipIterable` reader;
a continuous creation pipeline (not built: see below); PBKDF2 as the cause of slow encryption
(inferred from CPU time and entry count, not profiled).

## Recommendation and owner decisions

Decided by the owner after this report (8 Nov 2026):

1. **`zip` stays at `=9.1.3`.** No `rayon` feature, branch `zip-upgrade-ab` is kept for
   reference and is not merged (it carries the new dependency and the environment switches).
2. **The gain is in Zstar's own pipeline.** A continuous ordered window replaces the waves in
   zip creation, as a reusable component, with in-flight memory bounded in bytes. Results:
   see "Continuous pipeline" below once measured. The small-file limit is not changed on its own.
3. **Encrypted creation is not fixed with new crypto.** No own AES, no fork. Cause and numbers
   for the slowness, and the 7z comparison, are measured below.
4. **Extraction**: independent readers are worth 2 to 6% on a warm cache and cost an API change;
   not done without HDD or network numbers. A buffer above 128 KiB for zip is worth avoiding
   (1 MiB doubles the cost on small entries); to be confirmed on real hardware.
