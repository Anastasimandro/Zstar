# Vendored libarchive

- Version: 3.8.11, from the official release tarball `libarchive-3.8.11.tar.xz`
  (https://github.com/libarchive/libarchive/releases/tag/v3.8.11).
- Files are copied unmodified. Nothing in `libarchive/` was patched; compile-time choices live in
  `../csrc/config.h` and `../build.rs`.
- Copied `.c`: the 17 listed in `build.rs`, plus `archive_windows.c` (Windows builds only).
  Copied `.h`: every header of the `libarchive/` directory (needed to resolve includes; none of
  the unbuilt formats' code is compiled).
- Left out on purpose: every other format, every filter, every writer, every external library
  (zlib, bzip2, lzma, zstd, lz4, OpenSSL, iconv, ACL and xattr libraries). `archive_random` is
  replaced by `../csrc/shim.c`, which always fails.
- To update: replace the files from a new official tarball, bump `ARCHIVE_VERSION_*` in
  `csrc/config.h` and the version assertion in `src/lib.rs`, rerun the fixtures table and the
  provenance review.
