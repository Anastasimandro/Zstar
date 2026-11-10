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
   the median. Up is kept above +5%; down is kept if it costs under 2%. Fresh start doubles each
   knob while it pays (slow start); later probes move one step, every ~9 s in the foreground. A
   change over 25% at the same knobs means the machine changed: explore again.
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
