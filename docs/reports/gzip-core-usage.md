# tar.gz compression: how many cores are really busy

Question: 1 GiB incompressible file, tar.gz, 59 s on an HDD laptop and 20 s on an NVMe laptop,
`codec_cpu_ms` about equal to `wall_ms` although `threads_start` is 3-4. Is one core doing the
work, and can the gzip path compress independent blocks in parallel (pigz style)?

Measured on a 2-CPU cloud VM (virtio disk, not an HDD), release build of commit e7c77a1,
1 GiB from `/dev/urandom`, level default, `--stats`, process CPU time sampled every 100 ms.
No code was changed. Output checked with `gzip -t` and `tar -tzf`.

## Results

| run | wall | CPU (user+sys) | avg cores | busy cores p10/p50/p90 | codec_cpu_ms | wait_workers_ms |
|---|---|---|---|---|---|---|
| `-j 1` (single `GzEncoder`) | 19.9 s | 19.6 s | 0.99 | 0.90 / 1.00 / 1.09 | n/a | 0 |
| `-j 2` | 10.5 s | 19.3 s | 1.85 | 1.70 / 1.89 / 1.99 | 18 716 | 9 762 |
| `-j 2`, cold page cache | 11.6 s | 20.0 s | 1.73 | 1.60 / 1.78 / 1.90 | 19 450 | 10 384 |
| `-j 4` (oversubscribed: 2 CPUs) | 12.4 s | 20.6 s | 1.66 | 1.57 / 1.68 / 1.79 | 31 917 | 11 698 |
| `-j 2 --tune` | 11.4 s | 20.2 s | 1.76 | 1.60 / 1.80 / 1.90 | 19 693 | 10 767 |
| `-j 4 --tune` | 12.5 s | 20.4 s | 1.63 | 1.54 / 1.64 / 1.79 | 31 807 | 11 808 |
| `--profile gentle` (start 1, pool 2) | 20.8 s | 20.4 s | 0.98 | 0.90 / 1.00 / 1.10 | 19 805 | 20 019 |

## Findings

1. **The gzip path already compresses independent blocks in parallel.** `formats/pargz.rs`
   (`ParGz`) cuts the input into 256 KiB blocks, primes each with the previous 32 KiB, ends it
   with a sync flush and concatenates them into one valid deflate stream. It is used whenever
   `workers.pool > 1` (`formats/codec.rs`). Output is +0.03 % on incompressible data. This is
   the pigz design (single member, not multi-member); a multi-member variant would only change
   the framing, not the parallelism. `docs/reports/parallel-deflate.md` has the earlier A/B.
2. **`codec_cpu_ms` is total thread time, not wall time.** It sums, over all threads, the
   elapsed time of each compress job. For 1 GiB of this codec the total work is ~19-20 s of CPU
   at any thread count, so `codec_cpu_ms ~ wall_ms` is what *one* busy core looks like, but
   also what a 4-thread run that only gets a quarter of its threads busy looks like. It cannot
   tell the two apart on its own. It also counts time a thread spends preempted, so it
   overstates CPU when threads outnumber cores (`-j 4` here: 31.9 s for 20.6 s of real CPU).
   Useful derived numbers: workers-phase parallelism = `codec_cpu_ms / wait_workers_ms`
   (1.92 for `-j 2`), serial share = `(wall_ms - wait_workers_ms) / wall_ms` (5-6 % here).
3. **With 2 threads allowed, ~1.8-1.9 cores are busy.** Scaling is near-ideal: 19.9 s -> 10.5 s.
   Beyond 2 cores could not be measured on this VM.
4. **The symptom is reproduced when the active worker count is 1**, not when the pool is
   larger: `--profile gentle` (`threads_start: 1`) gives 0.98 cores and `codec_cpu_ms` 19.8 s
   against 20.7 s wall, the same signature as the laptops. `threads_start` only says how many
   workers were allowed at the start; `Workers::active()` follows `Control::threads()`, and the
   tuner changes that value while the operation runs (`govern.rs:186` calls
   `control.set_threads`). `active_peak` is `n/a` for gzip, so the stats cannot show whether
   the tuner lowered it. I could not confirm this on the laptops.
5. **Design limit that matters on slow disks:** each wave is a barrier. `flush_wave` spawns
   threads with `thread::scope` and joins them before returning, so while workers compress
   the producer (read + tar) is idle, and while the producer reads and the writer writes the
   workers are idle. The doc comment says one block set is compressed while the caller fills
   the next; the code does not do that. Wall time is therefore roughly
   `serial_io + cpu / threads` instead of `max(io, cpu / threads)`. Here serial I/O is 5-6 %
   of wall (page cache, fast virtio disk). On an HDD it is the time to read 1 GiB plus write
   ~1 GiB of output, added on top of the compression; that alone can explain a large part of
   59 s vs 20 s. A static `i % active` split per wave also leaves cores idle when block times
   differ (mixed data), and 4 threads are spawned per 2 MiB wave (512 waves for 1 GiB).
6. **`-j 4` on 2 CPUs is slower than `-j 2`** (12.4 s vs 10.5 s): oversubscription plus the
   wave barrier waiting for the slowest preempted thread.

## What is not known

- Core count and CPU model of the two laptops, and whether the NVMe run really had 3-4 active
  workers. Needed: the full `--stats` block of both runs, in particular `wait_workers_ms`,
  `wait_input_ms`, `wait_output_ms`, `tuner_changes`, `profile`, plus `nproc`.
  - If `wait_workers_ms` is ~wall and `codec_cpu_ms / wait_workers_ms` is ~1: only one worker
    was active (tuner or profile), the parallel code is not the problem.
  - If `wait_workers_ms` is well below wall: the barrier / serial I/O of finding 5 is the cost.
- Behaviour above 2 cores (the wave split and the barrier are the likely limits).

## Candidate changes (none made)

1. Make `active_peak` (and the minimum active count) available for gzip in `--stats`, so the
   next measurement settles finding 4.
2. Pipeline the waves: compress wave N on persistent workers while the caller fills wave N+1
   and the previous output is written; replace the static split by a shared next-block counter.
   Memory stays bounded (the existing two blocks per thread).
3. Decide whether the tuner may drop a compress-bound gzip run to 1 worker on a quiet CPU.
