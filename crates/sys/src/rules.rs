//! Decision rules that do not touch the OS, so they are tested on every platform.
#![cfg_attr(not(windows), allow(dead_code))]

use crate::DriveKind;

/// Combines the kinds of several drives (or several paths) into one: any `Hdd` wins, then
/// any `Unknown`, and only all-`Ssd` is `Ssd`. Nothing at all is `Unknown`.
pub fn combine(kinds: impl IntoIterator<Item = DriveKind>) -> DriveKind {
    let mut any = false;
    let mut unknown = false;
    for kind in kinds {
        any = true;
        match kind {
            DriveKind::Hdd => return DriveKind::Hdd,
            DriveKind::Unknown => unknown = true,
            DriveKind::Ssd => {}
        }
    }
    if !any || unknown {
        DriveKind::Unknown
    } else {
        DriveKind::Ssd
    }
}

/// Windows `STORAGE_BUS_TYPE` values, by name.
pub(crate) fn bus_name(bus: u32) -> &'static str {
    match bus {
        0 => "unknown",
        1 => "scsi",
        2 => "atapi",
        3 => "ata",
        4 => "1394",
        5 => "ssa",
        6 => "fibre",
        7 => "usb",
        8 => "raid",
        9 => "iscsi",
        0xA => "sas",
        0xB => "sata",
        0xC => "sd",
        0xD => "mmc",
        0xE => "virtual",
        0xF => "file-backed virtual",
        0x10 => "storage spaces",
        0x11 => "nvme",
        0x12 => "scm",
        0x13 => "ufs",
        _ => "other",
    }
}

/// Buses where the seek-penalty flag describes the bridge, the controller or a pool, not
/// the medium: USB, RAID, SD, MMC, virtual disks and Storage Spaces.
pub(crate) fn bus_hides_medium(bus: u32) -> bool {
    matches!(bus, 7 | 8 | 0xC | 0xD | 0xE | 0xF | 0x10)
}

/// One disk (or volume) from its seek-penalty flag and bus type. A failed query is `None`
/// and is never taken for a spinning disk.
pub(crate) fn judge(penalty: Option<bool>, bus: Option<u32>) -> (DriveKind, String) {
    if let Some(b) = bus
        && bus_hides_medium(b)
    {
        return (
            DriveKind::Unknown,
            format!(
                "bus {} does not tell the medium, so it stays unknown",
                bus_name(b)
            ),
        );
    }
    match penalty {
        Some(true) => (
            DriveKind::Hdd,
            "seek penalty reported: spinning disk".into(),
        ),
        Some(false) => (DriveKind::Ssd, "no seek penalty: solid state".into()),
        None => (
            DriveKind::Unknown,
            "the seek-penalty query failed or was denied".into(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use DriveKind::{Hdd, Ssd, Unknown};

    #[test]
    fn combine_rule() {
        assert_eq!(combine([Ssd, Ssd]), Ssd);
        assert_eq!(combine([Ssd, Hdd]), Hdd);
        assert_eq!(combine([Unknown, Hdd]), Hdd);
        assert_eq!(combine([Ssd, Unknown]), Unknown);
        assert_eq!(combine([Unknown, Unknown]), Unknown);
        assert_eq!(combine([Hdd]), Hdd);
        assert_eq!(combine([Ssd]), Ssd);
        assert_eq!(combine([]), Unknown);
    }

    #[test]
    fn seek_penalty_decides_on_internal_buses() {
        for bus in [Some(3), Some(0xB), Some(0x11), Some(1), Some(0xA), None] {
            assert_eq!(judge(Some(true), bus).0, Hdd, "{bus:?}");
            assert_eq!(judge(Some(false), bus).0, Ssd, "{bus:?}");
            assert_eq!(judge(None, bus).0, Unknown, "{bus:?}");
        }
    }

    #[test]
    fn bridging_buses_are_never_hdd_or_ssd() {
        for bus in [7, 8, 0xC, 0xD, 0xE, 0xF, 0x10] {
            for penalty in [Some(true), Some(false), None] {
                assert_eq!(
                    judge(penalty, Some(bus)).0,
                    Unknown,
                    "bus {bus} penalty {penalty:?}"
                );
            }
        }
    }

    #[test]
    fn bus_names() {
        assert_eq!(bus_name(7), "usb");
        assert_eq!(bus_name(0x11), "nvme");
        assert_eq!(bus_name(0x99), "other");
    }
}
