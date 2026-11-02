# 0008: Control handle and drive profiles

- Status: Accepted
- Date: 2026-11-02
- Changes: implements section 5 (engine: profiles, disk detection, control handle) of [`../DECISIONS.md`](../DECISIONS.md)

## Context

The GUI must let the user switch between Fast and Gentle while an operation runs, so the
core has to accept changes mid-flight. It also must not contain OS calls or hardware detection.

## Decision

- **`Control`** (`control.rs`) replaces `CancelToken` inside `Options`. It is a cheap clone over
  atomics only, with no locks on the hot path. Live values: cancel, thread limit, priority
  (`Normal | Low`), throughput limit in bytes/s (0 = none) and a profile switch
  (`Fast | Balanced | Gentle`, which sets threads and priority together).
- **Safe points only.** Workers read it between chunks, files or waves of blocks, never inside
  a block. A change takes effect at the next boundary and cannot corrupt output.
- **Threads.** The pool is created at its maximum (every CPU, or `Options::threads`). Workers
  above the current limit park and re-check every 20 ms; they also stop on cancel and when the
  work runs out. In gzip the limit chooses how many workers share each wave of blocks. zstd and
  xz fix their thread count when the encoder starts, and 7z likewise, so for those the limit
  applies at the start only.
- **Throughput limit.** A token bucket in virtual time (one atomic), charged where bytes are
  counted, with at most 50 ms of burst credit.
- **No OS calls in core.** Priority is a value. `Options::worker_hook` is called by each worker
  thread when it starts and whenever it sees the priority change. The real OS call, and
  restoring a thread afterwards, live outside core.
- **Profile and drive.** `Options` carries `Profile { Fast, Balanced, Gentle, Auto }` and
  `DriveKind { Ssd, Hdd, Unknown }`. The caller resolves the drive kind. One pure function,
  `settings_for`, maps both to threads, priority, copy-buffer size and whether many files may
  be extracted in parallel. The numbers sit in one table at the top of `profile.rs`:

  | | threads | priority |
  |---|---|---|
  | Fast | all CPUs | Normal |
  | Balanced | 3/4 of the CPUs | Normal |
  | Gentle | 1/4 of the CPUs, 1 to 2 | Low |
  | Auto | Ssd: Fast; Hdd, Unknown: Balanced | |

  | drive | buffer | parallel many-file extraction |
  |---|---|---|
  | Ssd | 128 KiB | yes |
  | Hdd, Unknown | 1 MiB | no |

  The profile sets CPU use and the drive sets I/O behaviour. Buffer size and parallel
  extraction are fixed when the operation starts; only threads and priority switch live.
- **CLI.** `--profile fast|balanced|gentle|auto` and `--drive hdd|ssd`. No live switching there.

## Consequences

- Without `--drive`, the CLI takes the HDD-safe I/O rules, so extracting a zip with many files
  is sequential by default. That is the safe choice until drive detection exists; it costs
  speed on SSDs, to be measured. `--drive ssd` restores parallel extraction.
- `CancelToken` is gone: callers use `Options::control` (`cancel()`, `is_cancelled()`).
- The table numbers are first guesses, to be tuned after measuring on real hardware.
- Compression level switching stays a later improvement, at block or file boundaries only.
- Measured on one 2-CPU machine: gz compression of 1 GiB showed no overhead from polling
  (12.5 s before, 12.1 s after, four alternating runs; the gap is noise).
