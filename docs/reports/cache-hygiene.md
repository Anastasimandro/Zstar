# Page-cache hygiene (ADR 0012, step 3)

`--cache-hygiene`: while a big file is copied, start writing each window and, one window later,
wait for it and drop it from the page cache (`sync_file_range` + `posix_fadvise(DONTNEED)`).
Windows are `io_depth` x chunk bytes (4 requests by default, 1 to 64 MiB). Files smaller than a
window (4 MiB for sources) cost nothing. Linux only; other systems do nothing.

## Measurement

Release build, gzip, 2.2 GiB input (700 MiB random + 1.5 GiB text), archive 710 MiB. Caches
dropped before each run; `/proc/meminfo` sampled every 50 ms. One virtual disk (ext4 on
`/dev/vda`, 8 GiB RAM, no real HDD or SSD), so the times mean little and only the memory
columns are solid.

| run | wall | peak Dirty | peak Writeback | peak Cached growth |
| --- | --- | --- | --- | --- |
| extract, plain (3 runs) | 2.07 / 2.06 / 2.47 s | 750-757 MiB | 48-80 MiB | 2909-2913 MiB |
| extract, `--cache-hygiene` (3 runs) | 2.32 / 2.27 / 2.34 s | 4 MiB | 4 MiB | 13 MiB |
| compress, plain (2 runs) | 10.88 / 12.11 s | 710 MiB | 0-56 MiB | 2869-2913 MiB |
| compress, `--cache-hygiene` (2 runs) | 9.48 / 10.31 s | 1 MiB | 1 MiB | 12 MiB |

Output is byte-identical with and without the flag (archive compared with `cmp`; extracted file
compared with the input).

Reading it: pending writes drop from about 750 MiB to 4 MiB and the cache no longer takes the
whole transfer. Extraction was about 5% slower here (mean 2.20 s plain, 2.31 s hygiene; the
plain runs vary by 20%); compression was faster. That is one VM disk, where the writes are
cheap; the wait is the cost on a drive that is faster than the work, and the gain is on one
that is slower or shared. It is off by default until the owner has numbers from a real HDD, a
SATA SSD and an NVMe, and from a USB stick.

## Not covered

Windows and macOS (no calls; nothing measured) and Android (`libc` has no `sync_file_range`
there). Files that formats open through `open_source` and write through the engine are covered;
what `libarchive` opens by itself (RAR) is not.
