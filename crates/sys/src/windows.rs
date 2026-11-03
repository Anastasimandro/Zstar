//! Windows: path -> volume -> storage queries, with zero desired access on the handles so
//! that no administrator rights are needed (to be confirmed on real hardware, see ADR 0009).
//!
//! 1. `IOCTL_STORAGE_QUERY_PROPERTY` (seek penalty, bus type) straight on the volume handle.
//! 2. If that fails: `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS` to find the physical disks, then
//!    the same query on each `\\.\PhysicalDriveN` (marked `fallback`).
//!
//! Any failure is `Unknown`, never `Hdd`.

use std::cell::Cell;
use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::ptr;

use windows_sys::Win32::Storage::FileSystem::{
    GetDriveTypeW, GetVolumeNameForVolumeMountPointW, GetVolumePathNameW,
    IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::IOCTL_STORAGE_QUERY_PROPERTY;
use windows_sys::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN, THREAD_MODE_BACKGROUND_END,
};

use crate::rules::{bus_name, combine, judge};
use crate::{Detection, DriveKind, Priority, cached, existing_ancestor};

const DRIVE_REMOTE: u32 = 4;
const FILE_SHARE_ALL: u32 = 7; // read | write | delete
/// `STORAGE_PROPERTY_ID` values.
const STORAGE_DEVICE_PROPERTY: u32 = 0;
const STORAGE_DEVICE_SEEK_PENALTY_PROPERTY: u32 = 7;

pub(crate) fn detect(path: &Path) -> Detection {
    let existing = existing_ancestor(path);
    let p = plain(std::fs::canonicalize(&existing).unwrap_or(existing));
    let text = p.to_string_lossy();
    if text.starts_with(r"\\?\UNC\")
        || (text.starts_with(r"\\") && !text.starts_with(r"\\?\") && !text.starts_with(r"\\.\"))
    {
        return Detection::unknown("network path (SMB): the medium is not visible from here");
    }
    let Some(root) = volume_root(&p) else {
        return Detection::unknown("GetVolumePathName failed");
    };
    // SAFETY: `root` is NUL-terminated and outlives the call.
    if unsafe { GetDriveTypeW(root.as_ptr()) } == DRIVE_REMOTE {
        return Detection::unknown("network volume: the medium is not visible from here");
    }
    let Some(volume) = volume_guid(&root) else {
        return Detection::unknown("GetVolumeNameForVolumeMountPoint failed");
    };
    cached(volume.clone(), || query_volume(&volume))
}

/// `\\?\C:\x` -> `C:\x`, which the volume functions accept.
fn plain(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => p,
    }
}

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(Some(0)).collect()
}

fn until_nul(buf: &[u16]) -> &[u16] {
    &buf[..buf.iter().position(|&c| c == 0).unwrap_or(buf.len())]
}

/// Mount point or drive root holding `path`, NUL-terminated.
fn volume_root(path: &Path) -> Option<Vec<u16>> {
    let w = wide(path.as_os_str());
    let mut buf = vec![0u16; w.len().max(261)];
    // SAFETY: `w` is NUL-terminated; `buf` is writable for `buf.len()` u16 units.
    let ok = unsafe { GetVolumePathNameW(w.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    (ok != 0).then(|| {
        let mut v = until_nul(&buf).to_vec();
        v.push(0);
        v
    })
}

/// `\\?\Volume{GUID}\` for a mount point or drive root; it also names the device in the cache.
fn volume_guid(root: &[u16]) -> Option<String> {
    let mut buf = [0u16; 261];
    // SAFETY: `root` is NUL-terminated; `buf` is writable for `buf.len()` u16 units.
    let ok = unsafe {
        GetVolumeNameForVolumeMountPointW(root.as_ptr(), buf.as_mut_ptr(), buf.len() as u32)
    };
    (ok != 0).then(|| {
        OsString::from_wide(until_nul(&buf))
            .to_string_lossy()
            .into_owned()
    })
}

/// Opens a device with no access rights: enough for the queries, and no admin needed.
fn open_zero_access(path: &str) -> io::Result<File> {
    OpenOptions::new()
        .access_mode(0)
        .share_mode(FILE_SHARE_ALL)
        .open(path)
}

fn ioctl(device: &File, code: u32, input: &[u8], output: &mut [u8]) -> io::Result<usize> {
    let mut returned = 0u32;
    // SAFETY: the handle belongs to a live `File`; both buffers are valid for their stated
    // lengths for the whole call, and no OVERLAPPED is given, so the call is synchronous.
    let ok = unsafe {
        DeviceIoControl(
            device.as_raw_handle(),
            code,
            input.as_ptr().cast(),
            input.len() as u32,
            output.as_mut_ptr().cast(),
            output.len() as u32,
            &mut returned,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(returned as usize)
    }
}

/// `STORAGE_PROPERTY_QUERY` with `PropertyStandardQuery`: id, query type, one spare byte.
fn property_query(id: u32) -> [u8; 12] {
    let mut q = [0u8; 12];
    q[..4].copy_from_slice(&id.to_ne_bytes());
    q
}

/// `DEVICE_SEEK_PENALTY_DESCRIPTOR`: version, size, then `IncursSeekPenalty` at byte 8.
fn seek_penalty(device: &File) -> io::Result<bool> {
    let mut out = [0u8; 16];
    let n = ioctl(
        device,
        IOCTL_STORAGE_QUERY_PROPERTY,
        &property_query(STORAGE_DEVICE_SEEK_PENALTY_PROPERTY),
        &mut out,
    )?;
    if n < 9 {
        return Err(io::Error::other("short seek-penalty answer"));
    }
    Ok(out[8] != 0)
}

/// `STORAGE_DEVICE_DESCRIPTOR`: `BusType` at byte 28, `RemovableMedia` at byte 10.
fn bus_and_removable(device: &File) -> io::Result<(u32, bool)> {
    let mut out = [0u8; 1024];
    let n = ioctl(
        device,
        IOCTL_STORAGE_QUERY_PROPERTY,
        &property_query(STORAGE_DEVICE_PROPERTY),
        &mut out,
    )?;
    if n < 32 {
        return Err(io::Error::other("short device-descriptor answer"));
    }
    let bus = u32::from_ne_bytes([out[28], out[29], out[30], out[31]]);
    Ok((bus, out[10] != 0))
}

/// What one handle (a volume or a physical disk) answered.
struct Probe {
    penalty: Option<bool>,
    bus: Option<u32>,
}

fn probe(label: &str, device: &File, d: &mut Detection) -> Probe {
    let penalty = match seek_penalty(device) {
        Ok(p) => {
            d.raw(&format!("seek_penalty {label}"), p.to_string());
            Some(p)
        }
        Err(e) => {
            d.raw(&format!("seek_penalty {label}"), format!("error: {e}"));
            None
        }
    };
    let bus = match bus_and_removable(device) {
        Ok((b, removable)) => {
            d.raw(
                &format!("bus_type {label}"),
                format!("{b} ({})", bus_name(b)),
            );
            d.raw(&format!("removable_media {label}"), removable.to_string());
            Some(b)
        }
        Err(e) => {
            d.raw(&format!("bus_type {label}"), format!("error: {e}"));
            None
        }
    };
    Probe { penalty, bus }
}

fn query_volume(volume: &str) -> Detection {
    let mut d = Detection::unknown("");
    d.chain.push(volume.to_owned());
    let device = volume.trim_end_matches('\\');
    let handle = match open_zero_access(device) {
        Ok(h) => h,
        Err(e) => {
            d.reason = format!("cannot open the volume with zero access: {e}");
            return d;
        }
    };

    // 1. Straight on the volume.
    let first = probe("volume", &handle, &mut d);
    if first.penalty.is_some() {
        d.bus = first.bus.map(|b| bus_name(b).to_owned());
        let (kind, why) = judge(first.penalty, first.bus);
        d.kind = kind;
        d.reason = format!("volume query: {why}");
        return d;
    }

    // 2. Fallback: the physical disks under the volume.
    d.fallback = true;
    let mut out = [0u8; 1024];
    let n = match ioctl(&handle, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, &[], &mut out) {
        Ok(n) => n,
        Err(e) => {
            d.reason = format!("volume query failed and disk extents too: {e}");
            return d;
        }
    };
    // VOLUME_DISK_EXTENTS: count (u32), 4 bytes of padding, then 24-byte DISK_EXTENTs that
    // start with the u32 disk number.
    let count = u32::from_ne_bytes([out[0], out[1], out[2], out[3]]) as usize;
    let mut disks = Vec::new();
    for i in 0..count.min(32) {
        let at = 8 + i * 24;
        if at + 4 > n {
            break;
        }
        let number = u32::from_ne_bytes([out[at], out[at + 1], out[at + 2], out[at + 3]]);
        if !disks.contains(&number) {
            disks.push(number);
        }
    }
    if disks.is_empty() {
        d.reason = "volume query failed and no disk extents were reported".into();
        return d;
    }
    let mut kinds = Vec::new();
    let mut reasons = Vec::new();
    for number in disks {
        let label = format!("PhysicalDrive{number}");
        d.chain.push(label.clone());
        let (kind, why) = match open_zero_access(&format!(r"\\.\{label}")) {
            Ok(disk) => {
                let p = probe(&label, &disk, &mut d);
                if d.bus.is_none() {
                    d.bus = p.bus.map(|b| bus_name(b).to_owned());
                }
                judge(p.penalty, p.bus)
            }
            Err(e) => {
                d.raw(&format!("open {label}"), format!("error: {e}"));
                (DriveKind::Unknown, format!("cannot open {label}: {e}"))
            }
        };
        kinds.push(kind);
        reasons.push(format!("{label}: {why}"));
    }
    d.kind = combine(kinds);
    d.reason = format!(
        "fallback through the physical disks: {}",
        reasons.join("; ")
    );
    d
}

thread_local! {
    /// Whether this thread is in background mode, so BEGIN and END are never repeated.
    static BACKGROUND: Cell<bool> = const { Cell::new(false) };
}

/// Low: background processing mode (lowers the thread's CPU priority and, in the words of
/// the docs, its "resource scheduling priorities"). Normal: leaves it again.
pub(crate) fn set_priority(priority: Priority) -> io::Result<()> {
    let want = priority == Priority::Low;
    if BACKGROUND.get() == want {
        return Ok(());
    }
    let mode = if want {
        THREAD_MODE_BACKGROUND_BEGIN
    } else {
        THREAD_MODE_BACKGROUND_END
    };
    // SAFETY: GetCurrentThread returns a pseudo-handle that needs no closing, and
    // SetThreadPriority takes it with a plain integer; background mode only works on the
    // calling thread, which is the one used here.
    let ok = unsafe { SetThreadPriority(GetCurrentThread(), mode) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    BACKGROUND.set(want);
    Ok(())
}
