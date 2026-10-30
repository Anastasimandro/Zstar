mod common;

use std::fs;
use std::io::Cursor;
use std::sync::Arc;

use common::*;
use zstar_core::{Error, Format, Options, create_file, extract, extract_file, handler, list};

fn zip_available() -> bool {
    handler(Format::Zip).is_ok()
}

#[test]
fn cp437_names_are_decoded() {
    if !zip_available() {
        return;
    }
    // 0x82 is 'é' and 0x94 is 'ö' in IBM code page 437; the UTF-8 flag is off.
    let mut z = RawZip::new();
    z.add(
        &[b'd', 0x82, b'/', 0x94, b'.', b't', b'x', b't'],
        b"cp437",
        Some(0o100_644),
        0,
    );
    let data = z.finish();

    let mut names = Vec::new();
    list(
        &mut Cursor::new(data.clone()),
        &Options::default(),
        &mut |e| names.push(e.name),
    )
    .unwrap();
    assert_eq!(names, ["dé/ö.txt"]);

    let tmp = tempfile::tempdir().unwrap();
    extract(&mut Cursor::new(data), tmp.path(), &Options::default()).unwrap();
    assert_eq!(fs::read(tmp.path().join("dé/ö.txt")).unwrap(), b"cp437");
}

#[test]
fn utf8_flagged_zip_names_are_kept() {
    if !zip_available() {
        return;
    }
    let mut z = RawZip::new();
    z.file("日本語/ü.txt", b"utf8");
    let tmp = tempfile::tempdir().unwrap();
    extract(
        &mut Cursor::new(z.finish()),
        tmp.path(),
        &Options::default(),
    )
    .unwrap();
    assert_eq!(fs::read(tmp.path().join("日本語/ü.txt")).unwrap(), b"utf8");
}

#[test]
fn duplicate_zip_names_are_rejected() {
    if !zip_available() {
        return;
    }
    let mut z = RawZip::new();
    z.file("same.txt", b"one").file("same.txt", b"two");
    let tmp = tempfile::tempdir().unwrap();
    let r = extract(
        &mut Cursor::new(z.finish()),
        tmp.path(),
        &Options::default(),
    );
    assert!(matches!(r, Err(Error::InvalidArchive(_))), "{r:?}");
}

#[test]
fn names_differing_only_by_case_extract_safely_with_many_threads() {
    if !zip_available() {
        return;
    }
    let mut z = RawZip::new();
    z.file("A.txt", &pseudo(300_000, 1))
        .file("a.txt", &pseudo(300_000, 2));
    let tmp = tempfile::tempdir().unwrap();
    let o = Options {
        threads: 8,
        ..Options::default()
    };
    let r = extract(&mut Cursor::new(z.finish()), tmp.path(), &o);
    let probe = tmp.path().join("probe");
    fs::write(&probe, b"").unwrap();
    if tmp.path().join("PROBE").exists() {
        // Case-insensitive file system: the second name must not silently replace the first.
        assert!(r.is_err(), "{r:?}");
        assert_eq!(
            fs::read(tmp.path().join("A.txt")).unwrap(),
            pseudo(300_000, 1)
        );
        return;
    }
    r.unwrap();
    assert_eq!(
        fs::read(tmp.path().join("A.txt")).unwrap(),
        pseudo(300_000, 1)
    );
    assert_eq!(
        fs::read(tmp.path().join("a.txt")).unwrap(),
        pseudo(300_000, 2)
    );
}

#[test]
fn corrupt_zip_data_fails_cleanly() {
    if !zip_available() {
        return;
    }
    let mut z = RawZip::new();
    z.file("a.txt", b"0123456789");
    let mut data = z.finish();
    let at = data.windows(10).position(|w| w == b"0123456789").unwrap();
    data[at + 3] ^= 0xff; // the stored CRC no longer matches
    let tmp = tempfile::tempdir().unwrap();
    let r = extract(&mut Cursor::new(data), tmp.path(), &Options::default());
    assert!(r.is_err(), "CRC mismatch must be an error");
    assert!(!tmp.path().join("a.txt").exists());
    assert!(find_part_files(tmp.path()).is_empty());
}

#[test]
fn truncated_archives_fail_cleanly() {
    for f in all_formats() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        write(&src.join("big.bin"), &pseudo(2 << 20, 9));
        let archive = tmp.path().join(format!("a.{}", ext(f)));
        create_file(f, &[src], &archive, &Options::default()).unwrap();
        let mut data = fs::read(&archive).unwrap();
        data.truncate(data.len() * 2 / 3);

        let dest = tmp.path().join("dest");
        let r = extract(&mut Cursor::new(data), &dest, &Options::default());
        assert!(
            r.is_err(),
            "{f:?} truncated archive must not extract successfully"
        );
        assert!(
            !dest.join("proj/big.bin").exists(),
            "{f:?}: incomplete file was kept"
        );
        assert!(
            find_part_files(&dest).is_empty(),
            "{f:?}: leftover .part file"
        );
    }
}

#[test]
fn unknown_and_empty_input_is_unsupported() {
    let tmp = tempfile::tempdir().unwrap();
    for data in [
        b"just some text, not an archive".to_vec(),
        Vec::new(),
        vec![0u8; 2048],
    ] {
        let r = extract(&mut Cursor::new(data), tmp.path(), &Options::default());
        assert!(matches!(r, Err(Error::UnsupportedFormat(_))), "{r:?}");
    }
}

#[test]
fn cancelling_mid_extraction_leaves_no_part_files_and_no_partial_file() {
    for f in all_formats() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        write(&src.join("big.bin"), &pseudo(4 << 20, 5));
        let archive = tmp.path().join(format!("a.{}", ext(f)));
        create_file(f, &[src], &archive, &Options::default()).unwrap();

        let mut o = Options {
            threads: 2,
            ..Options::default()
        };
        let token = o.cancel.clone();
        o.progress = Some(Arc::new(move |p| {
            if p.bytes_done > 256 * 1024 {
                token.cancel();
            }
        }));
        let dest = tmp.path().join("dest");
        let r = extract_file(&archive, &dest, &o);
        assert!(matches!(r, Err(Error::Cancelled)), "{f:?}: {r:?}");
        assert!(!dest.join("proj/big.bin").exists(), "{f:?}");
        assert!(find_part_files(&dest).is_empty(), "{f:?}");
    }
}

#[test]
fn cancelling_creation_leaves_no_output() {
    for f in all_formats() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        write(&src.join("big.bin"), &pseudo(4 << 20, 5));
        let out = tmp.path().join(format!("a.{}", ext(f)));

        let mut o = Options::default();
        let token = o.cancel.clone();
        o.progress = Some(Arc::new(move |p| {
            if p.bytes_done > 256 * 1024 {
                token.cancel();
            }
        }));
        let r = create_file(f, &[src], &out, &o);
        assert!(matches!(r, Err(Error::Cancelled)), "{f:?}: {r:?}");
        assert!(!out.exists(), "{f:?}");
        assert!(find_part_files(tmp.path()).is_empty(), "{f:?}");
    }
}

#[test]
fn extension_does_not_decide_the_format() {
    // A zip renamed to .tar.gz still extracts as a zip: detection uses magic bytes only.
    for f in all_formats() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        write(&src.join("a.txt"), b"magic");
        let archive = tmp.path().join(format!("a.{}", ext(f)));
        create_file(f, &[src], &archive, &Options::default()).unwrap();
        let misnamed = tmp.path().join("misnamed.bin");
        fs::rename(&archive, &misnamed).unwrap();
        extract_file(&misnamed, &tmp.path().join("d"), &Options::default()).unwrap();
        assert_eq!(fs::read(tmp.path().join("d/proj/a.txt")).unwrap(), b"magic");
    }
}
