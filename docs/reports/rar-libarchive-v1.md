# RAR v1 (read only) through libarchive: results

Status: code complete behind the Cargo feature `rar` (off by default, absent from `release.yml`).
**Fixture table NOT FILLED: the fixtures were not available.** Publishing is blocked by
[`rar-provenance.md`](rar-provenance.md) (not signed off). Design: [ADR 0014](../adr/0014-rar-read-only-libarchive.md).

## Expected results per fixture (to confirm, not yet run)

The 24 koni_rar files and the WinRAR files (RAR3, RAR5, 2-volume RAR5) must be placed by the owner in
`tests/fixtures/rar/` together with the upstream LICENSE/attribution (check it first). Nothing was
invented. Until then the "Result" column is empty on purpose.

| Case | Expected | Result |
|------|----------|--------|
| RAR5 plain, stored / compressed | extracts OK | not run |
| RAR3 plain, non-solid | extracts OK | not run |
| RAR3 solid | typed error `Unsupported` ("RAR solid archive support unavailable") | not run |
| RAR3 with filters | typed error `Unsupported` ("Parsing filters is unsupported") | not run |
| `ppmd_rar4_runs.rar` | **known gap**: failed on libarchive 3.7.4; not yet confirmed on 3.8.11. Any outcome is recorded as is | not run |
| Encrypted data or headers (RAR3/RAR5) | typed error "encrypted RAR not supported", nothing written | headers: tested on hand-built stubs; real files not run |
| RAR5 dictionary > 64 MiB | typed error `Unsupported` ("Declared dictionary size is not supported") | not run |
| 2-volume RAR5 | typed error `MultiVolumeUnsupported` | stub tested; real files not run |

Rows say "extracts OK / typed error / wrong output"; a wrong output would be listed here, never hidden.

## What was tested without fixtures

- 14 unit tests in `zstar-rar` (header inspection, error classification with the real 3.8.11 messages, panic in a stream, I/O errors).
- 6 tests in `crates/core/tests/rar_stubs.rs` on hand-built headers (detection, empty archive, volume, encrypted headers, password refused, truncation/bit-flip no crash).
- `rar_can_never_be_created` and the CLI refusal of `--format rar`.
- `cargo clippy --workspace --all-targets --locked -D warnings`, `cargo deny --locked check` (root and fuzz): pass.

## Measurements (Linux x86_64, release, lto fat, panic=abort; reported, not tuned)

| Item | Without `rar` | With `rar` |
|------|---------------|------------|
| `zstar` binary | 4,029,960 B | 4,222,032 B (+192,072 B, +4.8%) |
| `hyperfine zstar --version` | ~1.7-1.8 ms | ~1.7-1.8 ms (a first run showed +21%; reversed order showed it was an ordering artefact) |
| Peak RAM, ~1 GB RAR5 extraction | not measured | **not measured**: no such file and RAR cannot be created here |

## Platforms (CI job `rar-build`)

Linux gnu, Linux musl, Windows MSVC, macOS arm64, Android arm64. Only Linux gnu was built locally.
The others run in CI only (this environment cannot install those targets); results are reported
after the push, not assumed.

## Fuzzing

Target `rar_list_extract` (10 s per input, 512 MB), dictionary with RAR4/RAR5 signatures, six
synthetic seeds in `fuzz/seeds/rar_list_extract/` (not fixtures). It builds and passes check/clippy
locally. **The 5-minute run was not done**: libFuzzer needs nightly and the toolchain cannot be
installed here. Plan: `fuzz.yml` via workflow_dispatch with 300 s, after the fixtures are added as seeds.

## Not confirmed / known limits

- libarchive 3.8.11 tarball SHA-256 was computed locally; no upstream digest or GPG key was reachable, so it is **unverified**.
- libarchive is not patched; known gaps (no encryption, no solid RAR3, RAR3 filters, dictionary > 64 MiB) are visible, not worked around. The RAR3 PPMd memory limit (256 MiB) comes from reading the code, not from a measurement.
- With `panic = "abort"` a panic inside a callback aborts instead of becoming an error (memory-safe; documented in `zstar-rar`).
- Multi-volume is refused: the handler gets one stream and cannot find the other volumes.
- No selected-entry extraction in `zstar-core` for any format; RAR progress has no percentage.
- Bindings: 31 hand-written functions (the task estimated ~20).
