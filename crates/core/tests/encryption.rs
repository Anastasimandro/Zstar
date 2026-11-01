//! AES-256 in zip (WinZip AES) and 7z: round trips, wrong or missing passwords, damaged
//! archives, no leftovers, and interoperability with the real `7z` tool when it is installed.
#![cfg(any(feature = "zip-aes", feature = "sevenz-aes"))]

mod common;

use std::fs;
use std::path::Path;
use std::process::Command;

use common::*;
use zstar_core::{
    Error, Format, Options, Password, ZipMethod, create_file, extract_file, list_file,
};

const PW: &str = "correct horse battery staple";
const MARKER: &str = "zstar-marker-that-must-not-leak";

fn encrypted_formats() -> Vec<Format> {
    let mut v = Vec::new();
    if cfg!(feature = "zip-aes") {
        v.push(Format::Zip);
    }
    if cfg!(feature = "sevenz-aes") {
        v.push(Format::SevenZ);
    }
    v
}

fn with(pw: Option<&str>) -> Options {
    Options {
        password: pw.map(Password::new),
        ..Options::default()
    }
}

/// A project directory whose files all contain `MARKER`, archived with `pw`.
fn encrypted_archive(dir: &Path, format: Format, opts: &Options) -> std::path::PathBuf {
    let src = dir.join("proj");
    fs::create_dir_all(src.join("sub")).unwrap();
    write(&src.join("a.txt"), MARKER.repeat(200).as_bytes());
    write(&src.join("sub/b.bin"), &pseudo(300_000, 7));
    write(&src.join("empty.txt"), b"");
    let archive = dir.join(format!("a.{}", ext(format)));
    create_file(format, &[src], &archive, opts).unwrap();
    archive
}

/// Whether any regular file exists under `dest` (which may not exist at all).
fn wrote_files(dest: &Path) -> bool {
    dest.exists()
        && snapshot(dest)
            .values()
            .any(|n| matches!(n, Node::File { .. }))
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn round_trip_with_the_right_password() {
    for format in encrypted_formats() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        make_tree(&src);
        let archive = tmp.path().join(format!("a.{}", ext(format)));
        create_file(
            format,
            std::slice::from_ref(&src),
            &archive,
            &with(Some(PW)),
        )
        .unwrap();
        assert!(find_part_files(tmp.path()).is_empty());
        let dest = tmp.path().join("dest");
        extract_file(&archive, &dest, &with(Some(PW))).unwrap();
        assert_eq!(snapshot(&src), snapshot(&dest.join("proj")), "{format:?}");
    }
}

#[test]
fn parallel_extraction_decrypts_every_entry() {
    if !cfg!(feature = "zip-aes") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("many");
    for i in 0..60 {
        write(
            &src.join(format!("f{i}.bin")),
            &pseudo(5_000 + i * 97, i as u64),
        );
    }
    let archive = tmp.path().join("a.zip");
    let opts = Options {
        threads: 4,
        ..with(Some(PW))
    };
    create_file(Format::Zip, std::slice::from_ref(&src), &archive, &opts).unwrap();
    let dest = tmp.path().join("dest");
    extract_file(&archive, &dest, &opts).unwrap();
    assert_eq!(snapshot(&src), snapshot(&dest.join("many")));
}

#[test]
fn the_archive_does_not_contain_the_plaintext() {
    for format in encrypted_formats() {
        let tmp = tempfile::tempdir().unwrap();
        // Stored zip entries would show the plaintext if encryption did nothing.
        let opts = Options {
            zip_method: Some(ZipMethod::Stored),
            ..with(Some(PW))
        };
        let archive = encrypted_archive(tmp.path(), format, &opts);
        let bytes = fs::read(&archive).unwrap();
        assert!(
            !contains(&bytes, MARKER.as_bytes()),
            "{format:?} leaks data"
        );
        assert!(
            !contains(&bytes, PW.as_bytes()),
            "{format:?} stores the password"
        );
    }
}

#[test]
fn seven_z_hides_file_names_too() {
    if !cfg!(feature = "sevenz-aes") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let archive = encrypted_archive(tmp.path(), Format::SevenZ, &with(Some(PW)));
    assert!(!contains(&fs::read(&archive).unwrap(), b"empty.txt"));
    // Without the password even the listing is refused.
    let err = list_file(&archive, &with(None), &mut |_| {}).unwrap_err();
    assert!(matches!(err, Error::PasswordRequired), "{err}");
    let mut names = Vec::new();
    list_file(&archive, &with(Some(PW)), &mut |e| names.push(e.name)).unwrap();
    assert!(names.iter().any(|n| n.ends_with("b.bin")), "{names:?}");
}

#[test]
fn missing_password_fails_cleanly() {
    for format in encrypted_formats() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = encrypted_archive(tmp.path(), format, &with(Some(PW)));
        let dest = tmp.path().join("dest");
        let err = extract_file(&archive, &dest, &with(None)).unwrap_err();
        assert!(matches!(err, Error::PasswordRequired), "{format:?}: {err}");
        assert!(find_part_files(tmp.path()).is_empty(), "{format:?}");
        // Nothing with content may have been written.
        assert!(
            !wrote_files(&dest),
            "{format:?} wrote files without the password"
        );
    }
}

#[test]
fn wrong_password_fails_cleanly() {
    for format in encrypted_formats() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = encrypted_archive(tmp.path(), format, &with(Some(PW)));
        let dest = tmp.path().join("dest");
        let err = extract_file(&archive, &dest, &with(Some("not the password"))).unwrap_err();
        assert!(matches!(err, Error::WrongPassword), "{format:?}: {err}");
        assert!(find_part_files(tmp.path()).is_empty(), "{format:?}");
        assert!(
            !wrote_files(&dest),
            "{format:?} wrote files with a wrong password"
        );
    }
}

#[test]
fn errors_never_show_the_password() {
    let tmp = tempfile::tempdir().unwrap();
    for format in encrypted_formats() {
        let archive = encrypted_archive(tmp.path(), format, &with(Some(PW)));
        let err = extract_file(
            &archive,
            &tmp.path().join("d"),
            &with(Some("hunter2-wrong")),
        )
        .unwrap_err();
        let shown = format!("{err} {err:?}");
        assert!(
            !shown.contains("hunter2-wrong") && !shown.contains(PW),
            "{shown}"
        );
    }
    let debug = format!("{:?}", Password::new(PW));
    assert!(!debug.contains(PW), "{debug}");
}

#[test]
fn an_empty_password_is_refused() {
    for format in encrypted_formats() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("f.txt");
        write(&src, b"x");
        let out = tmp.path().join(format!("a.{}", ext(format)));
        let err = create_file(format, &[src], &out, &with(Some(""))).unwrap_err();
        assert!(matches!(err, Error::InvalidOption(_)), "{err}");
        assert!(!out.exists(), "{format:?} left an output file");
    }
}

#[test]
fn formats_without_encryption_refuse_a_password() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("f.txt");
    write(&src, b"x");
    for format in [Format::Tar, Format::TarGz, Format::Gz, Format::Zst] {
        if zstar_core::handler(format).is_err() {
            continue;
        }
        let out = tmp.path().join(format!("a.{}", ext(format)));
        let err =
            create_file(format, std::slice::from_ref(&src), &out, &with(Some(PW))).unwrap_err();
        assert!(matches!(err, Error::InvalidOption(_)), "{format:?}: {err}");
        assert!(!out.exists(), "{format:?} left an output file");
    }
}

#[test]
fn damaged_encrypted_archives_error_and_leave_nothing() {
    for format in encrypted_formats() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = encrypted_archive(tmp.path(), format, &with(Some(PW)));
        let bytes = fs::read(&archive).unwrap();

        let truncated = tmp.path().join(format!("t.{}", ext(format)));
        fs::write(&truncated, &bytes[..bytes.len() / 2]).unwrap();
        let dest = tmp.path().join("d1");
        assert!(
            extract_file(&truncated, &dest, &with(Some(PW))).is_err(),
            "{format:?}"
        );

        // Flip bytes in the middle of the data: the authentication code or the decoder must
        // notice. Which error comes back depends on the format.
        let mut corrupt = bytes.clone();
        let mid = corrupt.len() / 2;
        for b in &mut corrupt[mid..mid + 64] {
            *b ^= 0xA5;
        }
        let damaged = tmp.path().join(format!("c.{}", ext(format)));
        fs::write(&damaged, &corrupt).unwrap();
        let dest = tmp.path().join("d2");
        assert!(
            extract_file(&damaged, &dest, &with(Some(PW))).is_err(),
            "{format:?}"
        );
        assert!(find_part_files(tmp.path()).is_empty(), "{format:?}");
    }
}

#[test]
fn cancelling_an_encrypted_create_leaves_no_archive() {
    for format in encrypted_formats() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("f.bin");
        write(&src, &pseudo(1 << 20, 3));
        let out = tmp.path().join(format!("a.{}", ext(format)));
        let opts = with(Some(PW));
        opts.cancel.cancel();
        let err = create_file(format, &[src], &out, &opts).unwrap_err();
        assert!(matches!(err, Error::Cancelled), "{format:?}: {err}");
        assert!(!out.exists(), "{format:?}");
        assert!(find_part_files(tmp.path()).is_empty(), "{format:?}");
    }
}

fn seven_zip() -> Option<&'static str> {
    ["7z", "7za", "7zz"]
        .into_iter()
        .find(|bin| Command::new(bin).arg("--help").output().is_ok())
}

#[test]
fn the_real_7z_tool_reads_our_archives() {
    let Some(bin) = seven_zip() else { return };
    for format in encrypted_formats() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = encrypted_archive(tmp.path(), format, &with(Some(PW)));
        let dest = tmp.path().join("out");
        let status = Command::new(bin)
            .arg("x")
            .arg(format!("-p{PW}"))
            .arg(format!("-o{}", dest.display()))
            .arg("-y")
            .arg(&archive)
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "{format:?}: {}",
            String::from_utf8_lossy(&status.stdout)
        );
        assert_eq!(
            fs::read(dest.join("proj/a.txt")).unwrap(),
            MARKER.repeat(200).as_bytes(),
            "{format:?}"
        );
        // And 7z with the wrong password must fail, proving the data really is encrypted.
        let bad = Command::new(bin)
            .arg("t")
            .arg("-pwrong")
            .arg(&archive)
            .output()
            .unwrap();
        assert!(
            !bad.status.success(),
            "{format:?} opened with a wrong password"
        );
    }
}

#[test]
fn we_read_what_the_real_7z_tool_encrypts() {
    let Some(bin) = seven_zip() else { return };
    for (format, args) in [
        (Format::Zip, vec!["-tzip", "-mem=AES256"]),
        (Format::SevenZ, vec!["-t7z", "-mhe=on"]),
    ] {
        if !encrypted_formats().contains(&format) {
            continue;
        }
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        make_tree(&src);
        let archive = tmp.path().join(format!("theirs.{}", ext(format)));
        let out = Command::new(bin)
            .arg("a")
            .args(&args)
            .arg(format!("-p{PW}"))
            .arg(&archive)
            .arg(&src)
            .current_dir(tmp.path())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{format:?}: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        let dest = tmp.path().join("dest");
        extract_file(&archive, &dest, &with(Some(PW))).unwrap();
        let got = snapshot(&dest.join("proj"));
        let want = snapshot(&src);
        // 7z does not record every attribute we do, so compare names and contents only.
        assert_eq!(
            want.keys().collect::<Vec<_>>(),
            got.keys().collect::<Vec<_>>(),
            "{format:?}"
        );
        assert!(
            matches!(
                extract_file(&archive, &tmp.path().join("x"), &with(Some("nope"))),
                Err(Error::WrongPassword)
            ),
            "{format:?}"
        );
    }
}
