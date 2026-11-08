//! Zip compression methods: every method we can write round-trips and fails cleanly when
//! damaged; methods we can only read (LZMA, Deflate64) are checked against archives made by
//! 7-Zip, committed under `tests/data`.

mod common;

use std::fs;
use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;

use common::*;
use zstar_core::{
    Error, Format, LimitKind, Options, ZipMethod, create_file, extract, extract_file,
};

const METHODS: [ZipMethod; 4] = [
    ZipMethod::Stored,
    ZipMethod::Deflate,
    ZipMethod::Bzip2,
    ZipMethod::Zstd,
];

fn with(method: ZipMethod) -> Options {
    Options {
        zip_method: Some(method),
        threads: 2,
        ..Options::default()
    }
}

/// Methods this build can write (`None` when a method's feature is off).
fn writable(tmp: &Path, method: ZipMethod) -> Option<std::path::PathBuf> {
    let src = tmp.join("probe.txt");
    write(&src, b"x");
    let out = tmp.join("probe.zip");
    match create_file(Format::Zip, &[src], &out, &with(method)) {
        Ok(()) => {
            fs::remove_file(&out).unwrap();
            Some(out)
        }
        Err(Error::InvalidOption(m)) if m.contains("not enabled") => None,
        Err(e) => panic!("{method:?}: {e}"),
    }
}

fn fixture_text() -> String {
    (0..3000)
        .map(|i| format!("zstar fixture line {i}\n"))
        .collect()
}

#[test]
fn every_writable_method_round_trips_a_tree() {
    for method in METHODS {
        let tmp = tempfile::tempdir().unwrap();
        if writable(tmp.path(), method).is_none() {
            continue;
        }
        let src = tmp.path().join("proj");
        make_tree(&src);
        let archive = tmp.path().join("a.zip");
        create_file(
            Format::Zip,
            std::slice::from_ref(&src),
            &archive,
            &with(method),
        )
        .unwrap();
        assert!(find_part_files(tmp.path()).is_empty());
        let dest = tmp.path().join("dest");
        extract_file(&archive, &dest, &with(method)).unwrap();
        assert_eq!(snapshot(&src), snapshot(&dest.join("proj")), "{method:?}");
    }
}

#[test]
fn the_method_really_is_the_one_asked_for() {
    // Zip stores the method id (0 stored, 8 deflate, 12 bzip2, 93 zstd) in each local header.
    for (method, id) in [
        (ZipMethod::Stored, 0u16),
        (ZipMethod::Deflate, 8),
        (ZipMethod::Bzip2, 12),
        (ZipMethod::Zstd, 93),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        if writable(tmp.path(), method).is_none() {
            continue;
        }
        let src = tmp.path().join("f.txt");
        write(&src, &vec![b'a'; 50_000]);
        let archive = tmp.path().join("a.zip");
        create_file(Format::Zip, &[src], &archive, &with(method)).unwrap();
        let bytes = fs::read(&archive).unwrap();
        assert_eq!(&bytes[..4], b"PK\x03\x04");
        assert_eq!(u16::from_le_bytes([bytes[8], bytes[9]]), id, "{method:?}");
    }
}

#[test]
fn damaged_archives_fail_cleanly_for_every_method() {
    for method in METHODS {
        let tmp = tempfile::tempdir().unwrap();
        if writable(tmp.path(), method).is_none() {
            continue;
        }
        let src = tmp.path().join("big.bin");
        write(&src, &pseudo(2 << 20, 17));
        let archive = tmp.path().join("a.zip");
        create_file(Format::Zip, &[src], &archive, &with(method)).unwrap();
        let good = fs::read(&archive).unwrap();

        let mut truncated = good.clone();
        truncated.truncate(good.len() * 2 / 3);
        let mut corrupted = good.clone();
        corrupted[good.len() / 3] ^= 0xff;
        for (what, data) in [("truncated", truncated), ("corrupted", corrupted)] {
            let dest = tmp.path().join(format!("dest-{what}"));
            let r = extract(&mut Cursor::new(data), &dest, &Options::default());
            assert!(r.is_err(), "{method:?} {what} extracted successfully");
            assert!(
                !dest.join("big.bin").exists(),
                "{method:?} {what}: file kept"
            );
            assert!(
                find_part_files(&dest).is_empty(),
                "{method:?} {what}: .part left"
            );
        }
    }
}

#[test]
fn bombs_and_cancellation_behave_for_every_method() {
    for method in METHODS.into_iter().filter(|m| *m != ZipMethod::Stored) {
        let tmp = tempfile::tempdir().unwrap();
        if writable(tmp.path(), method).is_none() {
            continue;
        }
        let src = tmp.path().join("zeros");
        write(&src, &vec![0u8; 8 << 20]);
        let archive = tmp.path().join("a.zip");
        create_file(Format::Zip, &[src], &archive, &with(method)).unwrap();

        let mut o = with(method);
        o.limits.max_ratio = 10;
        o.limits.ratio_min_bytes = 1 << 20;
        let dest = tmp.path().join("ratio");
        let r = extract_file(&archive, &dest, &o);
        assert!(
            matches!(r, Err(Error::LimitExceeded(LimitKind::Ratio))),
            "{method:?}: {r:?}"
        );
        assert!(find_part_files(&dest).is_empty());

        let mut o = with(method);
        let token = o.control.clone();
        o.progress = Some(Arc::new(move |p| {
            if p.bytes_done > 256 * 1024 {
                token.cancel();
            }
        }));
        let dest = tmp.path().join("cancel");
        let r = extract_file(&archive, &dest, &o);
        assert!(matches!(r, Err(Error::Cancelled)), "{method:?}: {r:?}");
        assert!(!dest.join("zeros").exists());
        assert!(find_part_files(&dest).is_empty());
    }
}

#[test]
fn levels_are_validated_per_method() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("f.txt");
    write(&src, &vec![b'a'; 10_000]);
    let out = tmp.path().join("o.zip");
    let try_level = |method, level| {
        let o = Options {
            zip_method: Some(method),
            level,
            overwrite: true,
            ..Options::default()
        };
        create_file(Format::Zip, std::slice::from_ref(&src), &out, &o)
    };
    assert!(try_level(ZipMethod::Deflate, Some(9)).is_ok());
    assert!(try_level(ZipMethod::Deflate, Some(10)).is_err());
    assert!(try_level(ZipMethod::Stored, Some(5)).is_err());
    if writable(tmp.path(), ZipMethod::Bzip2).is_some() {
        assert!(try_level(ZipMethod::Bzip2, Some(9)).is_ok());
        assert!(try_level(ZipMethod::Bzip2, Some(0)).is_err());
    }
    if writable(tmp.path(), ZipMethod::Zstd).is_some() {
        assert!(try_level(ZipMethod::Zstd, Some(22)).is_ok());
        assert!(try_level(ZipMethod::Zstd, Some(23)).is_err());
    }
}

#[test]
fn lzma_and_deflate64_archives_from_7zip_extract() {
    for name in ["lzma.zip", "deflate64.zip"] {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data")
            .join(name);
        let tmp = tempfile::tempdir().unwrap();
        let r = extract_file(&path, tmp.path(), &Options::default());
        if cfg!(all(feature = "zip-lzma", feature = "zip-deflate64")) {
            r.unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(
                fs::read_to_string(tmp.path().join("fixture.txt")).unwrap(),
                fixture_text(),
                "{name}"
            );
        } else if r.is_ok() {
            assert_eq!(
                fs::read_to_string(tmp.path().join("fixture.txt")).unwrap(),
                fixture_text()
            );
        }
        assert!(find_part_files(tmp.path()).is_empty());
    }
}

#[test]
fn a_method_we_do_not_support_fails_with_an_error_and_no_leftovers() {
    // PPMd is deliberately not built in.
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/ppmd.zip");
    let tmp = tempfile::tempdir().unwrap();
    let r = extract_file(&path, tmp.path(), &Options::default());
    assert!(r.is_err(), "{r:?}");
    assert!(!tmp.path().join("fixture.txt").exists());
    assert!(find_part_files(tmp.path()).is_empty());
}

/// The parallel window must give the same bytes as the serial path, also for files that
/// straddle the size where an entry is written to the compressor in pieces, and for a tree
/// that mixes empty files, directories, small and mid-size files in one order.
#[test]
fn threads_do_not_change_the_archive_bytes_of_a_mixed_tree() {
    for method in [ZipMethod::Deflate, ZipMethod::Zstd] {
        let tmp = tempfile::tempdir().unwrap();
        if writable(tmp.path(), method).is_none() {
            continue;
        }
        let src = tmp.path().join("mix");
        let sizes = [
            0usize,
            5,
            4096,
            (1 << 20) - 1,
            1 << 20,
            (1 << 20) + 1,
            3 << 20,
            70,
            2 << 20,
            9,
        ];
        for (i, n) in sizes.iter().enumerate() {
            let body: Vec<u8> = (0..*n)
                .map(|k| ((k * 7 + k / 251 + i) % 251) as u8)
                .collect();
            write(&src.join(format!("d{}/f{i}.bin", i % 3)), &body);
        }
        let make = |threads: usize, name: &str| {
            let out = tmp.path().join(name);
            let opts = Options {
                zip_method: Some(method),
                threads,
                ..Options::default()
            };
            create_file(Format::Zip, std::slice::from_ref(&src), &out, &opts).unwrap();
            fs::read(out).unwrap()
        };
        let serial = make(1, "serial.zip");
        assert_eq!(serial, make(4, "parallel.zip"), "{method:?}");
        assert_eq!(serial, make(2, "two.zip"), "{method:?}");
    }
}
