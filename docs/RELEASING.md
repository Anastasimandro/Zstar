# Releasing

Every push to main that passes CI updates the pre-release `nightly`. Nothing else to do.

To publish a version, in one commit on main:

1. Bump `version` in `Cargo.toml` (`0.2.0`, or `0.2.0-rc.1` for a pre-release). The same file
   repeats it on the `zstar-core` line under `[workspace.dependencies]` (cargo-deny rejects a
   path dependency without a version): change both. Then update `Cargo.lock` (`cargo check`),
   since builds use `--locked`.
2. Put `[release]` in the commit message.

After CI passes, the Release workflow tags `vX.Y.Z` and publishes it. A version with a hyphen is a
pre-release; without one it is stable and marked Latest. If the tag already exists the run fails
and changes nothing: bump the version and commit again.

Releases currently ship Linux (x64) and Windows (x64) only; macOS is paused
([ADR 0004](adr/0004-macos-paused.md)).

Pull requests never release. Background: [ADR 0003](adr/0003-release-flow.md).
