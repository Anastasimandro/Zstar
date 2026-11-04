# Cost of `--stats`

Measured on the build of Part 1 of [ADR 0010](../adr/0010-measure-first-adaptive-io.md), on a
shared Linux VM (2 vCPUs, no control over neighbours), so run-to-run noise is far above 1%.
The conclusion therefore rests on two things: the wall-time runs show no difference outside
the noise, and the added work is small enough to bound by counting.

## Wall time: 1 GiB gzip compress

`zstar compress -f out.gz text.bin` with default threads (2), release build, one 1 GiB file of
word-salad text (compresses to 548 MiB), page cache warm, output on the same disk. Seven
rounds, each round running the three variants one after the other. `baseline` is the build of
the commit before this change; `off` is this change without `--stats`; `stats` adds `--stats`.

| variant | median | min | max |
| --- | ---: | ---: | ---: |
| baseline | 16.20 s | 14.47 s | 16.95 s |
| off | 15.81 s | 14.29 s | 20.85 s |
| stats | 14.56 s | 14.18 s | 16.39 s |

The three archives have the same size (574,730,974 bytes). The spread inside one variant (14.3
to 20.8 s) is larger than any difference between variants, so these runs cannot show a cost
under 1%; they only show there is none large enough to see.

## Bounding it

With stats on, each source read and each output write costs two `Instant::now` calls and three
relaxed atomic adds, and each compression thread adds once per wave. One start-and-elapsed
pair measured 71 ns on this VM. This run does about 1,024 reads (1 MiB buffer) and about 4,100
writes (one per 256 KiB block), so about 5,100 timed calls: roughly 0.4 ms in total, 0.002% of
16 s. With stats off each call pays one branch on an `Option`.

On many small files the number of timed calls grows with the number of files, not the bytes;
that case is part of the owner's measurement protocol, not measured here.

## A real block

From a run with `--stats` on that machine (`drive_kind: hdd` is what the detector said for the
VM disk):

```
read_ms: 327.088
write_ms: 366.960
codec_cpu_ms: 26464.086
wall_ms: 16412.158
threads_pool: 2
```

Reading and writing were 4% of the wall time; deflate ran about 13 s on each of the two threads
out of 16.4 s, so this case was limited by the CPU and not by the disk.
