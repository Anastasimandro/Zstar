//! OS queries for Zstar: which kind of drive holds a path, real thread priority, a
//! sequential-read hint for source files and a same-device check.
//!
//! This crate is independent of `zstar-core` and is the only place in the workspace that may
//! contain `unsafe`. Detection never spawns a process, never panics and answers `Unknown`
//! whenever it is not sure: `Hdd` is only reported when the OS says "rotational" or "seek
//! penalty" about a plain internal disk.

#![deny(unsafe_op_in_unsafe_fn)]

use std::fmt;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[cfg(target_os = "linux")]
mod linux;
mod rules;
#[cfg(windows)]
mod windows;

pub use rules::combine;

/// The kind of drive a path lives on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriveKind {
    Ssd,
    Hdd,
    Unknown,
}

impl DriveKind {
    pub fn name(self) -> &'static str {
        match self {
            DriveKind::Ssd => "ssd",
            DriveKind::Hdd => "hdd",
            DriveKind::Unknown => "unknown",
        }
    }
}

/// How hard the current thread should yield to the rest of the system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    Normal,
    Low,
}

/// The outcome of a detection together with how it was reached, so a wrong answer on real
/// hardware can be diagnosed from `zstar drive-info`.
#[derive(Debug, Clone)]
pub struct Detection {
    /// The final decision.
    pub kind: DriveKind,
    /// Storage bus, when the OS tells (`nvme`, `ata`, `usb`, ...).
    pub bus: Option<String>,
    /// Devices walked to reach the decision, outermost first.
    pub chain: Vec<String>,
    /// Raw values read from the OS, as `(name, value)`.
    pub raw: Vec<(String, String)>,
    /// One line saying why `kind` is what it is.
    pub reason: String,
    /// The first method failed and a second one decided (Windows: per-disk query).
    pub fallback: bool,
}

impl Detection {
    pub(crate) fn unknown(reason: impl Into<String>) -> Detection {
        Detection {
            kind: DriveKind::Unknown,
            bus: None,
            chain: Vec::new(),
            raw: Vec::new(),
            reason: reason.into(),
            fallback: false,
        }
    }

    pub(crate) fn raw(&mut self, name: &str, value: impl Into<String>) {
        self.raw.push((name.to_owned(), value.into()));
    }
}

impl fmt::Display for Detection {
    /// Plain text, one `key: value` per line.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "class: {}", self.kind.name())?;
        if let Some(bus) = &self.bus {
            writeln!(f, "bus: {bus}")?;
        }
        if !self.chain.is_empty() {
            writeln!(f, "chain: {}", self.chain.join(" > "))?;
        }
        for (k, v) in &self.raw {
            writeln!(f, "raw: {k}={v}")?;
        }
        if self.fallback {
            writeln!(f, "fallback: yes")?;
        }
        write!(f, "reason: {}", self.reason)
    }
}

/// The kind of drive holding `path`. The path need not exist yet (an output file): the
/// closest existing parent is used. Cached per device for the life of the process.
pub fn drive_kind(path: &Path) -> DriveKind {
    detect(path).kind
}

/// Like [`drive_kind`], with the reasoning.
pub fn detect(path: &Path) -> Detection {
    #[cfg(target_os = "linux")]
    return linux::detect(path);
    #[cfg(windows)]
    return windows::detect(path);
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = path;
        Detection::unknown("drive detection is not implemented on this OS")
    }
}

/// Opens `path` for reading and tells the OS the file will be read from start to end
/// (Windows: `FILE_FLAG_SEQUENTIAL_SCAN`; Linux: `posix_fadvise(POSIX_FADV_SEQUENTIAL)`; other
/// systems: a plain open). The hint is best effort: if it cannot be given the file is opened
/// anyway, so this fails only when `File::open` would.
pub fn open_sequential(path: &Path) -> io::Result<File> {
    #[cfg(target_os = "linux")]
    return open_hinted(path, linux::open_sequential);
    #[cfg(windows)]
    return open_hinted(path, windows::open_sequential);
    #[cfg(not(any(target_os = "linux", windows)))]
    File::open(path)
}

#[cfg_attr(not(any(target_os = "linux", windows, test)), allow(dead_code))]
fn open_hinted(path: &Path, hinted: impl FnOnce(&Path) -> io::Result<File>) -> io::Result<File> {
    hinted(path).or_else(|_| File::open(path))
}

/// Whether `a` and `b` live on the same device: `Some(true)` or `Some(false)`, or `None` when
/// the OS did not say (network paths, a failed query, an unsupported OS). Neither path needs
/// to exist: the closest existing parent is used.
///
/// Linux compares `st_dev`; Windows compares volume GUIDs. `Some(false)` only means "different
/// device id": two partitions or subvolumes of one physical disk still differ. Never panics
/// and never starts a process.
pub fn same_device(a: &Path, b: &Path) -> Option<bool> {
    #[cfg(target_os = "linux")]
    return Some(linux::device_id(a)? == linux::device_id(b)?);
    #[cfg(windows)]
    return Some(windows::volume_of(a)? == windows::volume_of(b)?);
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = (a, b);
        None
    }
}

/// Sets the priority of the calling thread. Windows: background mode (lowers CPU and, per
/// the docs' wording, resource scheduling priorities). Linux: nice plus I/O priority.
/// Other systems: does nothing and succeeds.
///
/// Going back to `Normal` can fail on Linux for an unprivileged process, because the kernel
/// does not let it lower a nice value again; the I/O priority is still restored.
pub fn set_current_thread_priority(priority: Priority) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    return linux::set_priority(priority);
    #[cfg(windows)]
    return windows::set_priority(priority);
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = priority;
        Ok(())
    }
}

/// Results are keyed by device, so a second path on the same drive costs one stat. The lock
/// is never held while querying the OS.
static CACHE: Mutex<Vec<(String, Detection)>> = Mutex::new(Vec::new());

#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
pub(crate) fn cached(key: String, query: impl FnOnce() -> Detection) -> Detection {
    if let Ok(cache) = CACHE.lock()
        && let Some((_, d)) = cache.iter().find(|(k, _)| *k == key)
    {
        return d.clone();
    }
    let d = query();
    if let Ok(mut cache) = CACHE.lock()
        && !cache.iter().any(|(k, _)| *k == key)
    {
        cache.push((key, d.clone()));
    }
    d
}

/// `path` or its closest existing ancestor, made absolute.
#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
pub(crate) fn existing_ancestor(path: &Path) -> PathBuf {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut p = abs.as_path();
    while !p.exists() {
        match p.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => p = parent,
            _ => break,
        }
    }
    p.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_detection_gives_a_valid_answer_and_does_not_panic() {
        // CI runners are virtual machines: only "no panic, valid value" can be asserted.
        let here = std::env::current_dir().unwrap();
        let missing = here.join("no-such-dir").join("out.zip");
        for p in [
            here.as_path(),
            missing.as_path(),
            Path::new(""),
            Path::new("relative/missing"),
            Path::new("."),
        ] {
            let d = detect(p);
            assert!(matches!(
                d.kind,
                DriveKind::Ssd | DriveKind::Hdd | DriveKind::Unknown
            ));
            assert!(!d.reason.is_empty());
            assert_eq!(drive_kind(p), d.kind, "the cached answer must agree");
            assert!(d.to_string().starts_with("class: "));
        }
    }

    #[test]
    fn priority_can_be_lowered_and_restored_on_a_worker_thread() {
        std::thread::spawn(|| {
            // Errors are allowed (no permission to restore nice); panics are not.
            let _ = set_current_thread_priority(Priority::Low);
            let _ = set_current_thread_priority(Priority::Low);
            let _ = set_current_thread_priority(Priority::Normal);
            let _ = set_current_thread_priority(Priority::Normal);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn sequential_open_reads_like_a_plain_open_and_fails_like_one() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.bin");
        std::fs::write(&file, b"hello").unwrap();
        let mut got = String::new();
        io::Read::read_to_string(&mut open_sequential(&file).unwrap(), &mut got).unwrap();
        assert_eq!(got, "hello");
        let missing = dir.path().join("missing");
        let err = open_sequential(&missing).unwrap_err();
        assert_eq!(err.kind(), File::open(&missing).unwrap_err().kind());
    }

    #[test]
    fn a_failing_hint_falls_back_to_a_plain_open() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.bin");
        std::fs::write(&file, b"x").unwrap();
        let f = open_hinted(&file, |_| Err(io::Error::other("no hint")));
        assert!(f.is_ok());
        let f = open_hinted(&dir.path().join("missing"), |_| {
            Err(io::Error::other("no hint"))
        });
        assert_eq!(f.unwrap_err().kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn same_device_agrees_with_itself_and_never_panics() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("sub").join("not-yet-there.zip");
        std::fs::write(&a, b"x").unwrap();
        #[cfg(any(target_os = "linux", windows))]
        {
            assert_eq!(same_device(&a, &a), Some(true));
            assert_eq!(same_device(&a, &b), Some(true));
            assert_eq!(same_device(&a, dir.path()), Some(true));
        }
        for (x, y) in [
            (Path::new(""), Path::new(".")),
            (Path::new("relative/missing"), a.as_path()),
        ] {
            let _ = same_device(x, y);
        }
        #[cfg(target_os = "linux")]
        if Path::new("/proc").exists() {
            // /proc is its own file system, so it cannot share a device with a temp dir.
            assert_eq!(same_device(&a, Path::new("/proc")), Some(false));
        }
    }

    #[test]
    fn cache_keeps_the_first_answer() {
        let a = cached("test-key".into(), || Detection::unknown("first"));
        let b = cached("test-key".into(), || Detection::unknown("second"));
        assert_eq!((a.reason.as_str(), b.reason.as_str()), ("first", "first"));
    }
}
