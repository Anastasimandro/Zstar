//! Linux: `st_dev` -> `/sys/dev/block/MAJ:MIN` -> whole disk -> `queue/rotational`.
//!
//! The sysfs root and the mountinfo text are parameters of [`resolve`], so tests run it on
//! fake trees.

use std::fs::{self, File};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};

use crate::rules::combine;
use crate::{Detection, DriveKind, Priority, cached, existing_ancestor};

/// `st_dev` of `path` or its closest existing parent.
pub(crate) fn device_id(path: &Path) -> Option<u64> {
    fs::metadata(existing_ancestor(path)).ok().map(|m| m.dev())
}

/// Plain open plus a sequential-access hint. The hint's result is ignored on purpose.
pub(crate) fn open_sequential(path: &Path) -> io::Result<File> {
    let file = File::open(path)?;
    // SAFETY: posix_fadvise takes the descriptor, which `file` keeps open for the whole call,
    // and two plain integers (offset 0, length 0 = to the end of the file); it reads no memory.
    let _ = unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_SEQUENTIAL) };
    Ok(file)
}

/// How deep `slaves` (device-mapper, md) are followed, and how many at one level.
const MAX_DEPTH: u8 = 3;
const MAX_SLAVES: usize = 8;

pub(crate) fn detect(path: &Path) -> Detection {
    let p = existing_ancestor(path);
    let dev = match fs::metadata(&p) {
        Ok(m) => m.dev(),
        Err(e) => return Detection::unknown(format!("cannot stat {}: {e}", p.display())),
    };
    let (major, minor) = (major(dev), minor(dev));
    cached(format!("{major}:{minor}"), || {
        let mountinfo = fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
        resolve(Path::new("/sys"), &mountinfo, major, minor)
    })
}

/// glibc's `major()` and `minor()` for the 64-bit `dev_t` of Linux.
fn major(dev: u64) -> u32 {
    (((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff)) as u32
}

fn minor(dev: u64) -> u32 {
    ((dev & 0xff) | ((dev >> 12) & !0xff)) as u32
}

/// File system type of the mount whose device is `major:minor`, from `/proc/self/mountinfo`
/// text: `id parent maj:min root mount opts [optional...] - fstype source superopts`.
fn fstype_of(mountinfo: &str, major: u32, minor: u32) -> Option<&str> {
    let want = format!("{major}:{minor}");
    mountinfo.lines().find_map(|line| {
        if line.split(' ').nth(2) != Some(want.as_str()) {
            return None;
        }
        line.split_once(" - ")?.1.split(' ').next()
    })
}

/// File systems that do not map to one plain block device (or that hide it).
fn hides_device(fstype: &str) -> bool {
    matches!(fstype, "btrfs" | "zfs" | "overlay")
        || fstype.starts_with("fuse")
        || fstype.starts_with("nfs")
}

fn read(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_owned())
}

pub(crate) fn resolve(sysfs: &Path, mountinfo: &str, major: u32, minor: u32) -> Detection {
    let mut d = Detection::unknown("");
    if let Some(fstype) = fstype_of(mountinfo, major, minor) {
        d.raw("fstype", fstype);
        if hides_device(fstype) {
            d.reason = format!("file system {fstype} can span or hide devices");
            return d;
        }
    }
    let link = sysfs.join("dev/block").join(format!("{major}:{minor}"));
    let Ok(real) = fs::canonicalize(&link) else {
        d.reason =
            format!("no block device {major}:{minor} in sysfs (network or virtual file system?)");
        return d;
    };
    let root = fs::canonicalize(sysfs).unwrap_or_else(|_| sysfs.to_path_buf());
    d.kind = device(&root, &real, 0, &mut d);
    d
}

/// Decides one device, following partitions up to the whole disk and `slaves` down to the
/// disks under device-mapper and md. Fills the reason and the bus as it goes.
fn device(root: &Path, real: &Path, depth: u8, d: &mut Detection) -> DriveKind {
    let name = |p: &Path| {
        p.file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
    };
    let whole = if real.join("partition").exists() {
        d.chain.push(name(real));
        real.parent().unwrap_or(real)
    } else {
        real
    };
    let disk = name(whole);
    d.chain.push(disk.clone());

    if let Ok(entries) = fs::read_dir(whole.join("slaves")) {
        let slaves: Vec<_> = entries.flatten().collect();
        if !slaves.is_empty() {
            if depth >= MAX_DEPTH || slaves.len() > MAX_SLAVES {
                d.reason = format!("{disk} sits on too many or too deep layers");
                return DriveKind::Unknown;
            }
            d.raw(&format!("slaves of {disk}"), slaves.len().to_string());
            let kinds: Vec<_> = slaves
                .iter()
                .map(|s| match fs::canonicalize(s.path()) {
                    Ok(slave) => device(root, &slave, depth + 1, d),
                    Err(_) => {
                        d.reason = format!("cannot follow a slave of {disk}");
                        DriveKind::Unknown
                    }
                })
                .collect();
            let kind = combine(kinds);
            d.reason = format!(
                "{disk} is built on {} device(s): {}",
                slaves.len(),
                kind.name()
            );
            return kind;
        }
    }

    let removable = read(&whole.join("removable"));
    let rotational = read(&whole.join("queue/rotational"));
    if let Some(v) = &removable {
        d.raw(&format!("removable of {disk}"), v.as_str());
    }
    if let Some(v) = &rotational {
        d.raw(&format!("rotational of {disk}"), v.as_str());
    }

    // The part of the path under the sysfs root names the buses between the disk and the CPU.
    let rel = whole.strip_prefix(root).unwrap_or(whole);
    let parts: Vec<String> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    let digits_after = |s: &str, prefix: &str| {
        s.strip_prefix(prefix)
            .is_some_and(|r| r.chars().next().is_some_and(|c| c.is_ascii_digit()))
    };
    let bus = if parts.iter().any(|s| digits_after(s, "usb")) {
        Some("usb")
    } else if parts
        .iter()
        .any(|s| s == "mmc_host" || digits_after(s, "mmc"))
    {
        Some("mmc")
    } else if parts.iter().any(|s| s == "nvme" || digits_after(s, "nvme")) {
        Some("nvme")
    } else if parts.iter().any(|s| digits_after(s, "virtio")) {
        Some("virtio")
    } else if parts.iter().any(|s| digits_after(s, "ata")) {
        Some("ata")
    } else {
        None
    };
    d.bus = bus.map(str::to_owned);

    if removable.as_deref() == Some("1") {
        d.reason = format!("{disk} is removable");
        return DriveKind::Unknown;
    }
    if matches!(bus, Some("usb" | "mmc")) {
        d.reason = format!(
            "{disk} is behind {}: the bus is not the medium",
            bus.unwrap_or_default()
        );
        return DriveKind::Unknown;
    }
    match rotational.as_deref() {
        Some("1") => {
            d.reason = format!("{disk} is rotational: spinning disk");
            DriveKind::Hdd
        }
        Some("0") => {
            d.reason = format!("{disk} is not rotational: solid state");
            DriveKind::Ssd
        }
        Some(other) => {
            d.reason = format!("{disk} has an unexpected rotational value {other:?}");
            DriveKind::Unknown
        }
        None => {
            d.reason = format!("{disk} has no readable queue/rotational");
            DriveKind::Unknown
        }
    }
}

const IOPRIO_WHO_PROCESS: libc::c_long = 1;
const IOPRIO_CLASS_BE: libc::c_long = 2;
const IOPRIO_CLASS_SHIFT: u32 = 13;

/// Low: nice 10 and the lowest best-effort I/O level (7), which still gets a turn: the idle
/// class could starve the operation behind a busy disk. Normal: nice 0 and the default I/O
/// priority.
pub(crate) fn set_priority(priority: Priority) -> io::Result<()> {
    let (nice, ioprio) = match priority {
        Priority::Low => (10, (IOPRIO_CLASS_BE << IOPRIO_CLASS_SHIFT) | 7),
        Priority::Normal => (0, 0),
    };
    // SAFETY: ioprio_set takes three plain integers (who = 0 is the calling thread) and
    // reads no memory.
    let io_result = unsafe {
        libc::syscall(
            libc::SYS_ioprio_set,
            IOPRIO_WHO_PROCESS,
            0 as libc::c_long,
            ioprio,
        )
    };
    let io_err = (io_result == -1).then(io::Error::last_os_error);
    // SAFETY: setpriority takes plain integers; on Linux who = 0 with PRIO_PROCESS is the
    // calling thread only, not the whole process.
    let nice_result = unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, nice) };
    let nice_err = (nice_result == -1).then(io::Error::last_os_error);
    match nice_err.or(io_err) {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    /// A fake sysfs: `devices/<rel>` directories, `dev/block/MAJ:MIN` symlinks like the real one.
    struct Fake(TempDir);

    impl Fake {
        fn new() -> Fake {
            let t = tempfile::tempdir().unwrap();
            fs::create_dir_all(t.path().join("dev/block")).unwrap();
            Fake(t)
        }
        fn root(&self) -> &Path {
            self.0.path()
        }
        /// A whole disk (or dm/md device) with its flags.
        fn disk(&self, rel: &str, rotational: Option<&str>, removable: Option<&str>) {
            let dir = self.root().join("devices").join(rel);
            fs::create_dir_all(dir.join("queue")).unwrap();
            if let Some(v) = rotational {
                fs::write(dir.join("queue/rotational"), v).unwrap();
            }
            if let Some(v) = removable {
                fs::write(dir.join("removable"), v).unwrap();
            }
        }
        /// A partition directory inside the disk at `rel`.
        fn part(&self, disk_rel: &str, name: &str) -> String {
            let rel = format!("{disk_rel}/{name}");
            let dir = self.root().join("devices").join(&rel);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("partition"), "1\n").unwrap();
            rel
        }
        fn link(&self, major_minor: &str, rel: &str) {
            symlink(
                self.root().join("devices").join(rel),
                self.root().join("dev/block").join(major_minor),
            )
            .unwrap();
        }
        fn slave(&self, dm_rel: &str, name: &str, target_rel: &str) {
            let dir = self.root().join("devices").join(dm_rel).join("slaves");
            fs::create_dir_all(&dir).unwrap();
            symlink(self.root().join("devices").join(target_rel), dir.join(name)).unwrap();
        }
        fn run(&self, mountinfo: &str, major: u32, minor: u32) -> Detection {
            resolve(self.root(), mountinfo, major, minor)
        }
    }

    const SATA: &str = "pci0000:00/0000:00:1f.2/ata1/host0/target0:0:0/0:0:0:0/block";
    const EXT4: &str = "30 1 8:1 / / rw - ext4 /dev/sda1 rw\n";

    #[test]
    fn sata_partition_follows_the_whole_disk_flag() {
        for (flag, kind) in [("0\n", DriveKind::Ssd), ("1\n", DriveKind::Hdd)] {
            let f = Fake::new();
            let disk = format!("{SATA}/sda");
            f.disk(&disk, Some(flag), Some("0\n"));
            let part = f.part(&disk, "sda1");
            f.link("8:1", &part);
            let d = f.run(EXT4, 8, 1);
            assert_eq!(d.kind, kind, "{d}");
            assert_eq!(d.bus.as_deref(), Some("ata"));
            assert_eq!(d.chain, ["sda1", "sda"]);
        }
    }

    #[test]
    fn device_mapper_over_a_partition_uses_the_disk_underneath() {
        // The dm device claims non-rotational; the disk under it spins and must win.
        let f = Fake::new();
        let disk = format!("{SATA}/sda");
        f.disk(&disk, Some("1"), Some("0"));
        let part = f.part(&disk, "sda2");
        f.disk("virtual/block/dm-0", Some("0"), Some("0"));
        f.slave("virtual/block/dm-0", "sda2", &part);
        f.link("253:0", "virtual/block/dm-0");
        let d = f.run("", 253, 0);
        assert_eq!(d.kind, DriveKind::Hdd, "{d}");
        assert_eq!(d.chain, ["dm-0", "sda2", "sda"]);
    }

    #[test]
    fn several_slaves_combine_like_paths_do() {
        let build = |a: &str, b: Option<&str>| {
            let f = Fake::new();
            f.disk(&format!("{SATA}/sda"), Some(a), Some("0"));
            f.disk(&format!("{SATA}/sdb"), b, Some("0"));
            f.disk("virtual/block/md0", Some("0"), None);
            f.slave("virtual/block/md0", "sda", &format!("{SATA}/sda"));
            f.slave("virtual/block/md0", "sdb", &format!("{SATA}/sdb"));
            f.link("9:0", "virtual/block/md0");
            f.run("", 9, 0).kind
        };
        assert_eq!(build("0", Some("0")), DriveKind::Ssd);
        assert_eq!(build("0", Some("1")), DriveKind::Hdd);
        assert_eq!(build("0", None), DriveKind::Unknown);
        assert_eq!(build("weird", Some("1")), DriveKind::Hdd);
    }

    #[test]
    fn nvme_partition() {
        let f = Fake::new();
        let disk = "pci0000:00/0000:00:1d.0/0000:3d:00.0/nvme/nvme0/nvme0n1";
        f.disk(disk, Some("0"), Some("0"));
        let part = f.part(disk, "nvme0n1p2");
        f.link("259:2", &part);
        let d = f.run("", 259, 2);
        assert_eq!(d.kind, DriveKind::Ssd, "{d}");
        assert_eq!(d.bus.as_deref(), Some("nvme"));
    }

    #[test]
    fn removable_is_unknown_whatever_rotational_says() {
        for rot in ["0", "1"] {
            let f = Fake::new();
            let disk = format!("{SATA}/sdb");
            f.disk(&disk, Some(rot), Some("1\n"));
            f.link("8:16", &disk);
            let d = f.run("", 8, 16);
            assert_eq!(d.kind, DriveKind::Unknown, "{d}");
        }
    }

    #[test]
    fn usb_and_mmc_ancestry_is_unknown() {
        let usb = "pci0000:00/0000:00:14.0/usb2/2-1/2-1:1.0/host6/target6:0:0/6:0:0:0/block/sdb";
        let mmc = "pci0000:00/0000:00:1c.0/0000:02:00.0/mmc_host/mmc0/mmc0:0001/block/mmcblk0";
        for (path, bus) in [(usb, "usb"), (mmc, "mmc")] {
            for rot in ["0", "1"] {
                let f = Fake::new();
                f.disk(path, Some(rot), Some("0"));
                let part = f.part(path, "p1");
                f.link("8:17", &part);
                let d = f.run("", 8, 17);
                assert_eq!(d.kind, DriveKind::Unknown, "{d}");
                assert_eq!(d.bus.as_deref(), Some(bus));
            }
        }
    }

    #[test]
    fn a_tempdir_called_usb_does_not_count_as_ancestry() {
        // Only the part of the path under the sysfs root is inspected.
        let t = tempfile::Builder::new().prefix("usb0-").tempdir().unwrap();
        let f = Fake(t);
        fs::create_dir_all(f.root().join("dev/block")).unwrap();
        let disk = format!("{SATA}/sda");
        f.disk(&disk, Some("0"), Some("0"));
        f.link("8:0", &disk);
        assert_eq!(f.run("", 8, 0).kind, DriveKind::Ssd);
    }

    #[test]
    fn opaque_file_systems_are_unknown_even_with_a_sysfs_entry() {
        for fstype in [
            "btrfs",
            "zfs",
            "overlay",
            "fuseblk",
            "fuse.sshfs",
            "nfs",
            "nfs4",
        ] {
            let f = Fake::new();
            let disk = format!("{SATA}/sda");
            f.disk(&disk, Some("0"), Some("0"));
            f.link("0:45", &disk);
            let info = format!(
                "100 29 0:45 /@home /home rw,relatime shared:1 master:2 - {fstype} /dev/sda2 rw\n"
            );
            let d = f.run(&info, 0, 45);
            assert_eq!(d.kind, DriveKind::Unknown, "{fstype}: {d}");
            assert!(d.raw.contains(&("fstype".into(), fstype.into())));
        }
        // An ordinary file system on the same device does not block detection.
        let f = Fake::new();
        let disk = format!("{SATA}/sda");
        f.disk(&disk, Some("0"), Some("0"));
        f.link("0:45", &disk);
        let info = "100 29 0:45 / /data rw - xfs /dev/sda2 rw\n";
        assert_eq!(f.run(info, 0, 45).kind, DriveKind::Ssd);
    }

    #[test]
    fn missing_files_and_weird_values_are_unknown() {
        let f = Fake::new();
        // No such device at all.
        assert_eq!(f.run("", 8, 99).kind, DriveKind::Unknown);

        let disk = format!("{SATA}/sda");
        // The disk exists but has no queue/rotational.
        f.disk(&disk, None, Some("0"));
        f.link("8:0", &disk);
        assert_eq!(f.run("", 8, 0).kind, DriveKind::Unknown);

        for (i, value) in ["2", "", "yes", "-1", "0 1"].into_iter().enumerate() {
            let f = Fake::new();
            let disk = format!("{SATA}/sdc");
            f.disk(&disk, Some(value), Some("0"));
            f.link("8:32", &disk);
            let d = f.run("", 8, 32);
            assert_eq!(d.kind, DriveKind::Unknown, "case {i} {value:?}: {d}");
        }
    }

    #[test]
    fn slaves_nested_too_deep_are_unknown() {
        let f = Fake::new();
        f.disk(&format!("{SATA}/sda"), Some("1"), Some("0"));
        let mut below = format!("{SATA}/sda");
        for i in 0..5 {
            let name = format!("virtual/block/dm-{i}");
            f.disk(&name, Some("0"), None);
            f.slave(&name, "next", &below);
            below = name;
        }
        f.link("253:9", &below);
        assert_eq!(f.run("", 253, 9).kind, DriveKind::Unknown);
    }

    #[test]
    fn mountinfo_lookup() {
        let info = "\
22 28 0:21 / /sys rw,nosuid shared:7 - sysfs sysfs rw
36 35 98:0 /mnt1 /mnt2 rw,noatime master:1 - ext3 /dev/root rw,errors=continue
40 1 259:2 / / rw,relatime - ext4 /dev/nvme0n1p2 rw
";
        assert_eq!(fstype_of(info, 98, 0), Some("ext3"));
        assert_eq!(fstype_of(info, 259, 2), Some("ext4"));
        assert_eq!(fstype_of(info, 8, 1), None);
        assert_eq!(fstype_of("garbage", 8, 1), None);
        assert_eq!(fstype_of("a b 8:1 c", 8, 1), None);
    }

    #[test]
    fn dev_t_decoding_matches_glibc() {
        assert_eq!((major(0x801), minor(0x801)), (8, 1));
        assert_eq!((major(0x10302), minor(0x10302)), (259, 2));
        assert_eq!((major(0x0010_0823), minor(0x0010_0823)), (8, 0x123));
        assert_eq!((major(0), minor(0)), (0, 0));
    }
}
