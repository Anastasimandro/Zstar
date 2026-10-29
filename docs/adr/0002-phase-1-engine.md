# 0002: Phase 1 engine design

- Status: Accepted
- Date: 2026-10-29

## Context

Phase 1 delivers `core` (zip, tar, tar.gz, tar.zst) and a minimal `cli`. These are the
engine decisions that are not already fixed in `docs/DECISIONS.md`.

## Decision

- **API**: formats implement `ArchiveFormat` over `&mut dyn ReadSeek` / `&mut dyn WriteSeek`.
  A new format is one impl plus one arm in `handler()`. The archive side is always a stream;
  the extraction target and the files to archive are paths.
- **Detection**: magic bytes only. tar is recognised by its header checksum, also in the first
  decompressed block of gzip/zstd; a plain `.gz`/`.zst` is not an archive yet.
- **Names**: `/` and `\` both separate components. `..`, absolute paths, drive prefixes and NUL are
  rejected on every platform. Windows reserved names and characters are rejected when
  `Options::windows_names` is on (default: Windows only). Zip validates every name before
  writing anything; tar, being sequential, rejects at the offending entry.
- **Symlinks**: created after all files, resolved with `canonicalize`, and removed if they leave the
  destination. Absolute targets, and dangling targets containing `..`, are refused. Windows skips
  symlink entries and counts them in `Summary::skipped`. Hard links (tar) may only point to a
  regular file already extracted from the same archive.
- **Permissions**: `mode & 0o755 | 0o400`: no setuid/setgid/sticky, no group/other write, never
  unreadable by the owner.
- **Bomb limits** (defaults): 64 GiB total, 10M entries, ratio 500 checked after 256 MiB of
  output against the archive size. Charged on bytes actually written, never on header claims.
  `Limits::unlimited()` and CLI `--no-limits` switch them off.
- **Threads**: zip extraction (one `ZipArchive` per worker over a locked shared reader),
  compression of zip files <= 1 MiB, zstd compression. Deflate of one large file and all tar
  extraction are inherently sequential. Case-insensitive name collisions force one thread.
- **CLI**: `lexopt`, not `clap` (binary size, startup). Exit codes: 0 ok, 1 error, 2 usage, 130 cancelled.

## Consequences

- Measured on 1 GiB (release, 2 CPUs): peak RSS 3-6 MiB for every format, 86 MiB for multithreaded
  zstd compression; binary 1.6 MB.
- Not done yet: zip entries using bzip2/lzma/zstd/deflate64/PPMd, AES-256, fuzzing, cross-tests
  against 7-Zip and libarchive, and benchmarks against 7-Zip and ouch (all required by
  `docs/DECISIONS.md` section 9 before a format is considered finished).
