# 0007: Format scope

- Status: Accepted
- Date: 2026-11-01
- Changes: section 4 (formats) of [`../DECISIONS.md`](../DECISIONS.md)

## Context

The format list needs a few precise rules so that no task has to guess them.

## Decision

- **tar.\*** archives are opened and created as one archive in a single pass: one list of files,
  and never an intermediate `.tar` on disk.
- **Brotli** has no magic bytes, so `.br` and `.tar.br` are detected by extension only.
- **RAR** is read-only through libarchive, without passwords in v1. Creating RAR stays forbidden
  and the `unrar` crate stays banned. A later optional bridge may call an external UnRAR or 7z
  executable already installed by the user, on Windows only; it is never bundled or linked.
- **Out of scope:** ISO, and libarchive's extra formats (xar, lha, warc, mtree, rpm).

## Consequences

- tar.* streaming must work without seeking back; listing a large compressed tar costs a full
  decompression pass until a cached index exists (post-v0.1, only with a measurement).
- The RAR bridge, if ever built, needs its own ADR before any code.
