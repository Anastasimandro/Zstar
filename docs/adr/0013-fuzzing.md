# 0013: Fuzzing of the formats and the path sanitiser

- Status: Accepted (NCSA allowed for `fuzz/` only, see Consequences)
- Date: 2026-11-21
- Changes: section 9 (quality bar) of [`../DECISIONS.md`](../DECISIONS.md): "fuzzing for every new format" now has a harness; builds on [0007](0007-format-scope.md)

## Context

Zstar opens files it did not make. Section 9 asks for fuzzing of every format, and until now
there was none. The engine promises no zip slip, no decompression bomb and near-constant
memory; a fuzzer is the cheapest way to test that against input nobody wrote by hand.

## Decision

1. **Scope.** Everything reachable from a hostile file in the current formats: detection, zip,
   tar and its five compressed wrappers (gz, zst, bz2, xz, lz4), 7z, the single-file
   streams (gz, zst, bz2, xz, lz4, br) and the entry-name sanitiser. Archive creation is out
   (the input is the user's own files). Encrypted content is out for now: no target passes a
   password, so the zip and 7z decryption paths are only reached up to the "password
   required" error.
2. **Tool.** `cargo-fuzz` with `libfuzzer-sys`, AddressSanitizer, in `fuzz/`. It is its own
   Cargo workspace (listed in `exclude` of the root), with its own `Cargo.lock` started from
   the root one so shared crates have the same versions. `cargo build --workspace` does not
   see it. Linux only, nightly only.
3. **Targets**, one per area: `detect`, `zip_list_extract`, `tar_list_extract`,
   `sevenz_list_extract`, `single_stream`, `path_sanitize`. Each calls the format handler
   directly, so a mutated stream reaches the parser even when its magic bytes are wrong. In
   `tar_list_extract` and `single_stream` the first byte picks the wrapper or codec, and one
   value goes through `zstar_core::extract` with detection, as the CLI does.
4. **What is asserted.** Extraction runs into a temp directory with the default options and
   limits. Afterwards: nothing next to the destination, a guard directory unchanged, and every
   symlink resolving inside the destination. `detect` must rewind. `path_sanitize` is checked
   against an independent rule (NUL, leading separator and `..` components are always
   rejected) and must return only plain relative components.
5. **Limits per input:** 10 s timeout and 512 MiB RSS (`-timeout=10 -rss_limit_mb=512`). Going
   over either is a bug in Zstar, never a reason to raise the limit. ASan's quarantine is set
   to 16 MiB (default 256 MiB): it counts as RSS and would take half of the budget with no
   relation to the code under test.
6. **Seeds.** Small valid archives made by Zstar, 7-Zip 23.01 and libarchive (bsdtar) 3.7.2,
   plus the independent `gzip` and `xz` tools, and hostile names for the sanitiser. They are
   committed as hex text (`fuzz/seeds/`), not as archives, because the project never commits
   binary files; `fuzz/seeds.py` unpacks them. A dictionary of magic bytes is in
   `fuzz/dictionaries/`.
7. **CI.** Pull requests: `ci.yml` runs each target for 60 s on Linux and fails on any crash,
   hang or overrun. Weekly (Sunday) and on demand: `fuzz.yml` runs each target for 20 min
   (adjustable), keeps the corpus between runs with `actions/cache` (restore the newest,
   minimise with `cargo fuzz cmin`, save under a new key) and uploads crashing inputs as
   artifacts. Actions are pinned by commit hash, cargo-fuzz is installed with `--locked` at an
   exact version, `cargo fetch --locked` stops the run if the fuzz lock would change, and no
   secret is used. The toolchain is the floating `nightly`; pin a date if it ever breaks CI
   for reasons unrelated to Zstar.
8. **Every new format ships with its target.** RAR, CAB, cpio, ar and deb get one when each
   lands, using the same helpers.

## Consequences

- A crash found by CI is a bug: fix it in its own commit with the minimised input as a
  regression test.
- `libfuzzer-sys` is licensed `(MIT OR Apache-2.0) AND NCSA` (its bundled LLVM libFuzzer is
  under NCSA, an OSI-approved BSD-style licence). Owner's decision: NCSA is allowed in
  `fuzz/deny.toml` only and is not in the root `deny.toml`, because the harness is a dev tool
  that is never built, linked or packaged into a release binary.
- The `deny` job in `ci.yml` checks both workspaces: the root with `deny.toml`, `fuzz/` with
  `fuzz/deny.toml`. The two files are identical except for the NCSA line; keep them in step.
- Each pull request spends about six short jobs; the weekly run about two hours of runner time.
- Extraction writes real files, so throughput is about a thousand inputs per second for zip
  and 7z, and tens of thousands for the sanitiser. See [`../reports/fuzzing.md`](../reports/fuzzing.md).
