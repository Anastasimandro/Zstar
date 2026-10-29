mod common;

use std::fs;
use std::io::Cursor;
use std::path::Path;

use common::*;
use zstar_core::{Error, Format, LimitKind, Limits, Options, create_file, extract, handler};

fn zip_available() -> bool {
    handler(Format::Zip).is_ok()
}

fn extract_bytes(data: Vec<u8>, dest: &Path, opts: &Options) -> Result<zstar_core::Summary, Error> {
    extract(&mut Cursor::new(data), dest, opts)
}

fn names_for(outside: &Path) -> Vec<String> {
    let abs = outside.join("evil.txt").to_string_lossy().into_owned();
    vec![
        "../evil.txt".into(),
        "a/../../evil.txt".into(),
        "a/b/../../../evil.txt".into(),
        "..\\evil.txt".into(),
        "a\\..\\..\\evil.txt".into(),
        abs,
        "C:\\evil.txt".into(),
        "/evil.txt".into(),
    ]
}

#[test]
fn zip_slip_is_rejected_before_anything_is_written() {
    if !zip_available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let outside = tmp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    for bad in names_for(&outside) {
        let mut z = RawZip::new();
        z.file("fine.txt", b"fine").file(&bad, b"pwned");
        let dest = tmp.path().join("dest");
        let r = extract_bytes(
            z.finish(),
            &dest,
            &Options {
                threads: 4,
                ..Options::default()
            },
        );
        assert!(
            matches!(r, Err(Error::UnsafePath { .. })),
            "{bad:?} gave {r:?}"
        );
        assert!(
            !dest.exists(),
            "{bad:?}: zip validates every name up front, so nothing is created"
        );
        assert!(!tmp.path().join("evil.txt").exists());
        assert!(!outside.join("evil.txt").exists());
    }
}

#[test]
fn tar_slip_is_rejected_and_nothing_escapes() {
    let tmp = tempfile::tempdir().unwrap();
    let outside = tmp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    for bad in names_for(&outside) {
        // The path inside a tar header field is limited to 100 bytes.
        if bad.len() > 100 {
            continue;
        }
        let tar = raw_tar(&[tar_file("fine.txt", b"fine"), tar_file(&bad, b"pwned")]);
        let dest = tmp.path().join("dest");
        let r = extract_bytes(tar, &dest, &Options::default());
        assert!(
            matches!(r, Err(Error::UnsafePath { .. })),
            "{bad:?} gave {r:?}"
        );
        assert!(!tmp.path().join("evil.txt").exists());
        assert!(!outside.join("evil.txt").exists());
        assert!(find_part_files(tmp.path()).is_empty());
        let _ = fs::remove_dir_all(&dest);
    }
}

#[test]
fn windows_reserved_names_are_rejected_when_the_rule_is_on() {
    let tmp = tempfile::tempdir().unwrap();
    let on = Options {
        windows_names: true,
        ..Options::default()
    };
    for bad in [
        "aux.txt",
        "dir/CON",
        "com1",
        "x/lpt9.log",
        "a:b",
        "trailing.",
        "q?.txt",
    ] {
        let tar = raw_tar(&[tar_file(bad, b"x")]);
        let r = extract_bytes(tar, &tmp.path().join("t"), &on);
        assert!(
            matches!(r, Err(Error::UnsafePath { .. })),
            "tar {bad:?}: {r:?}"
        );
        if zip_available() {
            let mut z = RawZip::new();
            z.file(bad, b"x");
            let r = extract_bytes(z.finish(), &tmp.path().join("z"), &on);
            assert!(
                matches!(r, Err(Error::UnsafePath { .. })),
                "zip {bad:?}: {r:?}"
            );
        }
    }
    // The same names are fine where the rule is off (the Unix default).
    let off = Options {
        windows_names: false,
        ..Options::default()
    };
    extract_bytes(
        raw_tar(&[tar_file("aux.txt", b"x")]),
        &tmp.path().join("ok"),
        &off,
    )
    .unwrap();
}

#[cfg(unix)]
mod links {
    use super::*;

    fn dest_and_outside() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let dest = tmp.path().join("dest");
        (tmp, dest, outside)
    }

    #[test]
    fn symlink_then_write_through_it_cannot_escape() {
        let (tmp, dest, outside) = dest_and_outside();
        for target in [
            outside.to_string_lossy().into_owned(),
            "../outside".to_owned(),
        ] {
            let tar = raw_tar(&[
                tar_symlink("link", &target),
                tar_file("link/pwned.txt", b"x"),
            ]);
            let r = extract_bytes(tar, &dest, &Options::default());
            assert!(r.is_err(), "{target}: {r:?}");
            assert!(
                !outside.join("pwned.txt").exists(),
                "{target}: wrote through the link"
            );
            let _ = fs::remove_dir_all(&dest);
        }
        assert!(find_part_files(tmp.path()).is_empty());
    }

    #[test]
    fn symlinks_resolving_outside_are_removed_and_reported() {
        let (_tmp, dest, outside) = dest_and_outside();
        for target in [
            outside.to_string_lossy().into_owned(),
            "../outside".to_owned(),
            "../nonexistent".to_owned(),
        ] {
            let r = extract_bytes(
                raw_tar(&[tar_symlink("l", &target)]),
                &dest,
                &Options::default(),
            );
            assert!(
                matches!(r, Err(Error::UnsafeLink { .. })),
                "{target}: {r:?}"
            );
            assert!(
                fs::symlink_metadata(dest.join("l")).is_err(),
                "{target}: link was left behind"
            );
            let _ = fs::remove_dir_all(&dest);
        }
    }

    #[test]
    fn symlink_chains_cannot_climb_out() {
        let (_tmp, dest, _outside) = dest_and_outside();
        // a -> .   and   b -> a/../..   resolves two levels above the destination.
        let tar = raw_tar(&[tar_symlink("a", "."), tar_symlink("b", "a/../..")]);
        let r = extract_bytes(tar, &dest, &Options::default());
        assert!(matches!(r, Err(Error::UnsafeLink { .. })), "{r:?}");
        assert!(fs::symlink_metadata(dest.join("b")).is_err());
    }

    #[test]
    fn harmless_relative_symlinks_are_kept() {
        let (_tmp, dest, _outside) = dest_and_outside();
        let tar = raw_tar(&[
            tar_file("top.txt", b"top"),
            tar_symlink("d/up", "../top.txt"),
            tar_symlink("d/dangling", "not/there"),
        ]);
        let s = extract_bytes(tar, &dest, &Options::default()).unwrap();
        assert_eq!(s.symlinks, 2);
        assert_eq!(fs::read(dest.join("d/up")).unwrap(), b"top");
    }

    #[test]
    fn zip_symlinks_get_the_same_checks() {
        if !zip_available() {
            return;
        }
        let (_tmp, dest, outside) = dest_and_outside();
        let mut z = RawZip::new();
        z.symlink("l", &outside.to_string_lossy());
        let r = extract_bytes(z.finish(), &dest, &Options::default());
        assert!(matches!(r, Err(Error::UnsafeLink { .. })), "{r:?}");
        assert!(fs::symlink_metadata(dest.join("l")).is_err());
    }

    #[test]
    fn hard_links_cannot_reach_outside_or_missing_files() {
        let (_tmp, dest, outside) = dest_and_outside();
        fs::write(outside.join("secret"), b"s").unwrap();
        let abs = outside.join("secret").to_string_lossy().into_owned();
        for target in ["../outside/secret", abs.as_str()] {
            let r = extract_bytes(
                raw_tar(&[tar_hardlink("h", target)]),
                &dest,
                &Options::default(),
            );
            assert!(
                matches!(r, Err(Error::UnsafePath { .. })),
                "{target}: {r:?}"
            );
        }
        let r = extract_bytes(
            raw_tar(&[tar_hardlink("h", "missing")]),
            &dest,
            &Options::default(),
        );
        assert!(matches!(r, Err(Error::UnsafeLink { .. })), "{r:?}");
        assert!(!dest.join("h").exists());

        let ok = raw_tar(&[tar_file("a", b"data"), tar_hardlink("b", "a")]);
        extract_bytes(ok, &dest, &Options::default()).unwrap();
        assert_eq!(fs::read(dest.join("b")).unwrap(), b"data");
    }
}

fn limits(f: impl FnOnce(&mut Limits)) -> Options {
    let mut o = Options::default();
    f(&mut o.limits);
    o
}

#[test]
fn bombs_are_stopped_by_total_size_in_every_format() {
    for f in all_formats() {
        if f == Format::Tar {
            continue; // uncompressed: not a bomb
        }
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("bomb");
        write(&src.join("zeros.bin"), &vec![0u8; 32 << 20]);
        let archive = tmp.path().join(format!("bomb.{}", ext(f)));
        create_file(f, &[src], &archive, &Options::default()).unwrap();
        assert!(
            fs::metadata(&archive).unwrap().len() < 1 << 20,
            "zeros should compress well"
        );

        let dest = tmp.path().join("dest");
        let o = limits(|l| l.max_total_bytes = 8 << 20);
        let r = zstar_core::extract_file(&archive, &dest, &o);
        assert!(
            matches!(r, Err(Error::LimitExceeded(LimitKind::TotalSize))),
            "{f:?}: {r:?}"
        );
        assert!(
            !dest.join("bomb/zeros.bin").exists(),
            "{f:?}: partial file must not be kept"
        );
        assert!(
            find_part_files(&dest).is_empty(),
            "{f:?}: .part must be cleaned up"
        );
    }
}

#[test]
fn bombs_are_stopped_by_compression_ratio_in_every_format() {
    for f in all_formats() {
        if f == Format::Tar {
            continue; // uncompressed: not a bomb
        }
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("bomb");
        write(&src.join("zeros.bin"), &vec![0u8; 32 << 20]);
        let archive = tmp.path().join(format!("bomb.{}", ext(f)));
        create_file(f, &[src], &archive, &Options::default()).unwrap();

        let dest = tmp.path().join("dest");
        let o = limits(|l| {
            l.max_ratio = 50;
            l.ratio_min_bytes = 1 << 20;
        });
        let r = zstar_core::extract_file(&archive, &dest, &o);
        assert!(
            matches!(r, Err(Error::LimitExceeded(LimitKind::Ratio))),
            "{f:?}: {r:?}"
        );
        assert!(find_part_files(&dest).is_empty(), "{f:?}");
        assert!(!dest.join("bomb/zeros.bin").exists(), "{f:?}");
    }
}

#[test]
fn ordinary_data_passes_the_default_limits() {
    for f in all_formats() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("ok");
        write(&src.join("random.bin"), &pseudo(2 << 20, 3));
        let archive = tmp.path().join(format!("ok.{}", ext(f)));
        create_file(f, &[src], &archive, &Options::default()).unwrap();
        zstar_core::extract_file(&archive, &tmp.path().join("d"), &Options::default()).unwrap();
    }
}

#[test]
fn entry_count_limit_is_enforced() {
    let o = limits(|l| l.max_entries = 10);
    let names: Vec<String> = (0..50).map(|i| format!("f{i}")).collect();
    let tmp = tempfile::tempdir().unwrap();

    let entries: Vec<_> = names.iter().map(|n| tar_file(n, b"x")).collect();
    let r = extract_bytes(raw_tar(&entries), &tmp.path().join("t"), &o);
    assert!(
        matches!(r, Err(Error::LimitExceeded(LimitKind::Entries))),
        "{r:?}"
    );

    if zip_available() {
        let mut z = RawZip::new();
        for n in &names {
            z.file(n, b"x");
        }
        let r = extract_bytes(z.finish(), &tmp.path().join("z"), &o);
        assert!(
            matches!(r, Err(Error::LimitExceeded(LimitKind::Entries))),
            "{r:?}"
        );
        assert!(
            !tmp.path().join("z").exists(),
            "zip checks the count before writing"
        );
    }
}

#[test]
fn unlimited_really_disables_the_checks() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("z");
    write(&src.join("zeros.bin"), &vec![0u8; 4 << 20]);
    let archive = tmp.path().join("z.tar.zst");
    if handler(Format::TarZst).is_err() {
        return;
    }
    create_file(Format::TarZst, &[src], &archive, &Options::default()).unwrap();
    let o = Options {
        limits: Limits::unlimited(),
        ..Options::default()
    };
    zstar_core::extract_file(&archive, &tmp.path().join("d"), &o).unwrap();
}
