# Drive detection: measurements (ADR 0009)

Cold time of the first `detect()` call, as printed by `zstar drive-info`.

| system | build | cold call |
|---|---|---|
| Linux (Firecracker VM, virtio disk), 10 runs | release | 0.154 to 0.239 ms, median about 0.17 ms |
| Linux, GitHub `ubuntu-latest` | debug | 0.259 ms |
| Windows, GitHub `windows-latest` | debug | 0.340 ms |
| macOS | paused | not measured |

All under the 5 ms target. CI numbers are debug builds of the `sys` crate, so they are an upper
bound. Real HDD and SSD times are pending the owner's runs.

CLI release binary, Linux x86-64: 3,772,296 B before, 3,797,488 B after (+25,192 B, +0.67 %).
Windows CLI size was not measured.

Dependency comparison (minimal release binaries, fat LTO): `libc` only 302,392 B;
`thread-priority` 3.1.1 plus `libc` 305,560 B, with four extra crates.

CI `drive-info` output on the Windows runner (administrator): volume query answered directly
(`seek_penalty=false`, bus 17 nvme), class ssd, no fallback.
