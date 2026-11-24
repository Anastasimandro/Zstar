# RAR v1 (read only) through libarchive: results

Status: code complete behind the Cargo feature `rar` (off by default, absent from `release.yml`).
Fixture table filled from a real run (below). Publishing is still blocked by
[`rar-provenance.md`](rar-provenance.md) (not signed off). Design: [ADR 0014](../adr/0014-rar-read-only-libarchive.md).

## Results per case, from the real fixtures

The 24 koni_rar files (MIT, `tests/fixtures/rar/LICENSE-koni_rar` and `NOTICE-koni_rar`) are the only
real archives. Test: `crates/core/tests/rar_fixtures.rs` (list, then extract, through the common
interface; libarchive 3.8.11; Linux x86_64 debug). Content is checked by CRC-32 only: the expected
value is the one stored in each archive's own file header, read by the test; no other tool is used.
Outcomes are "extracts OK / typed error / wrong output". **No fixture ended as wrong output.**
Rows 1-8 are the cases of the earlier expected column; rows 9-10 are cases the fixtures added.

| # | Case (fixtures) | Expected | Result |
|---|-----------------|----------|--------|
| 1 | RAR5 plain, stored / compressed (`store`, `normal`) | extracts OK | extracts OK: 4 files + 3 dirs each, CRC-32 matches |
| 2 | RAR3 plain, non-solid (`ppmd_rar4`, `ppmd_switch`, method 0x35) | extracts OK | extracts OK, CRC-32 matches. **no fixture** of RAR3 stored/LZ-only, unencrypted, non-solid |
| 3 | RAR3 solid (`solid_rar4`, `solid_ppmd`) | typed error `Unsupported` ("RAR solid archive support unavailable") | as expected: `UnsupportedFormat` "RAR: RAR solid archive support unavailable" at list and at extract. Extract still leaves the first entry in the destination (`solidbig` / `s1.txt`) before failing |
| 4 | RAR3 with filters (`filter_audio`, `filter_delta`, `filter_e8`, `filter_rgb`) | typed error `Unsupported` ("Parsing filters is unsupported") | **differs from expected: extracts OK**, CRC-32 matches, all four. I did not check that the files really contain filter blocks beyond their names (headers: RAR 2.9, method 0x35) |
| 5 | `ppmd_rar4_runs` | known gap, any outcome recorded | typed error `InvalidArchive` "corrupt RAR archive: Internal error extracting RAR file", nothing left in the destination. Still fails on 3.8.11 (the other two PPMd files, `ppmd_rar4` and `ppmd_switch`, extract OK) |
| 6 | Encrypted data or headers (data: `encrypted`, `encrypted_compressed`, `encrypted_solid`, `enc_rar4`, `enc_rar4_store`; headers: `encrypted_headers`, `hp_rar4`, `hp_rar4_store`) | typed error "encrypted RAR not supported", nothing written | as expected for all 8: `EncryptedUnsupported` at list and at extract, destination empty |
| 7 | RAR5 dictionary > 64 MiB | typed error `Unsupported` | **no fixture** (largest dictionary in the set: 1024 KiB) |
| 8 | 2-volume RAR5 | typed error `MultiVolumeUnsupported` | **no fixture** (no fixture has the volume flag; the hand-built stub test still covers the error) |
| 9 | RAR5 solid (`best_solid`) | not in the expected column | typed error `UnsupportedFormat` "RAR: Unsupported block header size (was 3, max is 2)" at extract (list OK); the directory `nested` was already created in the destination. Not investigated further |
| 10 | RAR 2.x non-solid (`rar2_audio`, `rar2_lz`, `rar2_lz_repeat`; headers: unpack version 20) | not in the expected column | typed error `InvalidArchive` "corrupt RAR archive: Prefix found", nothing left in the destination. I did not check whether the files are damaged or the format is just unsupported; the error text says corrupt. `rar2_solid` stops earlier with the solid error of row 3 at list |

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
| Peak RAM, extracting the largest fixture (`store.rar`, RAR5 stored, 100,399 B, one run) | not measured | **13,152 KiB** peak RSS. The same harness running `zstar --version` read 13,144 KiB, so the harness floor dominates: this says "about 12.8 MiB at most" for a small archive, nothing about a ~1 GB one |

## Platforms (CI job `rar-build`, run 45471824957 on main, commit ea8c782)

| Platform | Result |
|----------|--------|
| Linux gnu | pass: build, unit tests, `rar_stubs`, `rar_fixtures` (same outcomes as the table), CLI build |
| Linux musl | pass: `zstar-rar` builds (build only, no tests run) |
| Windows MSVC | pass: same steps as Linux gnu; `rar_fixtures` outcomes identical to the table |
| macOS arm64 | pass: same steps as Linux gnu; `rar_fixtures` outcomes identical to the table |
| Android arm64 | pass on the first run: `zstar-rar` cross-builds (build only, no tests run) |

## Fuzzing

Target `rar_list_extract` (10 s per input, 512 MB), dictionary with RAR4/RAR5 signatures. Seeds: the
six synthetic stubs in `fuzz/seeds/rar_list_extract/` plus the 24 real fixtures, read straight from
`tests/fixtures/rar/` by `fuzz/seeds.py` (no second copy; `store.rar`, 100,399 B, is cut to the
65,536 B `-max_len`). `fuzz.yml` via workflow_dispatch, 300 s (run 45471840790): 30 seed files,
445,524 executions in 301 s (~1,480/s), peak RSS 88 MB, **no crash, no timeout, no OOM**. The other
six targets ran in the same dispatch and also ended without findings.

## Not confirmed / known limits

- libarchive 3.8.11 tarball SHA-256 was computed locally; no upstream digest or GPG key was reachable, so it is **unverified**.
- libarchive is not patched; known gaps (no encryption, no solid RAR3, RAR3 filters, dictionary > 64 MiB) are visible, not worked around. The RAR3 PPMd memory limit (256 MiB) comes from reading the code, not from a measurement.
- With `panic = "abort"` a panic inside a callback aborts instead of becoming an error (memory-safe; documented in `zstar-rar`).
- Multi-volume is refused: the handler gets one stream and cannot find the other volumes.
- No selected-entry extraction in `zstar-core` for any format; RAR progress has no percentage.
- Bindings: 31 hand-written functions (the task estimated ~20).
