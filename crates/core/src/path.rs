use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Joins an archive entry name onto `base`, refusing anything that could
/// land outside it (zip-slip). Both `/` and `\` separate components. Rejects
/// absolute names, `..`, NUL, names with no component at all, and on
/// Windows any component containing `:` (drive letters, alternate streams).
/// The check is purely lexical; it does not look at the file system.
pub fn safe_join(base: &Path, name: &str) -> Result<PathBuf> {
    let unsafe_path = || Error::UnsafePath(name.to_owned());

    if name.is_empty() || name.contains('\0') || name.starts_with(['/', '\\']) {
        return Err(unsafe_path());
    }

    let mut joined = base.to_path_buf();
    let mut components = 0usize;
    for component in name.split(['/', '\\']) {
        match component {
            "" | "." => {}
            ".." => return Err(unsafe_path()),
            _ if cfg!(windows) && component.contains(':') => return Err(unsafe_path()),
            _ => {
                joined.push(component);
                components += 1;
            }
        }
    }
    if components == 0 {
        return Err(unsafe_path());
    }
    Ok(joined)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn join(name: &str) -> Result<PathBuf> {
        safe_join(Path::new("dest"), name)
    }

    #[test]
    fn accepts_plain_and_nested_names() {
        assert_eq!(join("a.txt").unwrap(), Path::new("dest").join("a.txt"));
        assert_eq!(
            join("dir/sub/a.txt").unwrap(),
            Path::new("dest").join("dir").join("sub").join("a.txt")
        );
        assert_eq!(join("dir/").unwrap(), Path::new("dest").join("dir"));
        assert_eq!(join("./a.txt").unwrap(), Path::new("dest").join("a.txt"));
        assert_eq!(
            join("dir\\a.txt").unwrap(),
            Path::new("dest").join("dir").join("a.txt")
        );
    }

    #[test]
    fn rejects_traversal() {
        for name in ["../x", "a/../../x", "a/..", "..\\x", "a\\..\\..\\x", ".."] {
            assert!(
                matches!(join(name), Err(Error::UnsafePath(_))),
                "{name} should be rejected"
            );
        }
    }

    #[test]
    fn rejects_absolute_and_degenerate_names() {
        for name in ["/etc/passwd", "\\windows\\system32", "", "./", "/", "a\0b"] {
            assert!(
                matches!(join(name), Err(Error::UnsafePath(_))),
                "{name:?} should be rejected"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn rejects_drive_letters_on_windows() {
        assert!(matches!(join("C:/x"), Err(Error::UnsafePath(_))));
        assert!(matches!(join("a/C:x"), Err(Error::UnsafePath(_))));
    }

    #[test]
    fn keeps_dots_inside_names() {
        assert_eq!(join("a..b").unwrap(), Path::new("dest").join("a..b"));
        assert_eq!(join(".hidden").unwrap(), Path::new("dest").join(".hidden"));
    }
}
