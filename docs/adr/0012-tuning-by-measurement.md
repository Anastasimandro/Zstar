# 0012: Tuning by measurement, in the foreground and in the background

- Status: Accepted (step 1: the pure tuner and its simulator; wiring is the next step)
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
5. **Interference, from our own numbers:** latency per request over 1.5x the best seen at the same
   depth and request size (drive busy), or bytes per busy second under 0.85x the best seen at the
   same worker count (cores busy or throttling), or outside pressure over 0.10. Two contended ticks
   in a row halve workers and depth; no probing for 20 ticks. References expire after 60 s.
6. **Safety:** never above the ceilings, never below one worker and one request; 20 senseless
   samples in a row fall back to 2 workers, 2 requests, 1 MiB. Output bytes do not depend on the
   knobs.
7. **Memory per volume:** `Tuner::best` returns the fastest knobs measured; the cli stores them per
   volume and passes them as `remembered` next time, which skips the slow start.

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
2. Live stats: flush the thread-local tallies of ADR 0011 every ~100 ms; live knobs in `Control`
   (workers already, then depth and request size at safe points).
3. `crates/sys` I/O hygiene: page cache drop (`posix_fadvise(DONTNEED)`) and bounded writeback.
4. `crates/sys` probes: idle time, battery, memory pressure; per-volume memory in the cli.
5. Android: foreground service, resumable operations.
