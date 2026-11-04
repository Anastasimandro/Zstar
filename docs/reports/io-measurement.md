# I/O measurement protocol (Part 1 of ADR 0010)

Goal: find out where the time goes when zstar compresses or extracts on real hardware: reading,
compressing, writing, or waiting. The numbers decide whether the later parts of
[ADR 0010](../adr/0010-measure-first-adaptive-io.md) are worth building. Nothing below has been
measured yet: the tables are empty on purpose.

You need a build with `--stats` (the `nightly` pre-release after this change, or
`cargo build --release -p zstar-cli`). No Python, no administrator rights.

## What `--stats` prints

A `key: value` block on stderr after a successful compress or extract. The keys never change.

| key | meaning |
| --- | --- |
| `operation`, `format` | what ran; `format` is `detected` for extract |
| `profile`, `drive_kind`, `drive_source` | profile in use, drive kind the engine started from, `given` (`--drive`) or `detected` |
| `same_device` | `yes` / `no` / `unknown`: source and output on the same device id (Linux `st_dev`, Windows volume). `no` can still be two partitions of one disk |
| `threads_pool`, `threads_start` | workers created and workers allowed at the start (1 for formats that do not use workers) |
| `wall_ms` | from just before the library call to its return; includes scanning the inputs |
| `read_ms` | time inside read calls on the source files (compress) or on the archive (extract); what the caller waited, which may be served by the OS cache |
| `write_ms` | time inside write calls on the archive (compress) or the extracted files (extract); a write that only reaches the OS cache is cheap |
| `codec_cpu_ms` | gzip only, with more than one thread: deflate time summed over threads (can exceed wall); `n/a` otherwise |
| `other_ms` | `op - read - write` on the calling thread. One thread: compressing or decompressing plus bookkeeping. Several threads: rough, because `read_ms` and `write_ms` are summed over threads |
| `parked_ms` | time workers slept for the throughput limit or the thread limit, summed over threads |
| `bytes_in`, `bytes_out` | bytes read from the source (compress) or archive (extract); bytes written to the archive or the files |
| `throughput_mib_s` | uncompressed bytes (source for compress, extracted for extract) divided by `wall_ms` |

Reading the result: if `read_ms + write_ms` is close to `wall_ms`, the disk is the limit. If
`other_ms` (one thread) or `codec_cpu_ms / threads_start` is close to `wall_ms`, the CPU is. If
none of them add up to `wall_ms`, something is idle: that is the number Part A needs.

## Test data

Random bytes do not compress, so they test the disk and the copy path; they are the worst case
for the compressor. Use a real compressible file of your own too if you have one (a text or log
dump; a video or a disk image is incompressible).

Windows (PowerShell), from an empty folder on the disk under test:

```powershell
# L: one 1 GiB file
$r=[Random]::new(1); $b=New-Object byte[] (8MB); $f=[IO.File]::Create("$PWD\big.bin")
1..128 | % { $r.NextBytes($b); $f.Write($b,0,$b.Length) }; $f.Close()
# M: 20,000 files of 16 KiB in 200 folders
$b=New-Object byte[] 16384
1..200 | % { $d="small\d$_"; New-Item -ItemType Directory -Force $d | Out-Null; 1..100 | % { $r.NextBytes($b); [IO.File]::WriteAllBytes("$PWD\$d\f$_.bin",$b) } }
```

Linux:

```sh
head -c 1073741824 /dev/urandom > big.bin
for d in $(seq 1 200); do mkdir -p small/d$d; for f in $(seq 1 100); do head -c 16384 /dev/urandom > small/d$d/f$f.bin; done; done
```

## Cases

For each machine (HDD laptop, SSD laptop) run every row of this matrix:

- **Data**: L (`big.bin`) and M (`small`). `gz` stores one file, so it only runs on L.
- **Placement**: source and output on the **same disk**, and on **different disks** (a second
  internal disk, or a USB disk; say which). For extract, the archive and the destination.
- **Format and operation**: `gz` (L only), `tar.gz`, `zip`, `tar.zst`, `7z`; compress and
  extract each.
- **Cache**: cold and warm, three runs each. Keep the middle one in the table; keep all the
  raw `stats` files.
- Close everything else. Plug the laptop in, use the same power plan, and let the machine sit
  idle for a minute before starting.

Optional, to see what the thread count does: repeat the L and M `tar.gz` rows with `-j 1`.

To see what the sequential-read hint does, run the same rows with a build from before this
change and compare `read_ms`; the hint is always on in the CLI.

## Running one case

Use `cmd.exe` on Windows (PowerShell 5 wraps native stderr in error records). `-f` replaces an
existing output.

```bat
zstar compress -f --stats D:\out\big.tar.gz big.bin 2> run01.txt
zstar extract  -f --stats -o D:\out\x D:\out\big.tar.gz 2> run01.txt
```

```sh
zstar compress -f --stats /mnt/other/big.tar.gz big.bin 2> run01.txt
zstar extract  -f --stats -o /mnt/other/x /mnt/other/big.tar.gz 2> run01.txt
```

Delete the extraction folder between extract runs. On Linux, `/usr/bin/time -v` in front of
the command adds peak memory (`Maximum resident set size`).

## Cold and warm without administrator rights

Warm: run the case once and discard it, then run it again; the files are in the OS cache.

Cold on Linux, for the files a run will read, and for the output of the previous run:

```sh
sync
dd if=big.bin iflag=nocache count=0 2>/dev/null     # one big file
find small -type f -exec dd if={} iflag=nocache count=0 status=none \;   # many files
dd if=/mnt/other/big.tar.gz iflag=nocache count=0 2>/dev/null           # the archive
```

`iflag=nocache` asks the kernel to drop that file's cached pages and needs no privileges
(GNU `dd`). Directory entries stay cached, which understates the cost of many small files; for
a fully cold M run, reboot instead.

Cold on Windows: restart the PC and run the case first thing after logging in (let startup
settle for a few minutes). Between runs without a restart, push the files out of the cache by
reading a filler file larger than the machine's RAM from a different disk:

```powershell
$b=New-Object byte[] (16MB); $s=[IO.File]::OpenRead("E:\filler.bin")
while ($s.Read($b,0,$b.Length) -gt 0) {}; $s.Close()
```

Create `E:\filler.bin` once, with the L snippet, sized to about twice the RAM. Say in the table
which method you used. A cold run is only trusted if `read_ms` is clearly larger than in the
warm run of the same case.

## Collecting the table

One row per kept run. Rename each stats file after the case, for example
`hdd-L-same-tar.gz-compress-cold.txt`, then turn the files into CSV.

Windows (PowerShell):

```powershell
Get-ChildItem *.txt | % { $h=@{}; Get-Content $_ | % { $k,$v=$_ -split ': ',2; $h[$k]=$v }
  "$($_.BaseName),$($h.wall_ms),$($h.read_ms),$($h.write_ms),$($h.codec_cpu_ms),$($h.other_ms),$($h.parked_ms),$($h.bytes_in),$($h.bytes_out),$($h.throughput_mib_s),$($h.same_device),$($h.drive_kind)" }
```

Linux:

```sh
for f in *.txt; do awk -F': ' -v n="${f%.txt}" '{v[$1]=$2} END {printf "%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s\n", n, v["wall_ms"], v["read_ms"], v["write_ms"], v["codec_cpu_ms"], v["other_ms"], v["parked_ms"], v["bytes_in"], v["bytes_out"], v["throughput_mib_s"], v["same_device"], v["drive_kind"]}' "$f"; done
```

## Table to fill in

Machine: ______ (CPU, RAM, drive model, bus). Build: ______ . Cold method: ______ .

| machine | data | placement | format | op | cache | wall_ms | read_ms | write_ms | codec_cpu_ms | other_ms | parked_ms | bytes_in | bytes_out | MiB/s | same_device | drive_kind |
| --- | --- | --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- | --- |
| HDD | L | same | gz | compress | cold | | | | | | | | | | | |
| HDD | L | same | gz | compress | warm | | | | | | | | | | | |
| HDD | L | same | gz | extract | cold | | | | | | | | | | | |
| HDD | L | same | gz | extract | warm | | | | | | | | | | | |
| HDD | L | different | gz | compress | cold | | | | | | | | | | | |
| HDD | L | different | gz | compress | warm | | | | | | | | | | | |
| HDD | L | different | gz | extract | cold | | | | | | | | | | | |
| HDD | L | different | gz | extract | warm | | | | | | | | | | | |
| HDD | L | same | tar.gz | compress | cold | | | | | | | | | | | |
| HDD | M | same | tar.gz | compress | cold | | | | | | | | | | | |

The full matrix is 2 machines x 2 datasets x 2 placements x 5 formats (`gz` on L only) x 2
operations x 2 cache states. Copy the HDD rows for each combination and for the SSD machine;
one line per kept run.

## What to send back

The table and the raw `stats` files. Also add, per machine, the output of
`zstar drive-info <path>` for each disk used, so we can see what the detector said.
