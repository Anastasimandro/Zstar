# 0004: macOS temporarily paused

- Status: Accepted (temporary)
- Date: 2026-10-31
- Changes: sections 2 (platforms) and 8 (tests on three OSes) of [`../DECISIONS.md`](../DECISIONS.md), authorised by the owner

## Context

GitHub's macOS arm64 hosted runners are short on capacity ("jobs targeting macOS arm64 runners
may experience longer queue times"). The `test (macos-latest)` job was never assigned a runner in
three attempts over several hours, which blocked every green CI run and with it every release.

## Decision

Pause macOS in CI and releases until the condition below is met:

- `ci.yml`: the test matrix is Linux and Windows.
- `release.yml`: only `linux-x64` and `windows-x64` are built, smoke-tested and published;
  `SHA256SUMS` and the notes cover those two archives.
- Nothing is deleted: macOS code, `cfg` branches and `shell-mac` plans stay as they are.
- README and `RELEASING.md` state that releases ship Linux and Windows only.

## Not tested meanwhile

- The whole test suite on macOS (paths, case-insensitive and Unicode-normalising file systems,
  symlinks, permissions, `ctrlc` and `nix` behaviour on macOS).
- That the CLI builds and passes `--version` on macOS; no macOS binary is published.
- Any macOS-only `cfg` code can break unnoticed.

## Condition to re-enable

Restore macOS to the test matrix and to the release matrix (and the archive count in the publish
job) **before Phase 4 (`shell-mac`) starts, and in any case before the first stable release**
(a version without a hyphen). If capacity is still a problem then, look for another kind of
macOS runner (larger or self-hosted) rather than leaving macOS out.
