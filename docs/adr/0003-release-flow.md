# 0003: Release flow

- Status: Accepted
- Date: 2026-10-30
- Changes: section 8 of [`../DECISIONS.md`](../DECISIONS.md) (tag-triggered draft releases), authorised by the owner

## Context

Section 8 planned a build on every tag `vX.Y.Z` that produces a draft Release. The pipeline that
grew out of it published a release for every green commit and numbered them automatically
(`0.1.1`, `0.1.2`, ...), which filled the releases page with builds nobody chose and treated
everything below 1.0 as a pre-release.

## Decision

The Release workflow still runs after CI (`workflow_run`), only for pushes to main:

- **No `[release]` in the commit message**: update the single rolling pre-release `nightly`
  (tag moved to the commit, assets replaced). Binaries report `<Cargo version>-nightly.<7-char sha>`.
- **`[release]` in the message**: the version is `version` in `Cargo.toml`. If `vX.Y.Z` already
  exists the run fails with a message and never overwrites. Otherwise the tag is created and the
  release is published directly, no draft. A hyphen in the version (`-rc.1`, `-beta.1`) makes it a
  pre-release; otherwise it is stable and marked Latest. Such a commit does not update `nightly`.
- **Pull requests**: CI only, never a release.
- Every release, nightly included: `SHA256SUMS`, a `--version` smoke test of each packaged binary,
  generated notes.
- Removed: automatic patch numbering, `[skip release]`, "below 1.0 is a pre-release".
- Hardening: every action pinned by full commit hash; `contents: read` by default and
  `contents: write` only in the publish job; `GITHUB_TOKEN` only, no secrets.

## Consequences

- A release is an explicit act: bump the version and say `[release]` (see `../RELEASING.md`).
- `nightly` is a moving target. Do not rely on its tag; use the `vX.Y.Z` releases.
- The `release` concurrency group queues runs and keeps only one waiting run, so two pushes in
  quick succession while another run is going may drop the older waiting run. Re-push if a
  `[release]` run was dropped.
- Still open from section 8: signing, attestations and a protected release environment. They are
  added to this flow when their accounts exist.
