# RAR through libarchive: what it can and cannot do

Status: research report, 2026-11-01. Nothing here was built into Zstar yet. RAR stays extract-only
(docs/DECISIONS.md section 4); this only measures what libarchive offers.

## Findings

Facts come from the sources linked at the end. "Not verified" marks what I could not test myself.

| Question | Answer |
|---|---|
| Encrypted RAR (data or headers) | **Not decryptable.** libarchive only detects encryption. The RAR v3 reader sets "RAR encryption support unavailable" and cannot read names when the main header is encrypted. The RAR5 reader reports encrypted entries (PR #2096, merged 2024-10-11) and then fails with `ARCHIVE_FATAL`. No password API helps: libarchive has passphrase support for Zip and 7z, not RAR. |
| Multi-volume RAR | **Partly.** RAR5 sets are read when libarchive is given the volume files in order (`archive_read_open_filenames`). RAR v3 handles split headers. Regression in 3.8.10 ("Unsupported block header size", issue #3683), fixed on 2026-10-16 (PR #3686); the fix may not be in a release yet. Not verified by me: no RAR file is available here and Zstar must never create one. |
| Other RAR limits | The formats manual calls RAR support "limited" and names only RARv3 and self-extracting RAR. The v3 reader rejects solid archives and RAR filter blocks ("solid archive support unavailable", "Parsing filters is unsupported"). RAR4 solid is not supported by the RAR5 code path either. |
| Android | The `libarchive2-sys` crate documents Android (arm64-v8a, armeabi-v7a, x86, x86_64) with NDK r21+, building vendored libarchive with CMake and linking zlib/bz2/lzma/zstd/lz4 from the NDK. **Not verified by me**: no NDK here. Its Android path hardcodes the `darwin-x86_64` NDK toolchain directory, so it would not build on a Linux or Windows CI runner as is. |
| Builds on Linux | Yes, tested here: `libarchive2 0.2.1` builds in about 1 minute (CMake, libclang for bindgen, system zlib/bz2/lzma/zstd/lz4/xml2/crypto/acl). |
| Licence | libarchive and both crates: BSD-2-Clause (GPL-3.0 compatible). |

## Consequences for Zstar

1. **Encrypted RAR cannot be opened.** The Android parity goal "passwords" cannot hold for RAR. The UI must say so instead of asking for a password.
2. **Stream API mismatch.** `libarchive2` reads from a path, memory, or a `'static` reader. Zstar's handlers receive `&mut dyn ReadSeek`, and `zstar-core` forbids `unsafe`. Options: (a) libarchive-backed formats work on file paths only; (b) a small audited `unsafe` FFI module of our own, restricted to RAR/CAB/cpio/ar/deb. Multi-volume needs paths either way.
3. **Bundled version.** `libarchive2-sys 0.2.0` vendors libarchive 3.8.1, which predates security fixes, and RAR parsers are a classic attack surface. A build against a newer libarchive is preferable.
4. **Build weight.** CMake plus libclang on every build machine, a C toolchain for Android, and a young third-party crate (three releases).
5. Nothing can be tested for RAR here: a fixture must come from WinRAR (or another licensed tool) outside this repository.

## Decision needed from the owner

- Path-only libarchive formats (a) or own FFI (b)?
- Who supplies a small RAR3 and RAR5 fixture set, plus a two-volume RAR5 set?
- Accept shipping RAR as "no passwords" on every platform?

## Sources

- libarchive-formats(5): https://man.archlinux.org/man/libarchive-formats.5.en
- RAR reader source (v3): https://fuchsia.googlesource.com/third_party/libarchive/+/refs/heads/upstream/master/libarchive/archive_read_support_format_rar.c
- RAR5 encryption unsupported: https://github.com/libarchive/libarchive/issues/1374 and https://www.github.com/libarchive/libarchive/pull/2096
- RAR5 multi-volume regression: https://github.com/libarchive/libarchive/issues/3683
- `libarchive2-sys` README and build.rs: https://docs.rs/crate/libarchive2-sys/0.2.0/source/README.md and https://docs.rs/crate/libarchive2-sys/0.2.0/source/build.rs
