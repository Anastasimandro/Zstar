# Third-party notices

Zstar is GPL-3.0-only. Source and binary distributions that include the optional `rar` feature
contain code from libarchive. Its notice is reproduced here and in
`crates/rar-sys/vendor/libarchive/COPYING`; keep it with every copy.

## libarchive 3.8.11 (RAR and RAR5 reader only)

The libarchive distribution as a whole is Copyright by Tim Kientzle. Only these parts are built
into Zstar: `archive_read`, the RAR and RAR5 format readers, the PPMd7 decoder, the entry, string,
ACL and utility code they link against, CRC-32 and BLAKE2 (see `crates/rar-sys/build.rs`).

Licence of the individual files, as stated in each file and in libarchive's COPYING:

- Everything not listed below: the BSD-2-Clause notice that follows.
- `archive_ppmd7.c`: public domain (Igor Pavlov, based on Dmitry Shkarin's PPMd var.H).
- `archive_blake2*`: triple-licensed CC0 1.0, OpenSSL or Apache-2.0, at the user's choice (Samuel
  Neves). Zstar uses them under Apache-2.0.

```
Copyright (c) 2003-2018 <author(s)>
All rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions
are met:
1. Redistributions of source code must retain the above copyright
   notice, this list of conditions and the following disclaimer
   in this position and unchanged.
2. Redistributions in binary form must reproduce the above copyright
   notice, this list of conditions and the following disclaimer in the
   documentation and/or other materials provided with the distribution.

THIS SOFTWARE IS PROVIDED BY THE AUTHOR(S) ``AS IS'' AND ANY EXPRESS OR
IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES
OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE DISCLAIMED.
IN NO EVENT SHALL THE AUTHOR(S) BE LIABLE FOR ANY DIRECT, INDIRECT,
INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT
NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF
THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

The RAR5 reader is Copyright (c) 2018 Grzegorz Antoniak; the RAR reader Copyright (c) 2003-2007
Tim Kientzle; `archive_util.c` and `archive_windows.c` Copyright Michihiro Nakajima; `archive_time.c`
Copyright (c) 2025 Loic Jean David Arjanen; `archive_crc32.c` Copyright (c) 2009 Joerg Sonnenberger.
Each file carries the same BSD-2-Clause terms in its own header.

This notice covers the source files only. Whether the *format* knowledge behind a reader is
unencumbered is a separate question, tracked in `docs/reports/rar-provenance.md`; it is not a legal
conclusion.
