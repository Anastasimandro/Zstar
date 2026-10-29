//! Entry-name sanitising: the zip-slip / path-traversal barrier.
//!
//! Every name that comes out of an archive goes through [`sanitize`] before it is joined to
//! the destination. The result is a relative path made only of plain components, so joining
//! it to a directory can never leave that directory.

use std::path::PathBuf;

use crate::error::{Error, Result};

fn unsafe_path(name: &[u8], reason: &'static str) -> Error {
    Error::UnsafePath {
        name: String::from_utf8_lossy(name).into_owned(),
        reason,
    }
}

const RESERVED: [&str; 4] = ["CON", "PRN", "AUX", "NUL"];

/// Windows device names (`CON`, `COM1`, `LPT3`, ...), with or without an extension.
fn is_windows_reserved(component: &[u8]) -> bool {
    let stem_end = component
        .iter()
        .position(|&b| b == b'.')
        .unwrap_or(component.len());
    let stem = &component[..stem_end];
    let stem = &stem[..stem.iter().rposition(|&b| b != b' ').map_or(0, |i| i + 1)];
    let Ok(stem) = std::str::from_utf8(stem) else {
        return false;
    };
    let upper = stem.to_ascii_uppercase();
    if RESERVED.contains(&upper.as_str()) {
        return true;
    }
    match upper.as_bytes() {
        [b'C', b'O', b'M', d] | [b'L', b'P', b'T', d] => d.is_ascii_digit(),
        _ => false,
    }
}

fn check_windows_component(name: &[u8], component: &[u8]) -> Result<()> {
    if component
        .iter()
        .any(|&b| b < 0x20 || b"<>:\"|?*".contains(&b))
    {
        return Err(unsafe_path(
            name,
            "contains a character Windows does not allow",
        ));
    }
    if matches!(component.last(), Some(b'.' | b' ')) {
        return Err(unsafe_path(
            name,
            "ends with a dot or space, which Windows strips",
        ));
    }
    if is_windows_reserved(component) {
        return Err(unsafe_path(name, "is a reserved Windows device name"));
    }
    Ok(())
}

#[cfg(unix)]
fn component_to_os(component: &[u8]) -> std::ffi::OsString {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::OsStr::from_bytes(component).to_owned()
}

#[cfg(not(unix))]
fn component_to_os(component: &[u8]) -> std::ffi::OsString {
    String::from_utf8_lossy(component).into_owned().into()
}

/// Turns an archive entry name into a safe relative path.
///
/// Both `/` and `\` are separators, since zip files made on Windows use the latter.
/// Empty and `.` components are dropped. The name is rejected when it:
/// contains a NUL byte; is absolute (leading separator or a drive prefix such as `C:`);
/// or has a `..` component. With `windows_rules`, names Windows cannot create are rejected
/// too. An empty result means "the destination directory itself".
pub fn sanitize(name: &[u8], windows_rules: bool) -> Result<PathBuf> {
    if name.contains(&0) {
        return Err(unsafe_path(name, "contains a NUL byte"));
    }
    if matches!(name.first(), Some(b'/' | b'\\')) {
        return Err(unsafe_path(name, "is an absolute path"));
    }
    let mut out = PathBuf::new();
    for (i, component) in name.split(|&b| b == b'/' || b == b'\\').enumerate() {
        if i == 0 && matches!(component, [d, b':', ..] if d.is_ascii_alphabetic()) {
            return Err(unsafe_path(name, "has a drive prefix"));
        }
        match component {
            b"" | b"." => continue,
            b".." => return Err(unsafe_path(name, "contains a '..' component")),
            _ => {}
        }
        if windows_rules {
            check_windows_component(name, component)?;
        }
        out.push(component_to_os(component));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(name: &str) -> String {
        sanitize(name.as_bytes(), false)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/")
    }

    fn rejected(name: &str, windows: bool) -> bool {
        matches!(
            sanitize(name.as_bytes(), windows),
            Err(Error::UnsafePath { .. })
        )
    }

    #[test]
    fn accepts_ordinary_names() {
        assert_eq!(ok("a/b/c.txt"), "a/b/c.txt");
        assert_eq!(ok("./a//b/./c"), "a/b/c");
        assert_eq!(ok("dir/"), "dir");
        assert_eq!(ok("."), "");
        assert_eq!(ok("a..b/..c/d.."), "a..b/..c/d..");
        assert_eq!(ok("ünïcode/日本語.txt"), "ünïcode/日本語.txt");
    }

    #[test]
    fn rejects_parent_traversal_in_every_position() {
        for n in [
            "..",
            "../x",
            "a/../b",
            "a/b/..",
            "a/../../x",
            "..\\x",
            "a\\..\\..\\x",
            "./../x",
        ] {
            assert!(rejected(n, false), "{n} should be rejected");
        }
    }

    #[test]
    fn rejects_absolute_paths_and_drive_prefixes() {
        for n in [
            "/etc/passwd",
            "\\windows\\x",
            "C:\\x",
            "c:/x",
            "C:x",
            "//server/share",
        ] {
            assert!(rejected(n, false), "{n} should be rejected");
        }
        // A colon later in the name is an ordinary character off Windows.
        assert_eq!(ok("a/b:c"), "a/b:c");
    }

    #[test]
    fn rejects_nul() {
        assert!(rejected("a\0b", false));
    }

    #[test]
    fn backslash_is_a_separator() {
        assert_eq!(ok("a\\b\\c"), "a/b/c");
    }

    #[test]
    fn windows_rules_reject_reserved_names() {
        for n in [
            "con",
            "CON",
            "Aux.txt",
            "dir/nul",
            "NUL.tar.gz",
            "com1",
            "LPT9.log",
            "a/PRN",
            "con .txt",
        ] {
            assert!(
                rejected(n, true),
                "{n} should be rejected with windows rules"
            );
            assert!(!rejected(n, false), "{n} is fine without windows rules");
        }
        for n in ["console", "com10", "lpt", "com", "aux1", "nul_x", "c.on"] {
            assert!(!rejected(n, true), "{n} should be accepted");
        }
    }

    #[test]
    fn windows_rules_reject_bad_characters_and_trailing_dots() {
        for n in [
            "a<b", "a>b", "a:b", "a\"b", "a|b", "a?b", "a*b", "a\x01b", "name.", "name ", "d./f",
        ] {
            assert!(
                rejected(n, true),
                "{n:?} should be rejected with windows rules"
            );
        }
    }

    #[test]
    fn components_never_contain_separators() {
        let p = sanitize(b"a/b\\c/d", false).unwrap();
        assert_eq!(p.components().count(), 4);
        assert!(p.is_relative());
    }
}
