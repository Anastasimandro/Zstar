//! Speed profiles, drive kinds and the settings they map to.
//!
//! The caller finds out what kind of drive it is working on; core never touches hardware.
//! [`settings_for`] is pure, and every number that trades speed for gentleness is in the table
//! below, so it can be tuned after measuring without reading anything else.

use std::sync::OnceLock;

use crate::control::Priority;

// ---------------------------------------------------------------------------------------
// THE TABLE
//
//   profile    threads                          priority
//   Fast       every CPU                        Normal
//   Balanced   3/4 of the CPUs (at least 1)     Normal
//   Gentle     1/4 of the CPUs, 1 to 2          Low
//   Auto       resolved first: Ssd -> Fast, Hdd and Unknown -> Balanced
//
//   drive      buffer     parallel many-file extraction
//   Ssd        128 KiB    yes
//   Hdd        1 MiB      no
//   Unknown    1 MiB      no   (assumes the worst, so it takes the HDD-safe rules)
//
// The profile decides CPU use and the drive decides I/O behaviour, so the two never mix.
// ---------------------------------------------------------------------------------------

/// Copy buffer on a drive that likes parallel, smaller requests.
const SSD_BUFFER: usize = 128 * 1024;
/// Copy buffer on a drive that wants long sequential requests. A first guess, to be measured.
const HDD_BUFFER: usize = 1 << 20;

/// The CPU rules of an already resolved profile, for `cpus >= 1` CPUs.
pub(crate) fn cpu_rules(profile: Profile, cpus: usize) -> (usize, Priority) {
    let cpus = cpus.max(1);
    match profile {
        Profile::Fast | Profile::Auto => (cpus, Priority::Normal),
        Profile::Balanced => (cpus - cpus / 4, Priority::Normal),
        Profile::Gentle => ((cpus / 4).clamp(1, 2), Priority::Low),
    }
}

/// The I/O rules of a drive kind: (copy buffer in bytes, parallel many-file extraction).
fn io_rules(drive: DriveKind) -> (usize, bool) {
    match drive {
        DriveKind::Ssd => (SSD_BUFFER, true),
        DriveKind::Hdd | DriveKind::Unknown => (HDD_BUFFER, false),
    }
}

/// How hard an operation may work. All but `Auto` can be switched while it runs, through
/// [`Control::set_profile`](crate::Control::set_profile).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Profile {
    /// Every CPU, normal priority.
    Fast,
    /// Most CPUs, normal priority.
    Balanced,
    /// One or two threads at low priority: stays out of the way of the rest of the system.
    Gentle,
    /// Picked from the [`DriveKind`] when the operation starts.
    #[default]
    Auto,
}

impl Profile {
    /// Name as used on the command line.
    pub fn name(self) -> &'static str {
        match self {
            Profile::Fast => "fast",
            Profile::Balanced => "balanced",
            Profile::Gentle => "gentle",
            Profile::Auto => "auto",
        }
    }

    /// Parses a name as returned by [`Profile::name`].
    pub fn from_name(name: &str) -> Option<Profile> {
        [
            Profile::Fast,
            Profile::Balanced,
            Profile::Gentle,
            Profile::Auto,
        ]
        .into_iter()
        .find(|p| p.name() == name)
    }

    /// `Auto` replaced by the profile it stands for on `drive`; any other profile as it is.
    pub fn resolve(self, drive: DriveKind) -> Profile {
        match (self, drive) {
            (Profile::Auto, DriveKind::Ssd) => Profile::Fast,
            (Profile::Auto, _) => Profile::Balanced,
            (profile, _) => profile,
        }
    }
}

/// The kind of drive the data lives on. The caller decides it (from the OS, a measurement or
/// the user); core only acts on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DriveKind {
    /// Solid state: parallel requests are fine.
    Ssd,
    /// Spinning disk: one sequential stream, big buffers.
    Hdd,
    /// Not known: the HDD-safe I/O rules apply.
    #[default]
    Unknown,
}

impl DriveKind {
    /// Name as used on the command line.
    pub fn name(self) -> &'static str {
        match self {
            DriveKind::Ssd => "ssd",
            DriveKind::Hdd => "hdd",
            DriveKind::Unknown => "unknown",
        }
    }

    /// Parses a name as returned by [`DriveKind::name`].
    pub fn from_name(name: &str) -> Option<DriveKind> {
        [DriveKind::Ssd, DriveKind::Hdd, DriveKind::Unknown]
            .into_iter()
            .find(|d| d.name() == name)
    }
}

/// What a profile and a drive kind come to on a given machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// Worker threads to start with (at least 1).
    pub threads: usize,
    /// Priority workers should run at.
    pub priority: Priority,
    /// Size in bytes of the buffer files are copied through.
    pub buffer: usize,
    /// Whether many files may be extracted at the same time.
    pub parallel_files: bool,
}

/// Maps a profile and a drive kind to concrete settings on a machine with `cpus` CPUs.
pub fn settings_for(profile: Profile, drive: DriveKind, cpus: usize) -> Settings {
    let (threads, priority) = cpu_rules(profile.resolve(drive), cpus);
    let (buffer, parallel_files) = io_rules(drive);
    Settings {
        threads,
        priority,
        buffer,
        parallel_files,
    }
}

/// CPUs available to this process, asked once.
pub(crate) fn cpus() -> usize {
    static CPUS: OnceLock<usize> = OnceLock::new();
    *CPUS.get_or_init(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use DriveKind::{Hdd, Ssd, Unknown};
    use Priority::{Low, Normal};
    use Profile::{Auto, Balanced, Fast, Gentle};

    fn set(threads: usize, priority: Priority, buffer: usize, parallel_files: bool) -> Settings {
        Settings {
            threads,
            priority,
            buffer,
            parallel_files,
        }
    }

    #[test]
    fn each_profile_maps_to_its_row() {
        assert_eq!(settings_for(Fast, Ssd, 8), set(8, Normal, SSD_BUFFER, true));
        assert_eq!(
            settings_for(Fast, Hdd, 8),
            set(8, Normal, HDD_BUFFER, false)
        );
        assert_eq!(settings_for(Balanced, Ssd, 8).threads, 6);
        assert_eq!(settings_for(Gentle, Ssd, 8), set(2, Low, SSD_BUFFER, true));
        assert_eq!(settings_for(Gentle, Hdd, 8), set(2, Low, HDD_BUFFER, false));
    }

    #[test]
    fn auto_follows_the_drive_and_unknown_is_hdd_safe() {
        assert_eq!(settings_for(Auto, Ssd, 8), settings_for(Fast, Ssd, 8));
        assert_eq!(settings_for(Auto, Hdd, 8), settings_for(Balanced, Hdd, 8));
        assert_eq!(
            settings_for(Auto, Unknown, 8),
            settings_for(Balanced, Hdd, 8)
        );
        // An explicit profile keeps its CPU rules whatever the drive is.
        assert_eq!(
            settings_for(Fast, Unknown, 8),
            set(8, Normal, HDD_BUFFER, false)
        );
        assert_eq!(Auto.resolve(Ssd), Fast);
        assert_eq!(Gentle.resolve(Ssd), Gentle);
    }

    #[test]
    fn thread_counts_stay_in_range_on_any_machine() {
        for cpus in [0, 1, 2, 3, 4, 16, 128] {
            for profile in [Fast, Balanced, Gentle, Auto] {
                let s = settings_for(profile, Unknown, cpus);
                assert!(
                    (1..=cpus.max(1)).contains(&s.threads),
                    "{profile:?} on {cpus}"
                );
            }
            assert!(settings_for(Gentle, Ssd, cpus).threads <= 2);
        }
        // Two CPUs keep both for Balanced, so the default does not slow small machines down.
        assert_eq!(settings_for(Balanced, Unknown, 2).threads, 2);
        assert_eq!(settings_for(Gentle, Unknown, 2).threads, 1);
    }

    #[test]
    fn names_round_trip() {
        for p in [Fast, Balanced, Gentle, Auto] {
            assert_eq!(Profile::from_name(p.name()), Some(p));
        }
        for d in [Ssd, Hdd, Unknown] {
            assert_eq!(DriveKind::from_name(d.name()), Some(d));
        }
        assert_eq!(Profile::from_name("turbo"), None);
        assert_eq!(DriveKind::from_name("nvme"), None);
    }
}
