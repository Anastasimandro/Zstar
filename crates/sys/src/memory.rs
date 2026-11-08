//! Physical memory: how much there is and how much is free.
//!
//! * Linux and Android: `/proc/meminfo`, then the tightest cgroup memory limit above this
//!   process (v2 `memory.max`, v1 `memory.limit_in_bytes`), which a container or an Android
//!   app may have and which `/proc/meminfo` ignores.
//! * Windows: `GlobalMemoryStatusEx`.
//! * macOS: `sysctl` (`hw.memsize`, and the free, speculative and file-backed page counts).
//!
//! Anything unreadable is `None`, never a guess and never a panic. The parsing and mapping
//! logic is plain code that is built and tested on every OS; only the calls are per OS.

#[cfg(any(target_os = "linux", target_os = "android"))]
use std::fs;
use std::path::Path;

/// Memory of the machine, or of the container or app it runs in when that is smaller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ram {
    /// Physical memory in bytes.
    pub total: u64,
    /// Memory that can be given to a program without swapping, when the OS says so.
    pub available: Option<u64>,
}

/// The memory of this machine, or `None` when it cannot be read.
pub fn ram() -> Option<Ram> {
    platform()
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn platform() -> Option<Ram> {
    let meminfo = fs::read_to_string("/proc/meminfo").ok()?;
    let (total, available) = parse_meminfo(&meminfo)?;
    let limit = fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|own| cgroup_limit(Path::new("/sys/fs/cgroup"), &own));
    Some(within_cgroup(total, available, limit))
}

#[cfg(windows)]
fn platform() -> Option<Ram> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    let mut status = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        dwMemoryLoad: 0,
        ullTotalPhys: 0,
        ullAvailPhys: 0,
        ullTotalPageFile: 0,
        ullAvailPageFile: 0,
        ullTotalVirtual: 0,
        ullAvailVirtual: 0,
        ullAvailExtendedVirtual: 0,
    };
    // SAFETY: `status` is a valid, writable MEMORYSTATUSEX and its `dwLength` is set to the
    // structure size, which is all the call requires.
    let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
    if ok == 0 {
        return None;
    }
    from_counts(status.ullTotalPhys, Some(status.ullAvailPhys))
}

#[cfg(target_os = "macos")]
fn platform() -> Option<Ram> {
    let total = sysctl_number(c"hw.memsize")?;
    // Free pages alone understate what is available: speculative and file-backed pages are
    // given back on demand. Any of the counts missing means "available unknown".
    let available = (|| {
        let page = sysctl_number(c"hw.pagesize")?;
        let pages = sysctl_number(c"vm.page_free_count")?
            .checked_add(sysctl_number(c"vm.page_speculative_count")?)?
            .checked_add(sysctl_number(c"vm.page_pageable_external_count")?)?;
        pages.checked_mul(page)
    })();
    from_counts(total, available)
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    windows,
    target_os = "macos"
)))]
fn platform() -> Option<Ram> {
    None
}

/// A 4 or 8 byte unsigned `sysctl` value.
#[cfg(target_os = "macos")]
fn sysctl_number(name: &std::ffi::CStr) -> Option<u64> {
    let mut buf = [0u8; 8];
    let mut len: libc::size_t = buf.len();
    // SAFETY: `name` is NUL-terminated; `buf` is writable for `len` bytes and `len` holds its
    // size; no new value is passed (null pointer, length 0), so the call only reads.
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    match len {
        4 => Some(u64::from(u32::from_ne_bytes(buf[..4].try_into().ok()?))),
        8 => Some(u64::from_ne_bytes(buf)),
        _ => None,
    }
}

/// The counts an OS reported, checked: no memory at all is a failed query, and what is
/// available cannot exceed what exists.
#[cfg_attr(not(any(windows, target_os = "macos", test)), allow(dead_code))]
fn from_counts(total: u64, available: Option<u64>) -> Option<Ram> {
    (total > 0).then(|| Ram {
        total,
        available: available.map(|a| a.min(total)),
    })
}

/// `MemTotal` and `MemAvailable` of `/proc/meminfo` in bytes. Kernels before 3.14 have no
/// `MemAvailable`; `MemFree` stands in then (it understates, which is the safe side).
#[cfg_attr(
    not(any(target_os = "linux", target_os = "android", test)),
    allow(dead_code)
)]
fn parse_meminfo(text: &str) -> Option<(u64, Option<u64>)> {
    let field = |key: &str| -> Option<u64> {
        let rest = text.lines().find_map(|l| l.strip_prefix(key))?;
        let kib: u64 = rest.trim().strip_suffix("kB")?.trim().parse().ok()?;
        kib.checked_mul(1024)
    };
    let total = field("MemTotal:").filter(|&t| t > 0)?;
    let available = field("MemAvailable:").or_else(|| field("MemFree:"));
    Some((total, available))
}

/// A cgroup memory file: a number of bytes, `max` (no limit), or in cgroup v1 a huge number
/// standing for "no limit".
#[cfg_attr(
    not(any(target_os = "linux", target_os = "android", test)),
    allow(dead_code)
)]
fn parse_cgroup_number(text: &str) -> Option<u64> {
    let n: u64 = text.trim().parse().ok()?;
    // v1 reports `PAGE_COUNTER_MAX` rounded to a page (about 2^63) when nothing is set.
    (n < 1 << 60).then_some(n)
}

/// Where a cgroup of this process lives, as `(version, path)` pairs from `/proc/self/cgroup`:
/// `0::/path` is v2, `N:memory:/path` (or a list containing `memory`) is v1.
#[cfg_attr(
    not(any(target_os = "linux", target_os = "android", test)),
    allow(dead_code)
)]
fn parse_cgroup_paths(own: &str) -> Vec<(u8, String)> {
    own.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, ':');
            let (_, controllers, path) = (parts.next()?, parts.next()?, parts.next()?);
            if controllers.is_empty() {
                Some((2, path.to_string()))
            } else if controllers.split(',').any(|c| c == "memory") {
                Some((1, path.to_string()))
            } else {
                None
            }
        })
        .collect()
}

/// The tightest memory limit on the cgroup of this process or any cgroup above it, plus how
/// much of it is in use at the tightest level.
#[cfg_attr(
    not(any(target_os = "linux", target_os = "android", test)),
    allow(dead_code)
)]
fn cgroup_limit(root: &Path, own: &str) -> Option<CgroupLimit> {
    let mut best: Option<CgroupLimit> = None;
    for (version, path) in parse_cgroup_paths(own) {
        let (base, limit_file, usage_file) = if version == 2 {
            (root.to_path_buf(), "memory.max", "memory.current")
        } else {
            (
                root.join("memory"),
                "memory.limit_in_bytes",
                "memory.usage_in_bytes",
            )
        };
        // From the cgroup itself up to the root: a parent's limit binds its children.
        let mut dir = Some(Path::new(&path).to_path_buf());
        while let Some(rel) = dir {
            let at = base.join(rel.strip_prefix("/").unwrap_or(&rel));
            let limit = std::fs::read_to_string(at.join(limit_file))
                .ok()
                .and_then(|t| parse_cgroup_number(&t));
            if let Some(limit) = limit.filter(|&l| best.is_none_or(|b| l < b.limit)) {
                let usage = std::fs::read_to_string(at.join(usage_file))
                    .ok()
                    .and_then(|t| t.trim().parse().ok());
                best = Some(CgroupLimit { limit, usage });
            }
            dir = rel
                .parent()
                .map(Path::to_path_buf)
                .filter(|p| !p.as_os_str().is_empty());
        }
    }
    best
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CgroupLimit {
    limit: u64,
    usage: Option<u64>,
}

/// Machine memory narrowed by a cgroup limit, if there is one smaller than the machine.
#[cfg_attr(
    not(any(target_os = "linux", target_os = "android", test)),
    allow(dead_code)
)]
fn within_cgroup(total: u64, available: Option<u64>, cgroup: Option<CgroupLimit>) -> Ram {
    let Some(c) = cgroup.filter(|c| c.limit < total) else {
        return Ram { total, available };
    };
    // What is left under the limit; without a usage figure the whole limit is the bound.
    let room = c.usage.map_or(c.limit, |u| c.limit.saturating_sub(u));
    Ram {
        total: c.limit,
        available: Some(available.map_or(room, |a| a.min(room))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    #[test]
    fn meminfo_gives_total_and_available() {
        let text =
            "MemTotal:        2048000 kB\nMemFree:          100 kB\nMemAvailable:   1024000 kB\n";
        assert_eq!(
            parse_meminfo(text),
            Some((2_048_000 * 1024, Some(1_024_000 * 1024)))
        );
    }

    #[test]
    fn old_kernels_without_memavailable_use_memfree() {
        let text = "MemTotal: 4096 kB\nMemFree: 1024 kB\n";
        assert_eq!(parse_meminfo(text), Some((4096 * 1024, Some(1024 * 1024))));
        assert_eq!(
            parse_meminfo("MemTotal: 4096 kB\n"),
            Some((4096 * 1024, None))
        );
    }

    #[test]
    fn garbage_meminfo_is_unknown() {
        assert_eq!(parse_meminfo(""), None);
        assert_eq!(parse_meminfo("MemFree: 5 kB\n"), None);
        assert_eq!(parse_meminfo("MemTotal: lots kB\n"), None);
        assert_eq!(parse_meminfo("MemTotal: 0 kB\n"), None);
        assert_eq!(parse_meminfo("MemTotal: 5\n"), None);
        assert_eq!(parse_meminfo(&format!("MemTotal: {} kB\n", u64::MAX)), None);
    }

    #[test]
    fn counts_from_windows_or_macos_are_checked() {
        assert_eq!(from_counts(0, Some(5)), None);
        assert_eq!(
            from_counts(100, Some(40)),
            Some(Ram {
                total: 100,
                available: Some(40)
            })
        );
        assert_eq!(
            from_counts(100, Some(400)),
            Some(Ram {
                total: 100,
                available: Some(100)
            })
        );
        assert_eq!(
            from_counts(100, None),
            Some(Ram {
                total: 100,
                available: None
            })
        );
    }

    #[test]
    fn cgroup_numbers() {
        assert_eq!(parse_cgroup_number("536870912\n"), Some(536_870_912));
        assert_eq!(parse_cgroup_number("max\n"), None);
        assert_eq!(parse_cgroup_number("9223372036854771712\n"), None); // v1 "no limit"
        assert_eq!(parse_cgroup_number(""), None);
    }

    #[test]
    fn cgroup_paths_of_both_versions() {
        let own = "12:cpu,cpuacct:/a\n11:memory:/user/app\n0::/uid_1/pid_2\n";
        assert_eq!(
            parse_cgroup_paths(own),
            vec![
                (1, "/user/app".to_string()),
                (2, "/uid_1/pid_2".to_string())
            ]
        );
        assert!(parse_cgroup_paths("garbage").is_empty());
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn the_tightest_limit_above_the_process_wins_in_cgroup_v2() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(&root.join("app/memory.max"), "1073741824\n"); // 1 GiB on the parent
        write(&root.join("app/memory.current"), "104857600\n");
        write(&root.join("app/job/memory.max"), "max\n"); // none on the child
        write(&root.join("app/job/memory.current"), "52428800\n");
        let got = cgroup_limit(root, "0::/app/job\n");
        assert_eq!(
            got,
            Some(CgroupLimit {
                limit: 1 << 30,
                usage: Some(100 * MIB)
            })
        );
        // A tighter limit on the child binds.
        write(&root.join("app/job/memory.max"), "268435456\n");
        let got = cgroup_limit(root, "0::/app/job\n");
        assert_eq!(
            got,
            Some(CgroupLimit {
                limit: 256 * MIB,
                usage: Some(50 * MIB)
            })
        );
    }

    #[test]
    fn cgroup_v1_and_missing_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(
            &root.join("memory/user/memory.limit_in_bytes"),
            "536870912\n",
        );
        write(&root.join("memory/user/memory.usage_in_bytes"), "1048576\n");
        assert_eq!(
            cgroup_limit(root, "4:memory:/user\n"),
            Some(CgroupLimit {
                limit: 512 * MIB,
                usage: Some(MIB)
            })
        );
        // The root cgroup of v1 says "no limit" with a huge number: no limit.
        write(
            &root.join("memory/memory.limit_in_bytes"),
            "9223372036854771712\n",
        );
        assert_eq!(cgroup_limit(root, "4:memory:/\n"), None);
        // Nothing readable at all.
        assert_eq!(cgroup_limit(&root.join("nowhere"), "0::/x\n"), None);
    }

    #[test]
    fn a_cgroup_limit_narrows_the_machine_only_when_it_is_smaller() {
        let machine = (8 << 30, Some(5 << 30));
        let free = within_cgroup(machine.0, machine.1, None);
        assert_eq!(
            free,
            Ram {
                total: 8 << 30,
                available: Some(5 << 30)
            }
        );
        // A limit bigger than the machine means nothing.
        let big = CgroupLimit {
            limit: 16 << 30,
            usage: Some(0),
        };
        assert_eq!(within_cgroup(machine.0, machine.1, Some(big)), free);
        // 2 GiB limit with 1.5 GiB used: 512 MiB left, whatever the machine has free.
        let tight = CgroupLimit {
            limit: 2 << 30,
            usage: Some(1536 * MIB),
        };
        assert_eq!(
            within_cgroup(machine.0, machine.1, Some(tight)),
            Ram {
                total: 2 << 30,
                available: Some(512 * MIB)
            }
        );
        // Less free on the machine than under the limit: the machine's figure.
        let loose = CgroupLimit {
            limit: 4 << 30,
            usage: Some(0),
        };
        assert_eq!(
            within_cgroup(8 << 30, Some(MIB), Some(loose)),
            Ram {
                total: 4 << 30,
                available: Some(MIB)
            }
        );
        // No usage figure: the limit is the bound. Over-used limits leave nothing.
        let unknown = CgroupLimit {
            limit: 1 << 30,
            usage: None,
        };
        assert_eq!(
            within_cgroup(8 << 30, None, Some(unknown)).available,
            Some(1 << 30)
        );
        let over = CgroupLimit {
            limit: 1 << 30,
            usage: Some(2 << 30),
        };
        assert_eq!(within_cgroup(8 << 30, None, Some(over)).available, Some(0));
    }

    /// The real call, on the OSes that have one: a sane answer, not a particular number.
    #[test]
    #[cfg(any(target_os = "linux", windows, target_os = "macos"))]
    fn the_real_call_answers_something_plausible() {
        let r = ram().expect("this OS can report its memory");
        assert!(r.total >= 64 * MIB, "{r:?}");
        if let Some(a) = r.available {
            assert!(a <= r.total, "{r:?}");
        }
        // Linux, Windows and macOS all know what is free; a None here would mean the
        // parsing broke on a real system.
        assert!(r.available.is_some(), "{r:?}");
    }
}
