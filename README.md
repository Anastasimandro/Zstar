# Zstar

Fast, low-memory archive manager for Windows, Linux, macOS and Android. Open source
(GPL-3.0-only), written in Rust. Goals, in order: speed, low
memory use and fast startup; then a clean, intuitive design.

Releases currently ship Linux and Windows builds only. macOS is temporarily paused (see
[ADR 0004](docs/adr/0004-macos-paused.md)); the code keeps its macOS support but it is not built or tested in CI.

This repository currently holds the engine and the command line:

- `crates/core`: the archive engine (no UI, stream-based). Formats: zip, 7z, tar, tar.gz, tar.zst,
  tar.bz2, tar.xz, tar.lz4, and single files as gz, zst, bz2, xz, lz4 and br (Brotli).
- `crates/cli`: the `zstar` command.

```
zstar compress backup.tar.zst photos/ notes.txt
zstar list backup.tar.zst
zstar extract -o restore/ backup.tar.zst
```

The format is chosen from the output name when creating and detected from the file's
contents when reading. Run `zstar --help` for the options, including the decompression-bomb
limits.

## Measuring with `--stats`

`zstar compress --stats ...` and `zstar extract --stats ...` print a `key: value` block on
stderr when the operation succeeds. It is off by default and costs one branch per call when
off. Besides wall time, bytes and throughput it says where thread time went and what the engine
asked of the file system:

- `busy_ms`, `wait_input_ms`, `wait_output_ms`, `wait_workers_ms`, `finish_ms` (plus
  `parked_ms`): the time of every thread split into exclusive parts, in thread-milliseconds, so
  with several threads they add up to more than `wall_ms`. `dominant_stage` and
  `dominant_share_pct` name the largest part: `cpu`, `wait_input`, `wait_output`,
  `wait_workers`, `finish` or `parked`.
- `queue_avg`, `queue_max`: items handed to the workers per gzip wave or zip batch (`n/a` where
  there is no queue).
- `fs_opens`, `fs_stats`, `fs_mkdirs`, `fs_renames`, `files`, `files_per_s`, `latency_p50_ms`,
  `latency_p95_ms` (per file, from start to done).
- `write_completion: buffered`: writes and `finish_ms` hand data to the operating system. Zstar
  never calls `fsync`, so no number here is the time until the data is on the disk.

Every key is defined in the module docs of
[`crates/core/src/stats.rs`](crates/core/src/stats.rs); the way to run measurements is in
[docs/reports/io-measurement.md](docs/reports/io-measurement.md) and what the counters cost in
[docs/reports/stats-overhead-v2.md](docs/reports/stats-overhead-v2.md).

## Encrypted zip is slow with many small files

Zip encryption (WinZip AES-256) derives a new key for every entry: PBKDF2-HMAC-SHA1 with 1,000
iterations and a random salt, measured at 0.95 ms per entry on the test machine whatever the
file size, and it is not shared between entries. So 10,000 files of 4 KiB take about 10 s
encrypted against 0.3 to 0.6 s plain, and 10,000 empty files take 9.6 s. The data itself is
not the cost: one 4 GiB file stored takes 9.6 s with AES against 5.8 s without. Encrypted
archives are written on one thread.

For many small files the 7z format is the faster choice: it derives the key once per archive
(10,000 x 4 KiB: 1.3 s encrypted against 1.2 s plain). For a few large files the two formats
are alike. Numbers and method: [docs/reports/zip-upgrade.md](docs/reports/zip-upgrade.md).

## Limits on huge trees

Creating an archive walks the source tree on a helper thread and hands the entries to the
writer through a bounded queue, so nothing holds the whole tree. Siblings are still sorted per
directory, parents come before children and the archives are byte for byte the same as before.
Zip extraction keeps its full security check before the first write, but stores the plan
compactly (each name once plus 24 bytes per entry). Four limits apply to both, are errors when
exceeded (never a skipped check) and can be raised or removed with `--no-limits`:

- `--max-entries` (default 10,000,000 entries), `--max-path` (default 32 KiB per entry name),
  `--max-depth` (default 1,024 components) and `--max-metadata` (default 1 GiB of metadata held
  in memory: directory listings of the walk, the zip plan, and what the zip and 7z writers keep
  until they finish). The same keys exist under `[limits]` in the config file.
- While a tree is still being walked the progress total is unknown (`None`); it becomes known
  when the walk ends.

Numbers: [docs/reports/metadata-streaming.md](docs/reports/metadata-streaming.md).

## Build

```
cargo build --release --locked
cargo test --locked
```

A C compiler is needed (the zstd library is built from source). Dependencies and their
licences are listed in [DEPENDENCIES.md](DEPENDENCIES.md); closed decisions in
[docs/DECISIONS.md](docs/DECISIONS.md) and the ADRs in [docs/adr/](docs/adr/).

RAR support will be extract-only. Creating RAR files is, and will stay, unsupported.
