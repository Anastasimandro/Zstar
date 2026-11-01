# Parallel deflate (gz, tar.gz) and the zip limitation

Measured on one deterministic 1 GiB file (half word-salad text, half pseudo-random bytes;
`scripts/bench-1gib.sh`), level 6, release build, a 2-CPU machine. Output verified with
`gzip -t` and `cmp` against the original.

| option | threads | block | time | peak RAM | output |
|---|---|---|---|---|---|
| zlib-rs, one stream (the previous code) | 1 | - | 21.9-23.5 s | 3 MiB | 719.6 MiB |
| B: zlib-ng backend (C + cmake), one stream | 1 | - | 23.4-25.6 s | 3 MiB | 719.6 MiB |
| A: independent blocks, pigz style (zlib-rs) | 1 | 128 KiB | 24.7 s | 4 MiB | 719.6 MiB |
| A | 2 | 128 KiB | 15.4 s | 5 MiB | 719.6 MiB |
| A | 2 | 1 MiB | 13.4 s | 16 MiB | 719.5 MiB |
| A with zlib-ng | 2 | 128 KiB / 1 MiB | 15.2 s / 14.1 s | 5 / 15 MiB | 719.6 / 719.5 MiB |

- **B gives nothing**: zlib-rs is already a port of zlib-ng's algorithms; same time, same size.
  libdeflate was ruled out: it needs the whole input in memory and has no sync flush.
- **A is used** (blocks of 256 KiB, `formats/pargz.rs`). Each block is primed with the previous
  32 KiB and ends in a sync flush, so the size matches a single stream and any gzip reader
  reads it. Scaling beyond 2 threads could not be measured here.
- A first zlib-rs run took 84 s: a cold page cache, not the codec. Repeated, it is 23 s.

## Zip: not done, and why

`zip` 9.1.3 has no streaming way to add an entry that is already compressed.
`PreparedZipFile` can only be built by `ZipFileBuilder`, which compresses on its own and keeps
the whole compressed entry in memory; `raw_copy_file` needs an existing archive to copy from.
So a parallel deflate for one large zip entry would either break the near-constant-memory rule
or need one of:

1. a temporary single-entry zip on disk (spill of the compressed size) plus `raw_copy_file`;
2. our own zip writer (local headers, central directory, zip64, AES), replacing the crate for
   creation;
3. a streaming raw-entry API contributed to the `zip` crate.

Until one is chosen, zip entries larger than 1 MiB compress on one thread (as before); many
small files already compress in parallel.
