# Cost of `--stats` version 2

Same method as [io-stats-overhead.md](io-stats-overhead.md): wall-time rounds that cannot show a
cost below the noise, plus a bound by counting. Added here: CPU time (user + sys) for the
small-file cases, and a reproducible micro-measurement of one timed call.

Machine: shared Linux VM, 2 vCPUs, 8 GiB, kernel 6.18, release builds, page cache warm, output
on the same disk (the detector calls it `hdd`). Variants, run one after the other in every
round: `baseline` is the build of commit `3f6cc94` (before this change), `off` is this change
without `--stats`, `stats` is this change with `--stats`.

## Wall time and CPU time

| workload | rounds | metric | baseline | off | stats |
| --- | ---: | --- | ---: | ---: | ---: |
| gzip compress, one 1 GiB file (half text, half random) | 7 | wall, median (min / max) | 12.07 s (11.78 / 12.57) | 12.31 s (11.77 / 12.78) | 12.18 s (11.73 / 13.26) |
| zip create, 20,000 files of ~1 KiB | 15 | wall, median (min / max) | 0.721 s (0.683 / 0.807) | 0.748 s (0.682 / 0.785) | 0.725 s (0.701 / 0.829) |
| zip create, same | 15 | CPU, median (min / max) | 1.163 s (1.114 / 1.323) | 1.167 s (1.100 / 1.233) | 1.184 s (1.138 / 1.257) |
| zip extract, same archive | 15 | wall, median (min / max) | 1.437 s (0.656 / 2.905) | 1.623 s (0.624 / 2.769) | 1.584 s (0.888 / 2.826) |
| zip extract, same archive | 15 | CPU, median (min / max) | 1.385 s (0.589 / 2.322) | 1.429 s (0.609 / 2.167) | 1.621 s (0.605 / 2.247) |

What this shows and does not show:

- 1 GiB gzip and zip create: no difference outside the spread of each variant. In CPU time zip
  create is +0.3% `off` against `baseline` and +1.5% `stats` against `off`, which is what the
  count below predicts.
- Zip extract of 20,000 tiny files is dominated by the file system: the same variant takes
  0.6 s in one round and 2.3 s (CPU) or 2.9 s (wall) in another. The medians (+13% wall, +13%
  CPU for `stats` against `off`) are inside that spread, the minimums are equal (0.589, 0.609,
  0.605 s CPU), and the order of the variants flips between metrics. **These runs cannot show a
  cost of a few percent on extraction, in either direction.** The bound below is the only
  statement made for it.
- Archive bytes are the same with and without stats: that is checked by
  `archive_bytes_are_identical_with_and_without_stats`, not by these runs.

## Bounding it

One timed call is a clock read before and after, three relaxed atomic adds or a thread-local
tally, and (per file) a histogram add. Measured with
`cargo test --release -p zstar-core --lib stats::tests::span_cost -- --ignored --nocapture`:

| call | stats off | stats on |
| --- | ---: | ---: |
| empty call (reference) | 0.5 ns | |
| file-system span (`fs`) | 0.6 ns | 77.7 ns |
| output write (`timed_write`) | 0.0 ns | 74.8 ns |
| per-file timing (`time_file`) | 0.0 ns | 79.0 ns |
| `Timed::read` of 1 byte | 0.4 ns | 75.9 ns |

With stats off a call is the check of an `Option`, which the compiler folds to nothing or to
one predictable branch: below a nanosecond per call, and a few branches per file.

With stats on, about 75 to 80 ns per timed call. Counted from the code and from the counters of
a real run (10,000 files, one `--stats` block each, [below](#a-real-block)):

- zip extract, one tiny file: mkdir of the parent, destination check, create, one write,
  close, rename, the per-file timing, and about two archive refills (a lock wait and a read
  each): about 11 timed calls, 0.85 µs. A file took 32 µs in that run (322 ms for 10,000), so
  at most 2.6% **with stats on**, on the cheapest kind of file there is. Files that take
  milliseconds make it negligible.
- zip create, one tiny file: open, two reads, the per-file timing: about 4 calls, 0.3 µs
  against about 58 µs of CPU per file: 0.5%. Measured: +1.5%, noise included.
- 1 GiB gzip: about 5,100 timed calls, 0.4 ms of 12 s: 0.003%.

The calls grow with the number of files, not with bytes, so a tree of millions of empty files
is the worst case; it is part of Task 2's measurements.

Memory: `Stats` holds one 320-bucket histogram (2.5 KiB) and a few counters, once per operation.

## A real block

`zstar compress --stats a.zip many` on 10,000 files, then `zstar extract --stats -o out a.zip`
(the two blocks, abridged to the version 2 keys):

```
operation: compress            operation: extract
wall_ms: 380.362               wall_ms: 322.272
busy_ms: 504.600               busy_ms: 66.656
wait_input_ms: 55.293          wait_input_ms: 63.806
wait_output_ms: 17.178         wait_output_ms: 139.060
wait_workers_ms: 320.765       wait_workers_ms: 0.000
finish_ms: 1.168               finish_ms: 52.749
dominant_stage: cpu            dominant_stage: wait_output
dominant_share_pct: 56.1       dominant_share_pct: 43.2
queue_avg: 25.0                queue_avg: n/a
queue_max: 32                  queue_max: n/a
fs_opens: 10001                fs_opens: 10001
fs_stats: 10101                fs_stats: 10000
fs_mkdirs: 0                   fs_mkdirs: 10102
fs_renames: 1                  fs_renames: 10000
files_per_s: 26290.7           files_per_s: 31029.7
latency_p50_ms: 0.045          latency_p50_ms: 0.023
latency_p95_ms: 0.082          latency_p95_ms: 0.037
```

Compress: the calling thread waited for the two workers (320 ms of 380) and the workers were
busy compressing (504 ms of thread time over two threads): CPU-bound, with a batch of 25 files
on average. Extract (one worker, the engine took the HDD-safe rule for this disk): output
dominates, mostly the 10,102 `mkdir` calls and 10,000 destination checks, creates and renames.

## Something the new numbers showed

In the extract block above, `bytes_in` is 1,312,145,022 for an archive of 3,451,840 bytes: 380
times the archive. The binary of `3f6cc94` prints the same figure, so it is not caused by the
counters. It is about 131 KB read per entry, which fits two refills of the fixed 64 KiB
`BufReader` that every entry open starts with (finding 1 of the review; the two-refill count is
inferred from that size, not measured separately). Here the OS cache served it, so it cost
little; on a cold HDD it would not. Not changed in this task. It is a first thing to look at in
the zip A/B (Task 3), where the read buffer is a variable.
