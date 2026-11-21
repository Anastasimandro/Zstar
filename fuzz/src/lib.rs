//! Shared harness of the fuzz targets: an extraction sandbox and the checks run after it.

use std::ffi::OsString;
use std::fs;
use std::io::Cursor;
use std::path::{Component, Path, PathBuf};

use tempfile::TempDir;
use zstar_core::{Format, Options, handler};

const GUARD: &[u8] = b"outside the destination: must never change";

/// A fresh directory holding the extraction destination next to a guard file. Nothing an
/// extraction does may add to or change anything but the destination.
pub struct Sandbox {
    outer: TempDir,
}

impl Sandbox {
    pub fn new() -> Self {
        let outer = tempfile::tempdir().expect("temp dir");
        fs::create_dir(outer.path().join("dest")).expect("dest dir");
        fs::create_dir(outer.path().join("guard")).expect("guard dir");
        fs::write(outer.path().join("guard/file"), GUARD).expect("guard file");
        Sandbox { outer }
    }

    pub fn dest(&self) -> PathBuf {
        self.outer.path().join("dest")
    }

    /// Panics (a crash for the fuzzer) when the extraction left the destination.
    pub fn check(&self) {
        let mut top: Vec<OsString> = fs::read_dir(self.outer.path())
            .expect("read outer")
            .map(|e| e.expect("entry").file_name())
            .collect();
        top.sort();
        assert!(
            top == [OsString::from("dest"), OsString::from("guard")],
            "extraction wrote next to the destination: {top:?}"
        );
        let guard: Vec<OsString> = fs::read_dir(self.outer.path().join("guard"))
            .expect("read guard")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert!(guard == [OsString::from("file")], "guard directory changed");
        assert_eq!(
            fs::read(self.outer.path().join("guard/file")).expect("guard file"),
            GUARD,
            "guard file changed"
        );

        // Links are created last and must resolve inside the destination.
        let dest = self.dest();
        let mut dirs = vec![dest.clone()];
        while let Some(dir) = dirs.pop() {
            for entry in fs::read_dir(&dir).expect("read dest") {
                let path = entry.expect("entry").path();
                let meta = fs::symlink_metadata(&path).expect("stat");
                if meta.file_type().is_symlink() {
                    let target = fs::read_link(&path).expect("read link");
                    let resolved = lexical(&dir.join(target));
                    assert!(
                        resolved.starts_with(&dest),
                        "symlink {path:?} points outside the destination"
                    );
                } else if meta.is_dir() {
                    dirs.push(path);
                }
            }
        }
    }
}

impl Default for Sandbox {
    fn default() -> Self {
        Self::new()
    }
}

/// `path` with `.` and `..` resolved without touching the file system.
pub fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// Lists then extracts `data` as `format` with the default options and limits, and checks
/// that nothing left the sandbox. Errors are the expected outcome for hostile input.
pub fn list_and_extract(format: Format, data: &[u8], name_hint: Option<&str>) {
    let Ok(handler) = handler(format) else {
        return;
    };
    let opts = Options {
        name_hint: name_hint.map(str::to_owned),
        ..Options::default()
    };

    let mut entries = 0u64;
    let _ = handler.list(&mut Cursor::new(data), &opts, &mut |_| entries += 1);

    let sandbox = Sandbox::new();
    let _ = handler.extract(&mut Cursor::new(data), &sandbox.dest(), &opts);
    sandbox.check();
}

/// The `Format` for `selector`, cycling through `formats`.
pub fn pick(selector: u8, formats: &[Format]) -> Format {
    formats[usize::from(selector) % formats.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untouched_sandbox_passes() {
        let s = Sandbox::new();
        fs::write(s.dest().join("ok.txt"), b"x").unwrap();
        fs::create_dir(s.dest().join("d")).unwrap();
        std::os::unix::fs::symlink("../ok.txt", s.dest().join("d/inside")).unwrap();
        s.check();
    }

    #[test]
    #[should_panic(expected = "wrote next to the destination")]
    fn file_beside_the_destination_is_caught() {
        let s = Sandbox::new();
        fs::write(s.outer.path().join("evil"), b"x").unwrap();
        s.check();
    }

    #[test]
    #[should_panic(expected = "guard file changed")]
    fn changed_guard_is_caught() {
        let s = Sandbox::new();
        fs::write(s.outer.path().join("guard/file"), b"changed").unwrap();
        s.check();
    }

    #[test]
    #[should_panic(expected = "guard directory changed")]
    fn new_file_in_guard_is_caught() {
        let s = Sandbox::new();
        fs::write(s.outer.path().join("guard/evil"), b"x").unwrap();
        s.check();
    }

    #[test]
    #[should_panic(expected = "points outside the destination")]
    fn relative_symlink_escape_is_caught() {
        let s = Sandbox::new();
        std::os::unix::fs::symlink("../guard/file", s.dest().join("l")).unwrap();
        s.check();
    }

    #[test]
    #[should_panic(expected = "points outside the destination")]
    fn absolute_symlink_escape_is_caught() {
        let s = Sandbox::new();
        std::os::unix::fs::symlink("/etc/passwd", s.dest().join("l")).unwrap();
        s.check();
    }

    #[test]
    fn lexical_resolves_dots() {
        assert_eq!(lexical(Path::new("/a/b/../c/./d")), Path::new("/a/c/d"));
    }
}
