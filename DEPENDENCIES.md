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
| lexopt | 0.3.2 | MIT | cli | Argument parsing without the size and startup cost of a derive framework |
| ctrlc | 3.5.2 | MIT/Apache-2.0 | cli | Ctrl-C triggers the cooperative cancel token |
| tempfile | 3.27.0 | MIT OR Apache-2.0 | tests only | Temporary directories |

## Notable transitive dependencies

| Crate | Licence | Pulled in by | Note |
|-------|---------|--------------|------|
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
MIT, Apache-2.0 (with and without the LLVM exception), BSD-3-Clause, Zlib, Unicode-3.0 and
Unlicense (as one alternative of a dual licence).

## Rules

- RAR is extract-only, through libarchive. The `unrar` crate is banned in `deny.toml`.
- Slint, when the GUI arrives, is used only under its GPL-3.0 option.
- Adding a dependency means adding it to this file in the same commit.
