# 0012: Tuning by measurement, in the foreground and in the background

- Status: Accepted (steps 1 to 4 done: tuner, live stats and governor, cache hygiene, probes and per-volume memory; step 5, Android, waits for the owner, see below)
- Date: 2026-11-09
- Changes: section 5 (engine) of [`../DECISIONS.md`](../DECISIONS.md); supersedes the gates of Parts A to D of [0010](0010-measure-first-adaptive-io.md); builds on [0008](0008-control-handle-and-profiles.md), [0009](0009-drive-detection-and-thread-priority.md) and [0011](0011-evidence-bounded-metadata-zip-ab.md)

## Context

The engine chose buffers, threads and parallel extraction from a "HDD or SSD" label (ADR 0009),
and the label is often unknown or wrong (USB, SD, RAID, network shares, phones). ADR 0010 said
to measure first and regulate later. A label also says nothing about how busy the machine is,
which matters most when the user leaves an operation running in the background.

## Decision

1. **No drive labels in the decision.** `zstar_core::Tuner` (`crates/core/src/tune.rs`) decides
   three knobs from measurement only: workers, drive requests in flight (`io_depth`) and request
   size (`chunk`). The drive kind stays a hint for the first values only.
2. **Profile = ceiling, mode = objective, tuner = decides inside the ceiling.** The caller passes
   `TuneBounds` (from the profile and the memory budget) and a `Mode`:
   - `Foreground`: maximum throughput.
   - `BackgroundActive`: use only what is left; one or two physical cores stay free; halve at once
     on interference, come back one step every ~2 s.
   - `BackgroundAway`: maximum throughput; the first user input yields and switches to
     `BackgroundActive`.
   Ceilings can change mid-operation (`Tuner::set_bounds`: profile change, battery, heat);
   lower ceilings apply at once.
3. **The tuner is pure.** One `Sample` per tick (~100 ms: bytes, I/O operations, I/O wait, busy
   time, optional outside pressure, optional user input); it returns the knobs to apply at the
   next safe point. No clock, thread or OS call in it; `#![forbid(unsafe_code)]` holds.
4. **Throughput:** one knob at a time; a window is 8 ticks after 1 settling tick and its value is
   the median. A step is kept above +5%; in the background a step down is also kept if it costs
   under 2% (same speed, less pressure), but when somebody waits for the result a tie is not
   kept. Fresh start explores each knob in turn: up doubles while it pays, a worker or request
   count steps down by a quarter, request size halves; later probes move one step, every ~9 s in
   the foreground. Nothing is tried before 30 ticks of work (about 3 s), and a trial more than
   10% below its base is reverted after 3 ticks instead of a whole window. A change over 25% at
   the same knobs means the machine changed: explore again. *(Changed 2026-11-13, see the update
   at the end: it used to keep ties, halve counts and start trying at once.)*
5. **Interference, from our own numbers:** latency per request over 1.5x the typical value at the
   same depth and request size (drive busy), or CPU time under 0.85x of busy time (a thread
   waited for a core), or outside pressure over 0.10. Two contended ticks in a row halve workers
   and depth; no probing for 20 ticks. Each signal is the median of the last three ticks. A
   reference is a plain mean of its first 6 samples, then a moving average (weight 0.1) of clean
   ticks only, and it expires after 60 s without a clean tick. It is the typical value, not the
   best seen: the best of many noisy ticks sits in the tail of the noise and fires on jitter
   (measured in the simulator with 25% latency jitter). When `Sample::cpu` is missing, bytes per
   busy second under 0.85x the typical value stands in for the CPU test; it cannot tell a
   competitor from data that compresses slower, so `crates/sys` should supply `cpu`.
6. **Safety:** never above the ceilings, never below one worker and one request; 20 senseless
   samples in a row fall back to 2 workers, 2 requests, 1 MiB. Output bytes do not depend on the
   knobs.
7. **Memory per volume:** `Tuner::best` returns the knobs the last finished trial settled on, not
   the fastest window (which favours whatever file mix was streaming); the cli stores them per
   volume (`--tune`, file `volumes.tsv` in the state directory, newest first, 64 volumes) and
   passes them to `Tuner::with_start` next time: the run starts there and still explores, because
   a volume key is built from device names the OS may reuse, so a wrong hit costs a few ticks and
   never a wrong result. `Tuner::new` with `remembered` (skip exploring) is not used by the cli.
   Without an entry the run starts from the fixed profile of the drive class (update 2026-11-13).
   Keys start with a version (`v2:`); entries under another version are never found.
8. **User input** is read before anything else in a tick: an idle tick between files must not
   swallow the one-tick event that ends `BackgroundAway`.

## Acceptance (simulated now, measured later)

- Foreground: at least 90% of the best fixed setting on every simulated drive (NVMe, SATA SSD, HDD,
  SD, USB 2, network share, slow CPU), within 15 s. In the simulator: 95 to 100%.
- Background: no yield without a competitor; yields within 0.5 s to a drive or CPU competitor; the
  competitor keeps at least 80% of its speed (simulated: 89%); recovery to at least 70% afterwards.
- On real hardware, before claiming any gain: foreground p95 of the user's work within +10% while a
  background operation runs, and at least 90% of the foreground speed when nobody is at the machine.

## Risks

- Latency per request also changes with the file mix (many small files versus large ones). Two
  ticks in a row and expiring references limit false yields; real measurements decide whether the
  reference needs to be per file-size class.
- A competitor present for longer than the reference lifetime becomes the new normal. Acceptable:
  by then the operation already runs at the floor it reached while yielding.
- Linux CPU and I/O PSI include this operation's own threads, so only pressure it did not cause may
  be passed as `Sample::pressure`.

## Build order

1. Pure tuner and simulator with tests (this ADR, done).
2. Live stats and governor (done): `zstar_core::start_governor` runs a thread that every 100 ms
   turns the counter deltas into a `Sample`, steps the `Tuner` and puts the answer on the
   `Control` (`Running::set_mode` and `set_bounds` change the objective and ceilings live).
   Threads hand their tallies over every 25 ms at `check_cancel`. What is live now:
   - workers: `Control::set_threads`, at the next file or wave, as before;
   - request size: `Control::set_chunk`, at the next request, in the extraction copy loop and in
     single-file creation; the buffers are allocated at `Control::set_chunk_ceiling` (the
     tuner's `max_chunk`, which the caller derives from its memory budget);
   - `io_depth`: stored on the control (`Control::io_depth`); its consumer is the bounded
     writeback of step 3 (a worker may have this many requests written and not yet handed to
     the drive). This engine reads and writes synchronously, so a deeper queue is not a
     separate read-ahead.
   Not live yet: the zip creation pieces, which keep the buffer they start with because the
   pipeline promises identical bytes for the batched and the streaming path.
3. I/O hygiene (done, opt-in): `zstar_sys::cache_op` (Linux: `sync_file_range` and
   `posix_fadvise(DONTNEED)`) behind `Options::cache_hook`; core decides when, in windows of
   `io_depth` x chunk (module `hygiene`), for extraction output, the archive being created and
   big sources. `--cache-hygiene` in the cli, off until measured on real drives. On a virtual
   disk: pending writes 750 MiB -> 4 MiB, cache growth 2.9 GiB -> 13 MiB, same bytes
   (docs/reports/cache-hygiene.md). Windows, macOS and Android: no calls yet.
4. Probes and per-volume memory (done, opt-in): `zstar_sys::{memory_pressure, on_battery,
   user_idle, process_user_cpu}` (Linux: PSI, sysfs, getrusage; Windows: GetSystemPowerStatus,
   GetLastInputInfo, GetProcessTimes; `None` elsewhere, never a guess; Linux has no idle probe
   that works on every desktop). The governor turns idle time into user input and into
   `BackgroundAway` after 60 s, and halves the worker ceiling on battery. `zstar --tune`
   (compress and extract) starts it with ceilings from the profile, drive kind and RAM, and
   saves `Tuner::best` after a successful run. Numbers: `docs/reports/tune-ab.md`.
5. Android: foreground service, resumable operations. Not started: there is no
   `platform-android` crate yet, DECISIONS section 7 asks for a spike and the UI ladder first,
   and a foreground service needs a Java/Kotlin piece (level B or C, owner's approval). The core
   side is ready for it: the governor takes `Signals` from a hook, so the app feeds battery,
   thermal and memory state in; the OS probes in `zstar-sys` return `None` on Android today.

## Update 2026-11-12: real HDD measurements and what follows from them

Windows 10, SATA HDD, `drive-info` says `unknown` (the seek-penalty query fails with os error 31,
see ADR 0009). Wall time without the tuner, then with it:

| case | no tuner | tuner | result |
| --- | --- | --- | --- |
| 1 GiB, one file | 119 s | 65 s | tuner wins (-45%) |
| gentle, 20,000 files of 16 KiB, user active; mean of 2 runs per side | 464 s | 189 s | tuner wins (-59%) |
| normal, 20,000 small files, warm cache; 2 of 3 runs | 15 to 16 s | 22 to 24 s | tuner loses (about +45%) |
| cold cache, small files | 54 / 403 / 504 s | erratic with and without it | no conclusion: `wait_input` about 90%, `read_ms` only about 3 s |

Decisions:

1. **Normal profile: tuner off by default, on only for large files.** Provisional rule: average
   file size of at least 1 MiB, kept as one constant. The small-file regression is not explained
   yet and is not fixed blind. *Status: decided, not implemented.* The average is not known when
   the tuner would have to start (the tree is walked while the format writes; extraction knows
   totals only for formats with a central directory), so the mechanism is open and waits for the
   owner.
2. **Background mode is gentle and the tuner, always together.** `--profile gentle` implies
   `--tune` for compress and extract; there is no standalone gentle. *Done.*
3. **Diagnose, do not fix.** `--stats` (version 4) adds the time inside open and stat calls and
   the slowest of each (`fs_open_ms`, `fs_open_max_ms`, `fs_stat_ms`, `fs_stat_max_ms`), and every
   knob change of the tuner with its reason (`tuner_changes`, `tuner_change_<n>`). They are
   there to explain the small-file loss and the cold `wait_input`. *Done.*
4. **Drive detection.** The seek-penalty failure has no confirmed fallback yet; `drive-info` now
   prints two other properties when it fails (ADR 0009). *Diagnostic only.*

Until 1 is implemented the tuner stays opt-in (`--tune`) outside gentle, as before.

## Update 2026-11-13: the tuner must never lose to the fixed profile

Measured by the owner on NVMe (Windows), wall time without the tuner, then with it: 1 GiB, one
file, 21.6 s -> 24.4 to 25.6 s; gentle with the user active, 35.7 s -> 43.0 s. On the HDD the tuner
wins (table above). In the NVMe run the first `tuner_change` started at workers=1, chunk=65536,
while the fixed profile starts at 4 workers.

**Cause.** Found by reproducing it in the simulator before changing anything (test
`flat_throughput_leaves_the_knobs_where_they_started_outside_the_background`: it failed, with the
knobs ending at 1 worker, 1 request and the smallest chunk). When throughput does not depend on
the knobs (many small files, or a bottleneck elsewhere such as the cost of an open), every step
down costs under 2% and was kept, and slow start kept halving each knob. Workers, requests in
flight and request size walked to their floors within about 15 s. `Tuner::best` then returned
that floor and the cli saved it for the volume (decision 7), so the next run, large files
included, started at the minimum and spent its first seconds climbing. What is not confirmed: the
owner's `volumes.tsv` was not inspected; chunk=65536 is the cli's smallest request size, which
fits.

**Changes.**

1. *Start from the fixed profile.* `tuning::fixed_start`: workers and request size are what the
   untuned run uses (`Options::settings`: 128 KiB on SSD, 1 MiB on HDD and unknown); the fixed
   profile has no queue limit, so requests in flight start at the deepest queue the class allows
   (32, or 4 on HDD and unknown). Used when the volume has no remembered entry. Volume keys now
   start with `v2:`, so entries saved by the old tuner, which may be floors, are never used.
2. *Short operations are not explored.* The first trial waits for 30 ticks of work (about 3 s):
   `EXPLORE_AFTER_TICKS` in `tune.rs`, the one threshold. An operation shorter than that runs
   entirely on the fixed profile. Interference handling is not delayed.
3. *Smaller steps, faster revert.* A step down gives up a quarter of a worker or request count
   (at least 1) instead of half. Up still doubles: with +25% steps the climb stalled below the
   optimum in the simulator (HDD chunk stopped at 2.5 MiB of 4; an NVMe run from the minimum
   stopped at 7 workers), because each small step gains under the 5% threshold. A trial that runs
   10% below its base after 3 ticks is reverted then, not after the 9 of a window.
4. *No ties outside the background.* A step is kept only for a gain over 5%. The background keeps
   its rule (a step down under 2% cost is kept). `Tuner::best` therefore moves only on a real
   gain, and a flat run remembers its start.

**Simulator, not hardware.** Time with the tuner divided by time on the fixed profile of the
class, 10 seeds, median / worst; operations that take 2, 30 and 120 s on the fixed profile. The
fixed profile is the one `fixed_start` gives: whole pool, queue 32, 128 KiB for nvme, sata_ssd,
net and slow_cpu; three quarters of the pool, queue 4, 1 MiB for hdd, sd and usb2.

| machine | 2 s | 30 s | 120 s |
| --- | --- | --- | --- |
| nvme | 1.000 / 1.000 | 1.003 / 1.003 | 1.002 / 1.002 |
| sata_ssd | 1.000 / 1.000 | 1.000 / 1.000 | 1.000 / 1.000 |
| net | 1.000 / 1.000 | 1.000 / 1.000 | 1.000 / 1.000 |
| slow_cpu | 1.000 / 1.000 | 1.003 / 1.003 | 1.002 / 1.003 |
| hdd | 1.000 / 1.000 | 0.373 / 0.375 | 0.227 / 0.228 |
| usb2 | 1.000 / 1.000 | 0.625 / 0.627 | 0.570 / 0.570 |
| sd | 1.000 / 1.000 | 0.837 / 0.837 | 0.810 / 0.811 |

The simulator tests (`the_tuner_is_never_five_percent_slower_than_the_fixed_profile`, and
`the_tuner_keeps_its_gain_where_the_fixed_profile_is_wrong` for at most 0.8 on hdd and usb2) keep
this from regressing. The simulator's fixed SSD profile is already the best setting, so its 1.000
says only that the tuner does not make a good start worse; it says nothing about real drives.

**Real hardware (to measure; the acceptance of this change).** Never more than 5% slower than the
fixed profile on SSD or HDD, and the HDD gains kept. Same runs as the measurements above, release
build, with and without `--tune`, the old `volumes.tsv` irrelevant (new keys):

| case | no tuner | tuner before | tuner after |
| --- | --- | --- | --- |
| NVMe, 1 GiB, one file | 21.6 s | 24.4 to 25.6 s | to measure |
| NVMe, gentle, user active | 35.7 s | 43.0 s | to measure |
| HDD, 1 GiB, one file | 119 s | 65 s | to measure |
| HDD, gentle, 20,000 x 16 KiB, user active | 464 s | 189 s | to measure |
| HDD, normal, 20,000 small files, warm cache | 15 to 16 s | 22 to 24 s | to measure |

**Risks.**

- The HDD gains were measured with a tuner that kept ties. If some of them came from steps worth
  under 5% each, they shrink now. The HDD rows above are the check; if one falls short, the
  threshold for a step down on a drive that thrashes is the first thing to look at.
- `EXPLORE_AFTER_TICKS` (3 s) is a first guess. A longer delay protects short operations more and
  delays the HDD gain by the same amount; the real runs decide.
- The start queue depth differs from the fixed profile's (none). Its only consumer today is the
  cache-hygiene writeback, which is off by default.
- The normal profile still has the tuner off by default (update 2026-11-12, decision 1); this
  change does not touch that, and the small-file loss on the HDD is not explained by it.
