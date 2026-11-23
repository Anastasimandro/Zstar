# RAR reader provenance (gate for publishing the `rar` feature)

Status: **NOT SIGNED OFF.** Until the owner signs off at the bottom of this file, `release.yml`
must not enable the `rar` feature, and it does not. This is a record of what libarchive itself says
about where its RAR and RAR5 readers come from. It is not a legal conclusion; the owner is not a
lawyer and neither is the author of this file.

Marks: **VERIFIED** = read by us in a primary source on 22-23 Nov 2026 (the quoted file or page).
**CLAIMED** = stated by someone but not checkable from what we could reach.
**NOT FOUND** = we looked and libarchive says nothing.

Method: we read the libarchive 3.8.11 release tarball (the one vendored in
`crates/rar-sys/vendor/`) and searched every RAR-related file, `NEWS`, `README.md`, `COPYING`
and the man page for words such as unrar, rarlab, technote, specification, reverse engineering,
clean room, based on, derived. We then fetched upstream pages through the web tool. We could not
reach libarchive's mailing list archive or its full issue history, so absence below means "not
found where we could look", not "does not exist".

## Statements by libarchive about the readers

| # | Statement | Where | Mark |
|---|-----------|-------|------|
| 1 | "RAR and RAR 5.0 archives (with some limitations due to RAR's proprietary status)" | `README.md` of the 3.8.11 tarball, list of supported formats. [Upstream README](https://github.com/libarchive/libarchive/blob/master/README.md) | VERIFIED |
| 2 | "Libarchive has limited support for reading RAR format archives. Currently, libarchive can read RARv3 format archives ..." (does not mention RAR5) | `libarchive/libarchive-formats.5`, section RAR, 3.8.11 tarball | VERIFIED (text is dated: README and NEWS list RAR5) |
| 3 | NEWS entries: "Jun 26, 2011: RAR reader"; "Oct 06, 2018: RAR 5.0 reader" | `NEWS`, 3.8.11 tarball | VERIFIED |
| 4 | RAR (v3) reader file header: "Copyright (c) 2003-2007 Tim Kientzle", "Copyright (c) 2011 Andres Mejia", BSD-2-Clause. No statement of where the format knowledge came from. | `archive_read_support_format_rar.c` (3.8.11 tarball; same header in [v3.1.2](https://fuchsia.googlesource.com/third_party/libarchive/+/v3.1.2/libarchive/archive_read_support_format_rar.c)) | VERIFIED (the header); origin of knowledge NOT FOUND |
| 5 | RAR5 reader file header: "Copyright (c) 2018 Grzegorz Antoniak (http://antoniak.org)", BSD-2-Clause. No statement of where the format knowledge came from. Comments name no document, specification or other implementation. | `archive_read_support_format_rar5.c`, 3.8.11 tarball and [master](https://github.com/libarchive/libarchive/blob/master/libarchive/archive_read_support_format_rar5.c) | VERIFIED (the header and the absence of any source comment in the whole file); origin of knowledge NOT FOUND |
| 6 | PPMd7 decoder: "2010-03-12 : Igor Pavlov : Public domain" and "This code is based on PPMd var.H (2001): Dmitry Shkarin : Public domain" | `archive_ppmd7.c`, 3.8.11 tarball | VERIFIED |
| 7 | Licence map: all C sources BSD-2-Clause except `archive_parse_date.c` (public domain), the BLAKE2 files (CC0 / OpenSSL / Apache-2.0) and compress filter files (UC Regents BSD-3). | `COPYING`, 3.8.11 tarball | VERIFIED |
| 8 | The FreeBSD import of the RAR5 reader ([r339644](https://reviews.freebsd.org/rS339644?diff=1)) carries the same BSD header; the page shows no commit message. | FreeBSD review page | VERIFIED (header); origin NOT FOUND |

## Statements by others that bear on it (not by libarchive)

| # | Statement | Where | Mark |
|---|-----------|-------|------|
| 9 | RARLAB publishes "RAR 5.0 archive format" and says "If you need information about algorithms or more detailed information on data structures, please use UnRAR source code." The page states no licence or restriction. | [rarlab.com/technote.htm](https://www.rarlab.com/technote.htm) | VERIFIED (page text) |
| 10 | The UnRAR source licence forbids using the source to re-create the RAR compression algorithm. That is why Zstar never creates RAR and bans the `unrar` crate. | UnRAR licence text, e.g. [Chromium's copy](https://chromium.googlesource.com/chromium/src/+/master/third_party/unrar/LICENSE) | CLAIMED (we did not re-read its full text in this task; the project decision to ban it stands either way) |

## What this does NOT establish

- Whether the RAR3 or RAR5 reader authors worked from the RARLAB technote, from the UnRAR source,
  from observation of archives, or from other implementations: **NOT FOUND**. libarchive's files
  and README say only that the formats are "proprietary" and support is "limited".
- Whether the RAR3 filters, RAR5 decoder or AES code were derived from UnRAR: **NOT FOUND**. Item 9
  suggests the technote alone does not cover the decoder, which makes the question real, not
  answered.
- The contents of libarchive's pull requests, issue tracker and mailing list from 2011 and 2018 for
  these two files: **not reviewed** (the pages we tried were not reachable from our environment).
  Someone with access should read them before sign-off. Suggested starting points: the first commit
  of `archive_read_support_format_rar.c` (2011, Andres Mejia) and the 2018 RAR5 pull request by
  Grzegorz Antoniak.
- Anything about patents or trademarks ("RAR", "WinRAR").

## Decision needed from the owner

The technical work is finished behind the Cargo feature `rar`, which is off by default in `zstar-core`
and `zstar-cli` and absent from `release.yml`. To publish, the owner decides whether the evidence
above (and whatever further reading or advice the owner chooses to obtain) is enough, then records
it here and changes `release.yml` in a separate commit.

Sign-off: _not given._ Name / date / scope: ______________________
