# 0001: License

- Status: Accepted
- Date: 2026-10-27

## Context

Zstar is an open source archive manager with a public GitHub repository. The release pipeline plans to sign Windows builds through SignPath Foundation, which only accepts projects under an OSI-approved license with no commercial dual-licensing. The GUI uses Slint, which is offered under several licenses, one of which (Royalty-free) is not OSI-approved.

## Decision

1. The whole repository is licensed under GPL-3.0-only (SPDX identifier `GPL-3.0-only`). The `LICENSE` file at the repository root holds the GPLv3 text.
2. Slint is used only under its GPL-3.0 option. The Slint Royalty-free license is never used.

## Consequences

- Every workspace crate declares `license = "GPL-3.0-only"` in its `Cargo.toml` (set once through workspace metadata).
- Every dependency must be compatible with GPL-3.0-only and pass `cargo-deny`. Before adding one, its license is verified from the official source.
- No dependency or code under a proprietary or non-OSI license is added.
- The README carries the Slint attribution and the SignPath credit.
- Third-party notices are generated and kept up to date with `cargo-deny`.
- The license rule is never weakened to make a build or a dependency pass.
