# Dependencies

Zstar is licensed GPL-3.0-only. Every dependency below is under a licence that can be
combined with it, and `cargo deny check` (see `deny.toml`) enforces that in CI.

Versions are pinned exactly in the root `Cargo.toml` and were checked against the crates.io
index when added. `Cargo.lock` is committed and CI builds with `--locked`.

## Direct dependencies

| Crate | Version | Licence | Used by | Why |
|-------|---------|---------|---------|-----|
| zip | 9.1.3 | MIT | core (`zip` feature) | ZIP reading and writing; decodes CP437 names; parallel-friendly metadata |
| tar | 0.4.46 | MIT OR Apache-2.0 | core (`tar` feature) | tar header parsing and writing, GNU and pax long names |
| flate2 | 1.1.10 | MIT OR Apache-2.0 | core (`gzip` feature) | gzip streams; built with the pure-Rust `zlib-rs` backend |
| zstd | 0.14.3 | BSD-3-Clause | core (`zstd` feature) | Zstandard streams, multithreaded compression (`zstdmt`) |
| bzip2 | 0.6.1 | MIT OR Apache-2.0 | core (`bzip2` feature) | bzip2 streams; built on the pure-Rust `libbz2-rs-sys` backend, no C code |
| lzma-rust2 | 0.23.0 | Apache-2.0 | core (`xz` feature) | XZ streams (LZMA2), multithreaded compression. Pinned to 0.23.0, not the newest 0.24.0, so that it is the same copy `sevenz-rust2` 0.24.2 uses |
| lz4_flex | 0.14.0 | MIT | core (`lz4` feature) | LZ4 frame format, pure Rust. Its decoder stops after the first frame; `formats/codec.rs` wraps it to read concatenated frames |
| brotli | 9.1.1 | BSD-3-Clause AND MIT | core (`brotli` feature) | Brotli streams, pure Rust. Its writer swallows errors while closing; `formats/codec.rs` watches for them |
| sevenz-rust2 | 0.24.2 | Apache-2.0 | core (`sevenz` feature) | 7z reading and writing (LZMA, LZMA2, bzip2, filters). PPMd and Deflate are left out on purpose; the AES feature is added with the encryption step |
| lexopt | 0.3.2 | MIT | cli | Argument parsing without the size and startup cost of a derive framework |
| ctrlc | 3.5.2 | MIT/Apache-2.0 | cli | Ctrl-C triggers the cooperative cancel token |
| tempfile | 3.27.0 | MIT OR Apache-2.0 | tests only | Temporary directories |

## Notable transitive dependencies

| Crate | Licence | Pulled in by | Note |
|-------|---------|--------------|------|
| libbz2-rs-sys | bzip2-1.0.6 | bzip2 | Pure-Rust port of libbzip2. Licence added to `deny.toml`: BSD-style, no copyleft |
| brotli-decompressor, alloc-stdlib, alloc-no-stdlib | BSD-3-Clause AND MIT / BSD-3-Clause | brotli | Decoder and allocators |
| twox-hash | MIT | lz4_flex | xxHash32 for the frame checksums |
| sha2 | MIT OR Apache-2.0 | lzma-rust2 | XZ SHA-256 integrity check type |
| zlib-rs | Zlib | flate2 | Pure-Rust zlib; also what zip's deflate uses |
| zstd-sys | BSD-3-Clause | zstd | Bundles the reference C implementation of Zstandard (BSD-licensed); needs a C compiler at build time |
| zstd-safe | BSD-3-Clause | zstd | Safe wrapper over zstd-sys |
| crc32fast | MIT OR Apache-2.0 | flate2, zip | Checksums |
| indexmap, hashbrown, equivalent | MIT OR Apache-2.0 | zip | Entry index |
| typed-path | MIT OR Apache-2.0 | zip | Path handling |
| rustix, linux-raw-sys, libc | MIT / Apache-2.0 (with LLVM exception) | tempfile, tar | System calls |
| nix, objc2, dispatch2, block2 | MIT / Zlib / Apache-2.0 | ctrlc | Signal handling on Unix and macOS |
| windows-sys | MIT OR Apache-2.0 | several | Windows API bindings |

The complete list with licences is what `cargo deny list` prints. Licences encountered so far:
MIT, Apache-2.0 (with and without the LLVM exception), BSD-3-Clause, Zlib, Unicode-3.0,
bzip2-1.0.6 and Unlicense (as one alternative of a dual licence).

## Rules

- RAR is extract-only, through libarchive. The `unrar` crate is banned in `deny.toml`.
- Slint, when the GUI arrives, is used only under its GPL-3.0 option.
- Adding a dependency means adding it to this file in the same commit.
