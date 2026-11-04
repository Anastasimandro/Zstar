# 0010: Measure first, then regulate by measurement

- Status: Accepted (Part 1 only; Parts A to D are gates, not commitments)
- Changes: section 5 (engine: disk detection, profiles) of [`../DECISIONS.md`](../DECISIONS.md); builds on [0008](0008-control-handle-and-profiles.md) and [0009](0009-drive-detection-and-thread-priority.md)

## Context

The engine chooses buffers, thread counts and parallel extraction from a "HDD or SSD" label.
On a real Windows laptop with a SATA HDD, without elevation, the seek-penalty query fails with
error 31 (the bus query works), so detection returns `Unknown`. The same blindness exists for
USB sticks, SD cards, RAID, network shares, virtual disks and phone storage, where no label is
right. A label also says nothing about how busy the drive is right now.

## Decision

- **Direction.** The engine will stop depending on the label. It will measure its own
  throughput and waits while it works and regulate how hard it presses on the drive and the
  CPU. The drive detector in `crates/sys` becomes an optional hint for the starting point,
  never a requirement. Profiles (Fast, Balanced, Gentle, Auto) stay as starting values and as
  ceilings, and the live fast/gentle switch stays.
- **Measure before building.** Nothing that regulates is written until Part 1 has produced
  numbers on real hardware. If they show a simpler approach is enough, we take it.
- **Same boundaries as before.** Core makes no OS calls and has no `unsafe`; the OS side
  lives in `crates/sys` and reaches core through hooks that the CLI fills in. No external
  process, no new dependency, no change to default behaviour, archive bytes, the `.part`
  rename, security checks or release profile.

## Part 1 scope (this ADR)

1. **Sequential-access hint** when opening source files and archives for reading
   (`zstar_sys::open_sequential`; Windows `FILE_FLAG_SEQUENTIAL_SCAN`, Linux
   `posix_fadvise(POSIX_FADV_SEQUENTIAL)`, elsewhere a plain open). Core receives it as
   `Options::open_hook`; with no hook it uses `File::open`. Best effort: if the hint fails the
   file is opened anyway. Output files are not covered. No gain is claimed: it is measured
   with the protocol below.
2. **Same-device check** (`zstar_sys::same_device`, `None` when not sure). It is only
   reported; no behaviour depends on it.
3. **Instrumentation**, off by default (`Options::stats`, CLI `--stats`): wall time, time in
   source reads, time in output writes, deflate CPU time of the multi-threaded gzip path, time
   workers were parked by `Control`, bytes in and out, worker count, MiB/s. Counters are
   relaxed atomics fed by thin timing adapters; there is no lock on the hot path. What each
   number means, per path, is documented in `crates/core/src/stats.rs`.
4. **Measurement protocol** for the owner: [`../reports/io-measurement.md`](../reports/io-measurement.md).

## Later parts: gates

Each one starts only when the owner decides after reading the Part 1 measurements.

- **A. Pipeline.** Only if the measured idle time between read, compress and write is large:
  a reusable pipeline (reader, workers, ordered writer, bounded queues), first for gzip and
  tar.gz, shaped so the future parallel zip can reuse it. Possible rule for same-device
  operations: prefer large batches over simultaneous read and write. Measured, not assumed.
- **B. Controller.** A simple controller with few knobs (CPU workers, queue and read-ahead
  size, files in parallel) that regulates by throughput and wait times in 1 s windows, with
  hard ceilings from the profile, the fast/gentle switch and a safe fixed fallback. Tested
  first against a simulated slow disk.
- **C. Memory.** Remembering what was learned per drive between runs.
- **D. Other codecs.** Thread changes for zstd, xz and 7z (their thread count is fixed when
  the encoder starts today), then Android.

A and B are worth their complexity only if the numbers say so. If a fixed rule per
situation (for example same device or not) already gets close, that rule replaces them.

## Consequences

- The drive kind keeps its current uses until a later part replaces them one by one, each
  with a measurement.
- `--stats` makes real-hardware results comparable between machines and runs.
- Reads served from the OS cache and writes that only reach it are measured as the caller
  sees them; the protocol uses cold and warm runs to tell them apart.
