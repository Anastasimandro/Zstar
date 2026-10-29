//! The safe extraction session shared by every format.
//!
//! A format handler parses its archive and hands entries to a [`Session`]; the session owns
//! all file-system effects. That keeps the security rules in one place:
//!
//! * names are sanitised and joined below a canonical destination root;
//! * data is written to `<name>.part` and renamed only after the whole entry succeeded;
//! * bomb limits are charged as bytes are actually written, whatever the headers claim;
//! * symbolic links are created last and verified to resolve inside the root;
//! * permissions never gain setuid/setgid/sticky or group/other write bits.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::SystemTime;

use crate::error::{Error, LimitKind, Result};
use crate::options::Options;
use crate::path::sanitize;
use crate::progress::Tracker;

/// Size of the fixed copy buffer each worker uses.
pub(crate) const CHUNK: usize = 128 * 1024;

/// What an extraction did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Summary {
    /// Regular files written (hard links included).
    pub files: u64,
    /// Directories created.
    pub dirs: u64,
    /// Symbolic links created.
    pub symlinks: u64,
    /// Uncompressed bytes written.
    pub bytes: u64,
    /// Entries skipped: devices, FIFOs, and symlinks on platforms without them.
    pub skipped: u64,
}

pub(crate) fn part_path(dest: &Path) -> PathBuf {
    let mut s: OsString = dest.as_os_str().to_owned();
    s.push(".part");
    s.into()
}

pub(crate) struct Session<'a> {
    root: PathBuf,
    opts: &'a Options,
    tracker: Tracker<'a>,
    input_len: u64,
    #[cfg_attr(not(feature = "tar"), allow(dead_code))]
    entries: AtomicU64,
    files: AtomicU64,
    dirs: AtomicU64,
    skipped: AtomicU64,
    abort: AtomicBool,
    #[cfg_attr(not(unix), allow(dead_code))]
    symlinks: Mutex<Vec<DeferredLink>>,
    #[cfg_attr(not(unix), allow(dead_code))]
    deferred_bytes: AtomicU64,
}

/// Memory cap for symlinks waiting to be created.
#[cfg_attr(not(unix), allow(dead_code))]
const MAX_DEFERRED_LINK_BYTES: u64 = 64 << 20;

#[cfg_attr(not(unix), allow(dead_code))]
struct DeferredLink {
    rel: PathBuf,
    target: Vec<u8>,
    name: String,
}

impl<'a> Session<'a> {
    /// Creates `dest` if needed and fixes its canonical form as the root.
    ///
    /// On Windows `canonicalize` returns a `\\?\` path, which lifts the 260-character limit
    /// for everything joined below it.
    pub(crate) fn new(
        dest: &Path,
        opts: &'a Options,
        input_len: u64,
        bytes_total: Option<u64>,
        entries_total: Option<u64>,
    ) -> Result<Self> {
        fs::create_dir_all(dest)?;
        Ok(Session {
            root: fs::canonicalize(dest)?,
            opts,
            tracker: Tracker::new(opts, bytes_total, entries_total),
            input_len,
            entries: AtomicU64::new(0),
            files: AtomicU64::new(0),
            dirs: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            abort: AtomicBool::new(false),
            symlinks: Mutex::new(Vec::new()),
            deferred_bytes: AtomicU64::new(0),
        })
    }

    #[cfg_attr(not(feature = "tar"), allow(dead_code))]
    pub(crate) fn rel_path(&self, name: &[u8]) -> Result<PathBuf> {
        sanitize(name, self.opts.windows_names)
    }

    /// Stops all workers: they see [`Error::Cancelled`] at their next check.
    #[cfg_attr(not(feature = "zip"), allow(dead_code))]
    pub(crate) fn abort(&self) {
        self.abort.store(true, Ordering::Relaxed);
    }

    pub(crate) fn check_stop(&self) -> Result<()> {
        if self.abort.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        self.opts.check_cancel()
    }

    /// Counts one more entry against the entry limit.
    #[cfg_attr(not(feature = "tar"), allow(dead_code))]
    pub(crate) fn begin_entry(&self) -> Result<()> {
        self.check_stop()?;
        let n = self.entries.fetch_add(1, Ordering::Relaxed) + 1;
        if n > self.opts.limits.max_entries {
            return Err(Error::LimitExceeded(LimitKind::Entries));
        }
        Ok(())
    }

    /// Charges `n` bytes against the size and ratio limits.
    fn charge(&self, n: u64, current: &str) -> Result<()> {
        let total = self.tracker.add_bytes(n, current);
        let limits = &self.opts.limits;
        if total > limits.max_total_bytes {
            return Err(Error::LimitExceeded(LimitKind::TotalSize));
        }
        if limits.max_ratio > 0
            && total > limits.ratio_min_bytes
            && total / self.input_len.max(1) > limits.max_ratio
        {
            return Err(Error::LimitExceeded(LimitKind::Ratio));
        }
        Ok(())
    }

    pub(crate) fn make_dir(&self, rel: &Path) -> Result<()> {
        if !rel.as_os_str().is_empty() {
            fs::create_dir_all(self.root.join(rel))?;
            self.dirs.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Writes one regular file: `<dest>.part` first, then an atomic rename.
    ///
    /// `buf` is the caller's reusable copy buffer, which keeps memory use constant.
    pub(crate) fn write_file(
        &self,
        rel: &Path,
        name: &str,
        src: &mut dyn Read,
        mode: Option<u32>,
        mtime: Option<SystemTime>,
        buf: &mut [u8],
    ) -> Result<()> {
        if rel.as_os_str().is_empty() {
            return Err(Error::UnsafePath {
                name: name.into(),
                reason: "empty file name",
            });
        }
        let dest = self.root.join(rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        self.check_destination(&dest)?;

        let part = part_path(&dest);
        let written = self.copy_to_part(&part, name, src, mode, mtime, buf);
        let renamed = written.and_then(|()| fs::rename(&part, &dest).map_err(Error::from));
        if renamed.is_err() {
            let _ = fs::remove_file(&part);
        }
        renamed?;
        self.files.fetch_add(1, Ordering::Relaxed);
        self.tracker.entry_done(name);
        Ok(())
    }

    fn check_destination(&self, dest: &Path) -> Result<()> {
        match fs::symlink_metadata(dest) {
            Ok(m) if m.is_dir() || !self.opts.overwrite => Err(Error::AlreadyExists(dest.into())),
            Ok(_) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    fn copy_to_part(
        &self,
        part: &Path,
        name: &str,
        src: &mut dyn Read,
        mode: Option<u32>,
        mtime: Option<SystemTime>,
        buf: &mut [u8],
    ) -> Result<()> {
        let mut file = File::create(part)?;
        loop {
            self.check_stop()?;
            let n = match src.read(buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            };
            self.charge(n as u64, name)?;
            file.write_all(&buf[..n])?;
        }
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::PermissionsExt;
            // Drop setuid/setgid/sticky and group/other write; never lock the owner out.
            file.set_permissions(fs::Permissions::from_mode((mode & 0o755) | 0o400))?;
        }
        #[cfg(not(unix))]
        let _ = mode;
        if let Some(t) = mtime {
            let _ = file.set_modified(t); // a cosmetic failure must not fail the extraction
        }
        Ok(())
    }

    /// Creates a hard link to a file that was already extracted from the same archive.
    #[cfg_attr(not(feature = "tar"), allow(dead_code))]
    pub(crate) fn hard_link(&self, rel: &Path, target: &Path, name: &str, raw: &str) -> Result<()> {
        let link = self.root.join(rel);
        let source = self.root.join(target);
        let is_file = fs::symlink_metadata(&source).is_ok_and(|m| m.is_file());
        if rel.as_os_str().is_empty() || target.as_os_str().is_empty() || !is_file {
            return Err(Error::UnsafeLink {
                name: name.into(),
                target: raw.into(),
            });
        }
        if let Some(parent) = link.parent() {
            fs::create_dir_all(parent)?;
        }
        self.check_destination(&link)?;
        if fs::symlink_metadata(&link).is_ok() {
            fs::remove_file(&link)?;
        }
        fs::hard_link(&source, &link)?;
        self.files.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    #[cfg_attr(not(feature = "tar"), allow(dead_code))]
    pub(crate) fn skip_entry(&self) {
        self.skipped.fetch_add(1, Ordering::Relaxed);
    }

    /// Queues a symbolic link; it is created in [`finish`](Self::finish).
    ///
    /// Queued links live in memory, so their total size is capped regardless of the
    /// configured limits.
    pub(crate) fn defer_symlink(&self, rel: PathBuf, target: Vec<u8>, name: &str) -> Result<()> {
        #[cfg(unix)]
        {
            let queued = self
                .deferred_bytes
                .fetch_add(target.len() as u64 + 64, Ordering::Relaxed);
            if queued > MAX_DEFERRED_LINK_BYTES {
                return Err(Error::LimitExceeded(LimitKind::TotalSize));
            }
            self.symlinks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(DeferredLink {
                    rel,
                    target,
                    name: name.to_owned(),
                });
        }
        #[cfg(not(unix))]
        {
            let _ = (rel, target, name);
            self.skip_entry();
        }
        Ok(())
    }

    /// Creates the queued symlinks and returns the totals.
    pub(crate) fn finish(self) -> Result<Summary> {
        #[allow(unused_mut)]
        let mut symlinks = 0;
        #[cfg(unix)]
        for link in std::mem::take(&mut *self.symlinks.lock().unwrap_or_else(|e| e.into_inner())) {
            self.check_stop()?;
            self.create_symlink(&link)?;
            symlinks += 1;
        }
        Ok(Summary {
            files: self.files.load(Ordering::Relaxed),
            dirs: self.dirs.load(Ordering::Relaxed),
            symlinks,
            bytes: self.tracker.bytes(),
            skipped: self.skipped.load(Ordering::Relaxed),
        })
    }

    /// Creates one symlink, then proves it resolves inside the root.
    ///
    /// Links are made after every file and directory, and each one is checked by resolving
    /// it for real (`canonicalize`), so chains like `a -> .` plus `b -> a/../..` cannot
    /// escape. A link whose target does not exist yet cannot be resolved; for those, any
    /// `..` component is refused outright. Absolute targets are always refused.
    #[cfg(unix)]
    fn create_symlink(&self, link: &DeferredLink) -> Result<()> {
        use std::os::unix::ffi::OsStrExt;
        use std::path::Component;

        let bad = || Error::UnsafeLink {
            name: link.name.clone(),
            target: String::from_utf8_lossy(&link.target).into_owned(),
        };
        let target = Path::new(std::ffi::OsStr::from_bytes(&link.target));
        if link.rel.as_os_str().is_empty()
            || link.target.is_empty()
            || link.target.contains(&0)
            || target.has_root()
        {
            return Err(bad());
        }
        let path = self.root.join(&link.rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        self.check_destination(&path)?;
        if fs::symlink_metadata(&path).is_ok() {
            fs::remove_file(&path)?;
        }
        std::os::unix::fs::symlink(target, &path)?;

        let inside = match fs::canonicalize(&path) {
            Ok(resolved) => resolved.starts_with(&self.root),
            Err(_) => !target.components().any(|c| c == Component::ParentDir),
        };
        if !inside {
            let _ = fs::remove_file(&path);
            return Err(bad());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn session<'a>(dir: &Path, opts: &'a Options) -> Session<'a> {
        Session::new(dir, opts, 1000, None, None).unwrap()
    }

    #[test]
    fn part_file_is_renamed_on_success_and_removed_on_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let opts = Options::default();
        let s = session(tmp.path(), &opts);
        let mut buf = vec![0u8; 16];

        s.write_file(
            Path::new("d/ok.txt"),
            "d/ok.txt",
            &mut Cursor::new(b"hello"),
            None,
            None,
            &mut buf,
        )
        .unwrap();
        assert_eq!(fs::read(tmp.path().join("d/ok.txt")).unwrap(), b"hello");
        assert!(!tmp.path().join("d/ok.txt.part").exists());

        struct Failing;
        impl Read for Failing {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("boom"))
            }
        }
        let err = s.write_file(
            Path::new("bad.txt"),
            "bad.txt",
            &mut Failing,
            None,
            None,
            &mut buf,
        );
        assert!(err.is_err());
        assert!(!tmp.path().join("bad.txt").exists());
        assert!(!tmp.path().join("bad.txt.part").exists());
    }

    #[test]
    fn existing_files_survive_unless_overwrite_is_on() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("f"), b"old").unwrap();
        let mut buf = vec![0u8; 16];

        let opts = Options::default();
        let s = session(tmp.path(), &opts);
        let err = s.write_file(
            Path::new("f"),
            "f",
            &mut Cursor::new(b"new"),
            None,
            None,
            &mut buf,
        );
        assert!(matches!(err, Err(Error::AlreadyExists(_))));
        assert_eq!(fs::read(tmp.path().join("f")).unwrap(), b"old");

        let opts = Options {
            overwrite: true,
            ..Options::default()
        };
        let s = session(tmp.path(), &opts);
        s.write_file(
            Path::new("f"),
            "f",
            &mut Cursor::new(b"new"),
            None,
            None,
            &mut buf,
        )
        .unwrap();
        assert_eq!(fs::read(tmp.path().join("f")).unwrap(), b"new");
    }

    #[test]
    fn total_size_limit_is_enforced_on_actual_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut opts = Options::default();
        opts.limits.max_total_bytes = 100;
        let s = session(tmp.path(), &opts);
        let mut buf = vec![0u8; 32];
        let err = s.write_file(
            Path::new("big"),
            "big",
            &mut io::repeat(7),
            None,
            None,
            &mut buf,
        );
        assert!(matches!(
            err,
            Err(Error::LimitExceeded(LimitKind::TotalSize))
        ));
        assert!(!tmp.path().join("big").exists());
        assert!(!tmp.path().join("big.part").exists());
    }

    #[test]
    fn ratio_limit_only_applies_past_the_minimum() {
        let tmp = tempfile::tempdir().unwrap();
        let mut opts = Options::default();
        opts.limits.max_ratio = 10; // input_len is 1000 in these tests
        opts.limits.ratio_min_bytes = 50_000;
        let s = session(tmp.path(), &opts);
        let mut buf = vec![0u8; 1024];
        // 20_000 bytes is a ratio of 20, but below the minimum: allowed.
        s.write_file(
            Path::new("small"),
            "small",
            &mut io::repeat(1).take(20_000),
            None,
            None,
            &mut buf,
        )
        .unwrap();
        let err = s.write_file(
            Path::new("large"),
            "large",
            &mut io::repeat(1).take(100_000),
            None,
            None,
            &mut buf,
        );
        assert!(matches!(err, Err(Error::LimitExceeded(LimitKind::Ratio))));
    }

    #[test]
    fn entry_limit_is_enforced() {
        let tmp = tempfile::tempdir().unwrap();
        let mut opts = Options::default();
        opts.limits.max_entries = 2;
        let s = session(tmp.path(), &opts);
        s.begin_entry().unwrap();
        s.begin_entry().unwrap();
        assert!(matches!(
            s.begin_entry(),
            Err(Error::LimitExceeded(LimitKind::Entries))
        ));
    }

    #[test]
    fn cancel_stops_the_copy_and_leaves_no_part_file() {
        let tmp = tempfile::tempdir().unwrap();
        let opts = Options::default();
        opts.cancel.cancel();
        let s = session(tmp.path(), &opts);
        let mut buf = vec![0u8; 16];
        let err = s.write_file(
            Path::new("f"),
            "f",
            &mut Cursor::new(b"data"),
            None,
            None,
            &mut buf,
        );
        assert!(matches!(err, Err(Error::Cancelled)));
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn modes_are_masked() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let opts = Options::default();
        let s = session(tmp.path(), &opts);
        let mut buf = vec![0u8; 16];
        for (name, mode, want) in [
            ("a", 0o4777, 0o755),
            ("b", 0o666, 0o644),
            ("c", 0o000, 0o400),
            ("d", 0o755, 0o755),
        ] {
            s.write_file(
                Path::new(name),
                name,
                &mut Cursor::new(b"x"),
                Some(mode),
                None,
                &mut buf,
            )
            .unwrap();
            let got = fs::metadata(tmp.path().join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777;
            assert_eq!(got, want, "mode {mode:o} for {name}");
        }
    }
}
