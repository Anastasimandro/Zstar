# 0014: RAR, read only, through vendored libarchive

- Status: Accepted for the code; **publishing blocked** until the owner signs off [`../reports/rar-provenance.md`](../reports/rar-provenance.md)
- Date: 2026-11-22
- Changes: section 4 (formats) of [`../DECISIONS.md`](../DECISIONS.md) now has an implementation; answers the open questions of [`../reports/rar-libarchive.md`](../reports/rar-libarchive.md); builds on [0007](0007-format-scope.md) and [0013](0013-fuzzing.md)

## Context

RAR is extract-only, never created, and never through the `unrar` crate or the UnRAR source.
The research report left two choices open: libarchive formats on file paths only, or an FFI of our
own. The owner chose the second, with `unsafe` confined to two new crates.

## Decision

1. **Two crates.** `zstar-rar-sys` vendors libarchive 3.8.11 and declares 31 hand-written
   bindings (no bindgen, no libclang, no CMake). `zstar-rar` is the safe wrapper. These two hold all
   the `unsafe`; `zstar-core` keeps `#![forbid(unsafe_code)]` and sees RAR only as `Format::Rar`
   behind the Cargo feature `rar`, off by default.
2. **Minimal build.** `cc` compiles 17 hand-listed libarchive files (the RAR and RAR5 readers,
   PPMd7, entry and string helpers, CRC-32, BLAKE2) plus `archive_windows.c` on Windows. Every other
   format, filter, writer and external library is absent; `archive_random` is a shim that always
   fails. Nothing in libarchive is patched. See `crates/rar-sys/vendor/VENDOR.md`.
3. **Streams.** libarchive's read, skip and seek callbacks run over `Read + Seek`, each under
   `catch_unwind`. With the release profile's `panic = "abort"` a panic aborts instead of becoming
   an error; this is memory-safe and documented in `zstar-rar`.
4. **Behaviour.** Detection by magic bytes, listing, extraction of the whole archive. Single pass,
   so progress has no percentage (the total is unknown without decoding twice). Names go through the
   shared sanitiser; files are written to `.part` and renamed; entries, total bytes and ratio are
   limited as for other formats, and the declared size of each file is checked before it is read.
   Selected-entry extraction does not exist in `zstar-core` for any format yet, so RAR has none either.
5. **Typed refusals.** Encrypted data or headers: `Error::EncryptedUnsupported` ("encrypted RAR not
   supported"). A password in the options is refused as an invalid option. A volume of a multi-volume
   set: `Error::MultiVolumeUnsupported`. libarchive could read sets through its multi-callback API,
   but the handler receives a single stream and cannot know the other volumes' names; supporting it
   needs a path-based API and is left out of v1.
6. **Creation is refused twice**: `Format::is_extract_only` makes `create_file` fail before touching
   the disk, and the RAR handler's `create_stream` refuses. The CLI rejects `--format rar` when
   compressing.
7. **Release gate.** `release.yml` does not enable `rar`; CI job `rar-release-gate` fails if it ever
   does, or if `rar` becomes a default feature. Flipping it is a separate commit after sign-off.

## Consequences

- Every build with `rar` needs a C compiler (already true for `zstd-sys`). Binary +192 KiB on Linux.
- RAR5 archives with dictionaries above 64 MiB (WinRAR 7 can write larger ones) are refused by
  libarchive ("Declared dictionary size is not supported"); the typed error is `Unsupported`.
- Known libarchive gaps stay visible, not worked around: no encryption, no solid RAR3, no RAR3
  filter blocks. See the results table in [`../reports/rar-libarchive-v1.md`](../reports/rar-libarchive-v1.md).
- The fuzz target `rar_list_extract` is the one target where a bug can be memory-unsafe.
