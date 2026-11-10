//! What the governor and the profile pick want to know about the machine (ADR 0012, step 4).
//!
//! | probe | Linux | Windows | macOS |
//! | --- | --- | --- | --- |
//! | [`memory_pressure`] | `/proc/pressure/memory` | - | - |
//! | [`on_battery`] | `/sys/class/power_supply` | `GetSystemPowerStatus` | - |
//! | [`user_idle`] | - | `GetLastInputInfo` | - |
//! | [`process_user_cpu`] | `getrusage` | `GetProcessTimes` | `getrusage` |
//!
//! A `-` is `None`, never a guess: Linux has no way to ask how long the user has been idle
//! that works on every desktop, headless box and Wayland session without a library, and
//! nothing for the others was measured. Whoever asks treats `None` as "no signal". The parsing
//! is plain code that is built and tested on every OS; only the calls are per OS.

use std::time::Duration;

/// Stalls caused by lack of memory, 0 to 1: the share of the last 10 seconds in which some task
/// waited for memory (Linux pressure stall information). `None` where the OS does not say.
pub fn memory_pressure() -> Option<f64> {
    #[cfg(target_os = "linux")]
    return std::fs::read_to_string("/proc/pressure/memory")
        .ok()
        .and_then(|text| parse_psi(&text));
    #[cfg(not(target_os = "linux"))]
    None
}

/// `Some(true)` when the machine runs on its battery, `Some(false)` when it has mains power,
/// `None` when the OS does not say (a desktop has no battery to ask).
pub fn on_battery() -> Option<bool> {
    #[cfg(target_os = "linux")]
    return battery_state(std::path::Path::new("/sys/class/power_supply"));
    #[cfg(windows)]
    return windows_battery();
    #[cfg(not(any(target_os = "linux", windows)))]
    None
}

/// How long ago the user last touched the keyboard or the mouse. `None` where the OS does not
/// say.
pub fn user_idle() -> Option<Duration> {
    #[cfg(windows)]
    return windows_idle();
    #[cfg(not(windows))]
    None
}

/// User-mode CPU time this whole process has used so far. It only grows; the difference between
/// two readings is the CPU the process got in between.
pub fn process_user_cpu() -> Option<Duration> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    return unix_user_cpu();
    #[cfg(windows)]
    return windows_user_cpu();
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    None
}

/// The `some avg10` figure of a pressure file, as a 0 to 1 share. The line looks like
/// `some avg10=0.50 avg60=0.31 avg300=0.12 total=123456`.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn parse_psi(text: &str) -> Option<f64> {
    let line = text.lines().find(|l| l.starts_with("some "))?;
    let value = line
        .split_whitespace()
        .find_map(|field| field.strip_prefix("avg10="))?;
    let percent: f64 = value.parse().ok()?;
    percent
        .is_finite()
        .then(|| (percent / 100.0).clamp(0.0, 1.0))
}

/// Mains power wins over a battery that reports discharging (a docked laptop in transition);
/// without a battery or mains entry there is nothing to say.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn battery_state(root: &std::path::Path) -> Option<bool> {
    let (mut mains, mut discharging, mut battery) = (false, false, false);
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let read = |name: &str| {
            std::fs::read_to_string(entry.path().join(name))
                .map(|s| s.trim().to_owned())
                .unwrap_or_default()
        };
        match read("type").as_str() {
            "Mains" | "USB" => mains |= read("online") == "1",
            "Battery" => {
                battery = true;
                discharging |= read("status") == "Discharging";
            }
            _ => {}
        }
    }
    if mains {
        Some(false)
    } else if battery {
        Some(discharging)
    } else {
        None
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn unix_user_cpu() -> Option<Duration> {
    // SAFETY: `usage` is a zeroed, writable `rusage`, which is all `getrusage` needs; it reads
    // nothing of ours.
    let usage = unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut usage) != 0 {
            return None;
        }
        usage
    };
    let (secs, micros) = (
        u64::try_from(usage.ru_utime.tv_sec).ok()?,
        u32::try_from(usage.ru_utime.tv_usec).ok()?,
    );
    Some(Duration::new(secs, micros.checked_mul(1000)?))
}

#[cfg(windows)]
fn windows_battery() -> Option<bool> {
    use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};

    let mut status = SYSTEM_POWER_STATUS {
        ACLineStatus: 255,
        BatteryFlag: 0,
        BatteryLifePercent: 0,
        SystemStatusFlag: 0,
        BatteryLifeTime: 0,
        BatteryFullLifeTime: 0,
    };
    // SAFETY: `status` is a valid, writable SYSTEM_POWER_STATUS, which is all the call needs.
    if unsafe { GetSystemPowerStatus(&mut status) } == 0 {
        return None;
    }
    match status.ACLineStatus {
        0 => Some(true),
        1 => Some(false),
        _ => None,
    }
}

#[cfg(windows)]
fn windows_idle() -> Option<Duration> {
    use windows_sys::Win32::System::SystemInformation::GetTickCount64;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    // SAFETY: `info` is a valid, writable LASTINPUTINFO with `cbSize` set, which is all the
    // call needs.
    if unsafe { GetLastInputInfo(&mut info) } == 0 {
        return None;
    }
    // SAFETY: the call takes no arguments and reads nothing of ours.
    let now = unsafe { GetTickCount64() };
    // `dwTime` is a 32-bit tick count that wraps every 49 days, so compare it modulo 2^32.
    Some(Duration::from_millis(u64::from(
        (now as u32).wrapping_sub(info.dwTime),
    )))
}

#[cfg(windows)]
fn windows_user_cpu() -> Option<Duration> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: the pseudo handle of the current process needs no closing, and the four
    // FILETIMEs are valid and writable, which is all the call needs.
    let ok = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    };
    if ok == 0 {
        return None;
    }
    // FILETIME counts 100 ns units.
    let units = (u64::from(user.dwHighDateTime) << 32) | u64::from(user.dwLowDateTime);
    Some(Duration::from_nanos(units.checked_mul(100)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn pressure_is_the_some_avg10_share() {
        let text = "some avg10=12.50 avg60=3.00 avg300=1.00 total=1\nfull avg10=40.00 avg60=0 avg300=0 total=2\n";
        assert_eq!(parse_psi(text), Some(0.125));
        assert_eq!(parse_psi("some avg10=250.00 avg60=0"), Some(1.0));
        assert_eq!(parse_psi("full avg10=40.00"), None);
        assert_eq!(parse_psi("some avg10=nan"), None);
        assert_eq!(parse_psi("some avg60=1.0"), None);
        assert_eq!(parse_psi(""), None);
    }

    fn supply(root: &std::path::Path, name: &str, files: &[(&str, &str)]) {
        let dir = root.join(name);
        fs::create_dir(&dir).unwrap();
        for (file, text) in files {
            fs::write(dir.join(file), format!("{text}\n")).unwrap();
        }
    }

    #[test]
    fn battery_state_reads_power_supplies() {
        let new = || tempfile::tempdir().unwrap();

        let laptop = new();
        supply(
            laptop.path(),
            "BAT0",
            &[("type", "Battery"), ("status", "Discharging")],
        );
        supply(laptop.path(), "AC", &[("type", "Mains"), ("online", "0")]);
        assert_eq!(battery_state(laptop.path()), Some(true));

        let plugged = new();
        supply(
            plugged.path(),
            "BAT0",
            &[("type", "Battery"), ("status", "Charging")],
        );
        supply(plugged.path(), "AC", &[("type", "Mains"), ("online", "1")]);
        assert_eq!(battery_state(plugged.path()), Some(false));

        // Mains wins over a battery that still says it discharges.
        let docking = new();
        supply(
            docking.path(),
            "BAT0",
            &[("type", "Battery"), ("status", "Discharging")],
        );
        supply(docking.path(), "AC", &[("type", "Mains"), ("online", "1")]);
        assert_eq!(battery_state(docking.path()), Some(false));

        // A battery with no mains entry: it is what the status says.
        let alone = new();
        supply(
            alone.path(),
            "BAT0",
            &[("type", "Battery"), ("status", "Full")],
        );
        assert_eq!(battery_state(alone.path()), Some(false));

        // A desktop: nothing to ask. A missing directory: the same.
        let desktop = new();
        supply(
            desktop.path(),
            "hid-1",
            &[("type", "Battery"), ("status", "Discharging")],
        );
        fs::remove_dir_all(desktop.path().join("hid-1")).unwrap();
        assert_eq!(battery_state(desktop.path()), None);
        assert_eq!(battery_state(&desktop.path().join("nothing")), None);
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn process_cpu_only_grows_and_follows_the_work() {
        let before = process_user_cpu().unwrap();
        let start = std::time::Instant::now();
        let mut x = 0u64;
        while start.elapsed() < Duration::from_millis(150) {
            x = std::hint::black_box(x.wrapping_mul(31).wrapping_add(7));
        }
        let after = process_user_cpu().unwrap();
        assert!(after >= before);
        assert!(
            after - before >= Duration::from_millis(60),
            "{before:?} then {after:?}"
        );
    }

    #[test]
    fn probes_never_panic_and_stay_in_range() {
        if let Some(p) = memory_pressure() {
            assert!((0.0..=1.0).contains(&p));
        }
        let _ = on_battery();
        let _ = user_idle();
    }
}
