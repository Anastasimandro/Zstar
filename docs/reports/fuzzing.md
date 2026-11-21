# Fuzzing report, 21 Nov 2026

Scope, limits and CI cadence: [ADR 0013](../adr/0013-fuzzing.md).

## Setup of the runs

- Machine: 2 vCPU cloud container, Linux x86_64, nightly-style build (`RUSTC_BOOTSTRAP=1` on stable 1.97), AddressSanitizer, `cargo-fuzz` 0.13.2.
- Per run: 300 s, `-timeout=10 -rss_limit_mb=512 -max_len=65536`, magic-byte dictionary, seeds from `fuzz/seeds/`. Two targets ran at the same time, so execs/s is lower than a single run on this machine.
- Default extraction options and limits. After every input: nothing next to the destination, guard directory unchanged, every symlink inside the destination.

## Results (5 min each)

| Target | Executions | execs/s | Coverage (edges) | Features | Corpus | Peak RSS | Findings |
|--------|-----------:|--------:|-----------------:|---------:|-------:|---------:|----------|
| detect | 914,869 | 3,039 | 4,954 | 12,137 | 964 | 91 MiB | none |
| zip_list_extract | 391,472 | 1,300 | 3,043 | 8,722 | 908 | 109 MiB | none |
| tar_list_extract | 273,100 | 907 | 6,601 | 18,158 | 1,297 | 107 MiB | none |
| sevenz_list_extract | 390,248 | 1,296 | 4,610 | 9,118 | 385 | 79 MiB | none |
| single_stream | 234,910 | 780 | 7,514 | 25,530 | 1,740 | 98 MiB | none |
| path_sanitize | 3,503,436 | 11,639 | 237 | 867 | 274 | 115 MiB | none |

Coverage is libFuzzer's count of covered code edges, not a percentage. `detect` and `path_sanitize` ran with the first seed set; `tar_list_extract` and `zip_list_extract` were run twice, the second time (shown) with the hostile seeds added (see below). Their first runs: tar 193,499 execs at 642/s, cov 6,233; zip 289,253 at 960/s, cov 2,868.

## Bugs

None found in 5 min per target: no crash, hang over 10 s, memory over 512 MiB or write outside the destination. Nothing to fix, so there are no regression fixtures. This does not show the absence of bugs: five minutes is short, and the weekly 20-minute run with a kept corpus is what builds depth.

## Checks of the checks

- The harness has self-tests (`cargo test --manifest-path fuzz/Cargo.toml --lib`) showing that a file beside the destination, a changed or added file in the guard directory, and relative or absolute symlinks pointing out are all detected.
- Hostile seeds (10 tar, 6 zip: `..`, absolute and backslash names, a symlink then a file written through it, symlinks to `/tmp`, hardlinks to `/etc/passwd` and to a path outside, directory replaced by a symlink) were run through the targets: all handled, nothing written outside.

## Not covered, or not verified

- Encrypted entries (no target passes a password); zip and 7z only reach the "password required" error.
- Only the default features were fuzzed; a build with a format feature off is not.
- Extraction limits are the defaults; no target runs with raised limits.
- The CI jobs have not run yet: the 60 s job only runs on pull requests, and work went straight to main. Run the "Fuzz (long run)" workflow by hand once to check it, with a small `seconds` value.
- CI times are therefore estimates: about 3 min per target in the pull request job (cached build about 1 min, 60 s of fuzzing, install and checkout), the six in parallel; weekly run about 25 min wall clock per target in parallel, 2.5 hours of runner time.
- crates.io could not be reached from the working environment, so versions and licences were read from docs.rs and the crates' manifests; release dates are unconfirmed.
- `cargo deny` rejects `libfuzzer-sys` because its licence includes NCSA, which `deny.toml` does not allow. I did not change `deny.toml`; the CI deny job covers the root workspace only. Decision for the owner.
- Seeds total about 35 KB of binary archives, 70 KB as committed hex: more than "a few KB", because the hostile seeds and the 4 KB tar blocks add up.
