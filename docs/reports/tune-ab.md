# `--tune` A/B sanity run

Release build, Linux VM (virtio disk detected as hdd), 256 MiB (half random, half repetitive),
`tar.gz`, page cache warm, 3 runs each, seconds, `zstar compress` then `zstar extract`.

| run | compress plain | compress `--tune` | extract plain | extract `--tune` |
| --- | --- | --- | --- | --- |
| 1 | 1.517 | 1.473 | 0.206 | 0.173 |
| 2 | 1.524 | 1.556 | 0.229 | 0.201 |
| 3 | 1.522 | 1.523 | 0.232 | 0.177 |

Compress is within noise. Extract looks 10-20 % faster with `--tune`, but this is one VM with a
warm cache and 3 runs: it is not a claim. The archive is byte-identical (tested). Remembered
volume line after the runs: `hdd:virtio:vda  2 2 1048576`.

Not measured: real HDD, SATA SSD, NVMe, USB, Windows, macOS. `--tune` stays opt-in until it is.
