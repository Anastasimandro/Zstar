# Decisions

## Phase 1: workspace, core, cli

- **Crates**: `crates/core` (`zstar-core`) and `crates/cli` (`zstar-cli`, binary `zstar`).
  Edition 2024, MSRV 1.88 (required by `zip` 9).
- **API shape**: formats implement `ArchiveFormat` over `&mut dyn ReadSeek` /
  `&mut dyn WriteSeek`. Adding a format = one impl + one arm in `handler()`. The archive side
  is always a stream; the extraction target and the files to archive are paths.
- **Detection**: magic bytes only. tar is recognised by header checksum, also inside the first
  decompressed block of gzip/zstd. A plain `.gz`/`.zst` is not an archive (later step).
- **Names**: `/` and `\` are both separators; `..`, absolute paths, drive prefixes and NUL are
  rejected for every name, on every platform. Windows reserved names and characters are
  rejected when `Options::windows_names` is on (default: Windows only).
- **Zip-slip**: zip validates every name before writing anything; tar, being sequential,
  rejects at the offending entry. Neither can write outside the destination.
- **Symlinks**: created after all files, then resolved with `canonicalize` and removed if they
  leave the destination. Dangling links containing `..` are refused. Absolute targets are
  refused. Windows skips symlink entries and reports them in `Summary::skipped`.
- **Hard links** (tar): only to a regular file already extracted from the same archive.
- **Permissions**: `mode & 0o755 | 0o400`; no setuid/setgid/sticky, no group/other write.
- **Bomb limits** (defaults): 64 GiB total, 10M entries, ratio 500 (checked after 256 MiB of
  output, against the archive's size). Charged on bytes actually written. `Limits::unlimited()`
  and CLI `--no-limits` switch them off.
- **Threads**: zip extraction (per-worker `ZipArchive` over a locked shared reader), zip
  compression of files <= 1 MiB, zstd compression. Deflate of one large file and all tar
  extraction are inherently sequential. Case-insensitive name collisions force a single thread.
- **Not enabled yet**: zip entries using bzip2/lzma/zstd/deflate64/PPMd, and AES (planned: AES-256
  for zip and 7z only).
- **CLI**: `lexopt`, not `clap` (size and startup). Exit codes: 0 ok, 1 error, 2 usage, 130 cancelled.
