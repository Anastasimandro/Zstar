//! Turning files and directories on disk into a flat list of things to archive.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::error::{Error, Result};
use crate::format::EntryKind;

/// One file-system object to be stored in a new archive.
#[derive(Debug, Clone)]
pub struct SourceEntry {
    /// Where the data lives on disk.
    pub path: PathBuf,
    /// Name inside the archive: relative, `/`-separated, no trailing slash.
    pub name: String,
    /// File, directory or symlink.
    pub kind: EntryKind,
    /// Size in bytes (0 for directories and links).
    pub size: u64,
    /// Unix permission bits (`0o777` mask), where the platform has them.
    pub mode: Option<u32>,
    /// Modification time.
    pub mtime: Option<SystemTime>,
    /// Target of a symlink, as stored on disk.
    pub link_target: Option<String>,
}

#[cfg(unix)]
fn mode_of(meta: &fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(meta.permissions().mode() & 0o777)
}

#[cfg(not(unix))]
fn mode_of(_meta: &fs::Metadata) -> Option<u32> {
    None
}

fn utf8(name: &std::ffi::OsStr) -> Result<&str> {
    name.to_str().ok_or_else(|| {
        Error::InvalidOption(format!(
            "file name is not valid UTF-8: {}",
            name.to_string_lossy()
        ))
    })
}

fn describe(path: &Path, name: String) -> Result<Option<SourceEntry>> {
    let meta = fs::symlink_metadata(path)?;
    let ft = meta.file_type();
    let mut entry = SourceEntry {
        path: path.to_owned(),
        name,
        kind: EntryKind::File,
        size: 0,
        mode: mode_of(&meta),
        mtime: meta.modified().ok(),
        link_target: None,
    };
    if ft.is_symlink() {
        entry.kind = EntryKind::Symlink;
        entry.link_target = Some(utf8(fs::read_link(path)?.as_os_str())?.to_owned());
    } else if ft.is_dir() {
        entry.kind = EntryKind::Dir;
    } else if ft.is_file() {
        entry.size = meta.len();
    } else {
        return Ok(None); // sockets, FIFOs, devices
    }
    Ok(Some(entry))
}

/// Lists `inputs` and everything below them, parents before children, siblings sorted by
/// name so the output is deterministic. Symlinks are stored, not followed.
///
/// Each input appears in the archive under its own file name. Entries whose canonical path
/// equals one of `exclude` are skipped (use it to keep an output archive out of itself).
pub fn collect_sources(inputs: &[PathBuf], exclude: &[PathBuf]) -> Result<Vec<SourceEntry>> {
    let mut out = Vec::new();
    for input in inputs {
        let base = match input.file_name() {
            Some(n) => utf8(n)?.to_owned(),
            None => utf8(fs::canonicalize(input)?.file_name().ok_or_else(|| {
                Error::InvalidOption(format!("cannot archive {}", input.display()))
            })?)?
            .to_owned(),
        };
        let mut stack = vec![(input.clone(), base)];
        while let Some((path, name)) = stack.pop() {
            if !exclude.is_empty() && is_excluded(&path, exclude) {
                continue;
            }
            let Some(entry) = describe(&path, name.clone())? else {
                continue;
            };
            let is_dir = entry.kind == EntryKind::Dir;
            out.push(entry);
            if is_dir {
                let mut children = fs::read_dir(&path)?
                    .map(|e| e.map(|e| e.file_name()))
                    .collect::<std::io::Result<Vec<_>>>()?;
                children.sort();
                for child in children.into_iter().rev() {
                    let child_name = format!("{name}/{}", utf8(&child)?);
                    stack.push((path.join(&child), child_name));
                }
            }
        }
    }
    Ok(out)
}

fn is_excluded(path: &Path, exclude: &[PathBuf]) -> bool {
    // Cheap pre-check on the file name before touching the file system.
    if !exclude.iter().any(|e| e.file_name() == path.file_name()) {
        return false;
    }
    fs::canonicalize(path).is_ok_and(|p| exclude.contains(&p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walks_in_sorted_parent_first_order_and_honours_exclude() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        fs::create_dir_all(root.join("b")).unwrap();
        fs::create_dir_all(root.join("a")).unwrap();
        fs::write(root.join("b/z.txt"), b"zz").unwrap();
        fs::write(root.join("a/y.txt"), b"y").unwrap();
        fs::write(root.join("skip.bin"), b"s").unwrap();

        let excl = fs::canonicalize(root.join("skip.bin")).unwrap();
        let got = collect_sources(std::slice::from_ref(&root), &[excl]).unwrap();
        let names: Vec<_> = got.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            ["proj", "proj/a", "proj/a/y.txt", "proj/b", "proj/b/z.txt"]
        );
        assert_eq!(got[4].size, 2);
        assert_eq!(got[1].kind, EntryKind::Dir);
    }

    #[test]
    fn dot_input_uses_the_real_directory_name() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("f"), b"1").unwrap();
        let got = collect_sources(&[tmp.path().join(".")], &[]).unwrap();
        let top = tmp.path().file_name().unwrap().to_str().unwrap();
        assert_eq!(got[0].name, top);
        assert_eq!(got[1].name, format!("{top}/f"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_stored_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/definitely/elsewhere", tmp.path().join("l")).unwrap();
        let got = collect_sources(&[tmp.path().join("l")], &[]).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, EntryKind::Symlink);
        assert_eq!(got[0].link_target.as_deref(), Some("/definitely/elsewhere"));
    }
}
