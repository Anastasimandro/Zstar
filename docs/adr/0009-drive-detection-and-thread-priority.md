# 0009: Drive detection and thread priority (crate `sys`)

- Status: Accepted
- Date: 2026-11-03
- Changes: sections 3 (workspace) and 5 (engine: disk detection, priority) of [`../DECISIONS.md`](../DECISIONS.md); completes [0008](0008-control-handle-and-profiles.md)

## Context

0008 left core with `DriveKind`, `Priority` and a `worker_hook`, and no way to fill them in.
Core must stay free of OS code and of `unsafe`, so the OS side needs its own place.

## Decision

- **New crate `crates/sys`** (`zstar-sys`). It depends on no workspace crate, core included, and
  has its own small `DriveKind { Ssd, Hdd, Unknown }` and `Priority { Normal, Low }`; the CLI
  and GUI convert. It is the only crate that may contain `unsafe` (`deny(unsafe_op_in_unsafe_fn)`,
  a SAFETY comment on every block). cli and gui depend on core and sys.
- **API.** `drive_kind(path)`, `detect(path) -> Detection` (kind, bus, device chain, raw values,
  reason, fallback flag), `combine(kinds)` and `set_current_thread_priority(Priority)`.
  Detection is lazy, cached per device in the process, never panics and never starts a process.
  A path that does not exist yet (an output file) is classified through its closest existing
  parent.
- **Combine rule**, for the several paths of one operation: any Hdd, else any Unknown, else Ssd.
  The CLI applies it to source plus output when compressing, archive plus destination when
  extracting. `--drive hdd|ssd` overrides everything.
- **Linux.** `st_dev` -> `/sys/dev/block/MAJ:MIN` -> whole disk (partitions are followed up) ->
  `queue/rotational` (1 Hdd, 0 Ssd, anything else Unknown). dm/md/LVM `slaves` are followed to
  three levels and eight devices; the results are combined by the rule above. Unknown when
  `removable` is 1, when the device path goes through `usb*` or `mmc*`, and when the file system
  (from `/proc/self/mountinfo`) is btrfs, zfs, overlay, fuse* or nfs*. The sysfs root and the
  mountinfo text are parameters, so tests use fake trees.
- **Windows.** Path -> `GetVolumePathNameW` -> `GetVolumeNameForVolumeMountPointW` -> the volume
  opened with **zero desired access** -> `IOCTL_STORAGE_QUERY_PROPERTY` for the seek penalty and
  the bus type. If the seek-penalty query fails: `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS`, then the
  same query on each `\\.\PhysicalDriveN` (reported as `fallback`). No penalty is Ssd, penalty is
  Hdd, any failure is Unknown, never Hdd. USB, RAID, SD, MMC, virtual and Storage Spaces buses
  are Unknown whatever the penalty says (the bus is not the medium). SMB paths and
  `DRIVE_REMOTE` volumes are Unknown.
- **Other systems.** Unknown, and a documented no-op for the priority. macOS is paused (0004);
  Android gets its own module later.
- **Priority.** Windows: background mode (`THREAD_MODE_BACKGROUND_BEGIN`/`END`), entered and left
  once per state change. Linux: nice 10 plus I/O class best-effort level 7 (the lowest class
  that is not starved; idle could be), both per thread; Normal restores nice 0 and the default
  I/O priority.
- **CLI.** Hidden `zstar drive-info <path>` prints class, bus, chain, raw values, `fallback` when
  the second method decided, the reason and the cold time. CI runs it on both systems.

## Dependencies: measured

Direct `libc` (Linux) and `windows-sys` (Windows) were chosen over the `thread-priority` crate.
Both are already in the lock file, `windows-sys` is needed anyway for the IOCTLs, and
`thread-priority` has no I/O priority and would still need `libc` for `ioprio_set`.

| | size of a minimal release binary (Linux x86-64, fat LTO) | crates added |
|---|---|---|
| nice + ioprio with `libc` | 302,392 B | 0 |
| same with `thread-priority` 3.1.1 | 305,560 B (+3,168 B) | 4 (bitflags, cfg-if, log, rustversion) |

On Windows `thread-priority` brings the `windows` crate instead of `windows-sys`; that was not
measured (no Windows toolchain here). `sysinfo` was not adopted and not measured: it covers far
more than this needs. Cost of the whole feature in the CLI: 3,772,296 B -> 3,797,488 B
(+25,192 B, Linux x86-64 release). Details in [`../reports/drive-detection.md`](../reports/drive-detection.md).

## Windows zero-access result

Opening the volume with zero desired access and sending the seek-penalty and device-descriptor
queries **worked on the CI runner** (`windows-latest`, which runs as an administrator): the
first method answered, no fallback was needed. **Not verified for a standard user.** That
needs `zstar drive-info <path>` on a real machine without elevation: a line `fallback: yes` or
`class: unknown` with an access-denied error in the raw values means the premise failed.

## Windows seek-penalty failure on a SATA HDD (2026-11-12)

On the owner's Windows 10 machine with a SATA HDD, the seek-penalty query fails with `os error 31`
(`ERROR_GEN_FAILURE`, "a device attached to the system is not functioning") both on the volume and
on `\\.\PhysicalDrive0`, so `drive-info` says `unknown`. It is not an access problem (that would
be error 5), and the physical-disk fallback of this ADR fails the same way: the storage stack
could not get the property from the device, whatever handle asks.

Options looked at, none adopted yet:

- `StorageDeviceTrimProperty` (8): answers a different question (TRIM support), and TRIM is not a
  test for solid state; the only source consulted calls it a matter of what you are checking for.
- `StorageDeviceMediumProductType` (15): same zero-access query; it may answer where the seek
  penalty does not, or may fail for the same reason. The meaning of its values is not confirmed
  from Microsoft's pages (not fetched), so it is not used to decide.
- ATA pass-through (IDENTIFY, rotation rate): needs a handle with read and write access, which
  contradicts the no-administrator rule above.
- Storage Management / WMI (`MSFT_PhysicalDisk`): COM, a new surface for startup time and size;
  the owner's decision.

So `probe` now also asks for the trim and medium-product-type values, only when the seek-penalty
query failed, and prints them in `drive-info` (`trim_enabled <label>`, `medium_product_type
<label>`). The decision rules did not change. One `drive-info` run on that HDD says whether
either property answers; if `medium_product_type` does, confirm what its values mean before
using it. Windows code is checked by CI only; this was written without a Windows toolchain.

## Windows I/O priority finding

Microsoft's [`SetThreadPriority`](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-setthreadpriority)
page says background mode lowers the thread's "resource scheduling priorities" and explains that
CPU priority alone is not enough for work that hits the disk or memory. It **does not literally
say "I/O priority"**; the same holds for the `SetPriorityClass` page. A third-party article
([Faithlife Code Blog, 2008](https://faithlife.codes/blog/2008/10/using_background_processing_mode_from_c))
states that I/O and memory priority are lowered. So: documented intent, not a literal
confirmation. (Both pages were read through a summarising fetch tool, not in a browser.) To
confirm, watch a gentle extraction's I/O priority on real hardware.

## Known limits

- Linux, unprivileged: after Low, going back to Normal **fails** at `setpriority` (EPERM, measured
  with `setpriv --reuid 65534`) because the kernel does not let a process lower its nice value
  without `CAP_SYS_NICE` or `RLIMIT_NICE`. The I/O priority is restored. A thread that went
  Gentle keeps nice 10 until it ends.
- btrfs, zfs, overlay, fuse and nfs are Unknown, so a btrfs root gets the HDD-safe rules.
- Virtual machines report what the hypervisor tells (the Linux CI runner shows `sda` as
  rotational, the Windows one an NVMe volume). On Linux, virtio is trusted like any other bus.
- The cache is never invalidated during a run.
- Storage Spaces, dynamic volumes and RAID are Unknown on Windows; mapped network drives
  rely on `DRIVE_REMOTE`.
- Not run on real HDD/SSD hardware yet; the owner's `drive-info` outputs are the check.

## Consequences

- The 0008 consequence "without `--drive` the CLI takes the HDD-safe I/O rules" now only holds
  when detection answers Unknown.
- Gentle really lowers priority on Windows and Linux.
