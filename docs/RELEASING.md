# Releasing

Every push to main that passes CI updates the pre-release `nightly`. Nothing else to do.

To publish a version, in one commit on main:

1. Bump `version` in `Cargo.toml` (`0.2.0`, or `0.2.0-rc.1` for a pre-release) and update
   `Cargo.lock` to match (`cargo check`), since builds use `--locked`.
2. Put `[release]` in the commit message.

After CI passes, the Release workflow tags `vX.Y.Z` and publishes it. A version with a hyphen is a
pre-release; without one it is stable and marked Latest. If the tag already exists the run fails
and changes nothing: bump the version and commit again.

Pull requests never release. Background: [ADR 0003](adr/0003-release-flow.md).
