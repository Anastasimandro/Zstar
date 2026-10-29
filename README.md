# Zstar

Fast, low-memory archive manager for Windows, Linux, macOS and Android. Open source
(GPL-3.0-only), written in Rust. Goals, in order: speed, low
memory use and fast startup; then a clean, intuitive design.

This repository currently holds the engine and the command line:

- `crates/core`: the archive engine (no UI, stream-based). zip, tar, tar.gz and tar.zst.
- `crates/cli`: the `zstar` command.

```
zstar compress backup.tar.zst photos/ notes.txt
zstar list backup.tar.zst
zstar extract -o restore/ backup.tar.zst
```

The format is chosen from the output name when creating and detected from the file's
contents when reading. Run `zstar --help` for the options, including the decompression-bomb
limits.

## Build

```
cargo build --release --locked
cargo test --locked
```

A C compiler is needed (the zstd library is built from source). Dependencies and their
licences are listed in [DEPENDENCIES.md](DEPENDENCIES.md); closed decisions in
[docs/DECISIONS.md](docs/DECISIONS.md) and the ADRs in [docs/adr/](docs/adr/).

RAR support will be extract-only. Creating RAR files is, and will stay, unsupported.
