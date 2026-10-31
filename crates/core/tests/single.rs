//! Single-file formats (gz, zst): round trip, damaged input, limits, cancellation.

mod common;

use std::fs;
use std::io::Cursor;
use std::sync::Arc;

use common::*;
use zstar_core::{
    Error, Format, LimitKind, Options, create_file, detect, extract, extract_file, list_file,
};

fn make(format: Format, dir: &std::path::Path, name: &str, data: &[u8]) -> std::path::PathBuf {
    let src = dir.join(name);
    write(&src, data);
    let archive = dir.join(format!("{name}.{}", ext(format)));
    create_file(
        format,
        std::slice::from_ref(&src),
        &archive,
        &Options::default(),
    )
    .unwrap();
    archive
}

#[test]
fn round_trip_names_the_output_after_the_archive() {
    for f in single_formats() {
        let tmp = tempfile::tempdir().unwrap();
        let data = pseudo(3 << 20, 11);
        let archive = make(f, tmp.path(), "report.bin", &data);
        assert!(find_part_files(tmp.path()).is_empty(), "{f:?}");
        if f == Format::Br {
            // No signature: detection by content alone must say so, not guess.
            let r = detect(&mut fs::File::open(&archive).unwrap());
            assert!(matches!(r, Err(Error::UnsupportedFormat(_))), "{r:?}");
        } else {
            assert_eq!(detect(&mut fs::File::open(&archive).unwrap()).unwrap(), f);
        }

        let dest = tmp.path().join("out");
        let s = extract_file(&archive, &dest, &Options::default()).unwrap();
        assert_eq!(fs::read(dest.join("report.bin")).unwrap(), data, "{f:?}");
        assert_eq!((s.files, s.bytes), (1, data.len() as u64));
        assert!(find_part_files(&dest).is_empty());

        let mut listed = Vec::new();
        let detected = list_file(&archive, &Options::default(), &mut |e| listed.push(e)).unwrap();
        assert_eq!(detected, f);
        assert_eq!(listed.len(), 1);
        assert_eq!(
            (listed[0].name.as_str(), listed[0].size),
            ("report.bin", data.len() as u64)
        );
        assert!(listed[0].compressed_size.is_some());
    }
}

#[test]
fn empty_and_compressible_files_round_trip() {
    for f in single_formats() {
        for (name, data) in [("empty", Vec::new()), ("zeros", vec![0u8; 2 << 20])] {
            let tmp = tempfile::tempdir().unwrap();
            let archive = make(f, tmp.path(), name, &data);
            let dest = tmp.path().join("out");
            extract_file(&archive, &dest, &Options::default()).unwrap();
            assert_eq!(fs::read(dest.join(name)).unwrap(), data, "{f:?} {name}");
        }
    }
}

#[test]
fn streams_use_the_hint_or_a_default_name() {
    for f in single_formats() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = make(f, tmp.path(), "a.txt", b"hello");
        let bytes = fs::read(&archive).unwrap();

        let dest = tmp.path().join("no-hint");
        let r = extract(&mut Cursor::new(bytes.clone()), &dest, &Options::default());
        if f == Format::Br {
            assert!(matches!(r, Err(Error::UnsupportedFormat(_))), "{r:?}");
        } else {
            r.unwrap();
            assert_eq!(fs::read(dest.join("data")).unwrap(), b"hello");
        }

        let dest = tmp.path().join("hint");
        let opts = Options {
            name_hint: Some(format!("x.{}", ext(f))),
            ..Options::default()
        };
        extract(&mut Cursor::new(bytes), &dest, &opts).unwrap();
        assert_eq!(fs::read(dest.join("x")).unwrap(), b"hello");
    }
}

#[test]
fn an_archive_without_a_known_extension_does_not_overwrite_itself() {
    for f in single_formats() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = make(f, tmp.path(), "a.txt", b"hello");
        let renamed = tmp.path().join("blob");
        fs::rename(&archive, &renamed).unwrap();
        if f == Format::Br {
            let r = extract_file(&renamed, tmp.path(), &Options::default());
            assert!(matches!(r, Err(Error::UnsupportedFormat(_))), "{r:?}");
            continue;
        }
        extract_file(&renamed, tmp.path(), &Options::default()).unwrap();
        assert_eq!(fs::read(tmp.path().join("blob.out")).unwrap(), b"hello");
        assert_eq!(
            detect(&mut fs::File::open(&renamed).unwrap()).unwrap(),
            f,
            "the archive itself is untouched"
        );
    }
}

#[test]
fn several_inputs_or_a_directory_are_refused() {
    for f in single_formats() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("d/a.txt"), b"a");
        write(&tmp.path().join("b.txt"), b"b");
        let out = tmp.path().join(format!("o.{}", ext(f)));
        for inputs in [
            vec![tmp.path().join("d")],
            vec![tmp.path().join("d/a.txt"), tmp.path().join("b.txt")],
        ] {
            let r = create_file(f, &inputs, &out, &Options::default());
            assert!(matches!(r, Err(Error::InvalidOption(_))), "{f:?}: {r:?}");
            assert!(!out.exists());
            assert!(find_part_files(tmp.path()).is_empty());
        }
    }
}

#[test]
fn truncated_and_corrupted_streams_fail_cleanly() {
    for f in single_formats() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = make(f, tmp.path(), "big.bin", &pseudo(2 << 20, 3));
        let good = fs::read(&archive).unwrap();

        let mut truncated = good.clone();
        truncated.truncate(good.len() * 2 / 3);
        let mut corrupted = good.clone();
        let mid = good.len() / 2;
        corrupted[mid] ^= 0xff;
        // Damage to the trailer (CRC / checksum) must be caught too, not only the middle.
        let mut bad_trailer = good.clone();
        let last = good.len() - 1;
        bad_trailer[last] ^= 0xff;

        for (what, data) in [
            ("truncated", truncated),
            ("corrupted", corrupted),
            ("bad trailer", bad_trailer),
        ] {
            let dest = tmp.path().join(format!("dest-{what}"));
            let r = extract(&mut Cursor::new(data), &dest, &Options::default());
            assert!(r.is_err(), "{f:?} {what} must not extract successfully");
            assert!(
                !dest.join("data").exists(),
                "{f:?} {what}: partial file kept"
            );
            assert!(
                find_part_files(&dest).is_empty(),
                "{f:?} {what}: .part left"
            );
        }
    }
}

#[test]
fn bombs_hit_the_size_and_ratio_limits() {
    for f in single_formats() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = make(f, tmp.path(), "zeros", &vec![0u8; 8 << 20]);

        let mut o = Options::default();
        o.limits.max_total_bytes = 1 << 20;
        let dest = tmp.path().join("size");
        let r = extract_file(&archive, &dest, &o);
        assert!(
            matches!(r, Err(Error::LimitExceeded(LimitKind::TotalSize))),
            "{f:?}: {r:?}"
        );
        assert!(!dest.join("zeros").exists());
        assert!(find_part_files(&dest).is_empty());

        let mut o = Options::default();
        o.limits.max_ratio = 10;
        o.limits.ratio_min_bytes = 1 << 20;
        let dest = tmp.path().join("ratio");
        let r = extract_file(&archive, &dest, &o);
        assert!(
            matches!(r, Err(Error::LimitExceeded(LimitKind::Ratio))),
            "{f:?}: {r:?}"
        );
        assert!(!dest.join("zeros").exists());
        assert!(find_part_files(&dest).is_empty());

        // Listing measures the size by decompressing, so it obeys the size limit too.
        let mut o = Options::default();
        o.limits.max_total_bytes = 1 << 20;
        let r = list_file(&archive, &o, &mut |_| {});
        assert!(
            matches!(r, Err(Error::LimitExceeded(LimitKind::TotalSize))),
            "{f:?}: {r:?}"
        );
    }
}

#[test]
fn cancelling_leaves_no_part_file_and_no_output() {
    for f in single_formats() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = make(f, tmp.path(), "big.bin", &pseudo(4 << 20, 5));

        let mut o = Options::default();
        let token = o.cancel.clone();
        o.progress = Some(Arc::new(move |p| {
            if p.bytes_done > 256 * 1024 {
                token.cancel();
            }
        }));
        let dest = tmp.path().join("dest");
        let r = extract_file(&archive, &dest, &o);
        assert!(matches!(r, Err(Error::Cancelled)), "{f:?}: {r:?}");
        assert!(!dest.join("big.bin").exists());
        assert!(find_part_files(&dest).is_empty());

        // Cancelling while compressing leaves neither the archive nor its .part file.
        let src = tmp.path().join("big.bin");
        let out = tmp.path().join(format!("again.{}", ext(f)));
        let mut o = Options::default();
        let token = o.cancel.clone();
        o.progress = Some(Arc::new(move |p| {
            if p.bytes_done > 256 * 1024 {
                token.cancel();
            }
        }));
        let r = create_file(f, std::slice::from_ref(&src), &out, &o);
        assert!(matches!(r, Err(Error::Cancelled)), "{f:?}: {r:?}");
        assert!(!out.exists());
        assert!(find_part_files(tmp.path()).is_empty());
    }
}

#[test]
fn levels_are_validated() {
    for f in single_formats() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("f.txt");
        write(&src, &vec![b'a'; 100_000]);
        let out = tmp.path().join(format!("o.{}", ext(f)));
        let Some((lo, hi)) = level_range(f) else {
            let o = Options {
                level: Some(3),
                ..Options::default()
            };
            let r = create_file(f, std::slice::from_ref(&src), &out, &o);
            assert!(matches!(r, Err(Error::InvalidOption(_))), "{f:?}: {r:?}");
            assert!(!out.exists());
            continue;
        };
        for (level, ok) in [(lo, true), (hi, true), (lo - 1, false), (hi + 1, false)] {
            let o = Options {
                level: Some(level),
                overwrite: true,
                ..Options::default()
            };
            let r = create_file(f, std::slice::from_ref(&src), &out, &o);
            assert_eq!(r.is_ok(), ok, "{f:?} level {level}: {r:?}");
        }
    }
}

#[test]
fn existing_output_is_kept_unless_overwrite_is_on() {
    for f in single_formats() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = make(f, tmp.path(), "a.txt", b"new");
        let dest = tmp.path().join("dest");
        write(&dest.join("a.txt"), b"old");
        let r = extract_file(&archive, &dest, &Options::default());
        assert!(matches!(r, Err(Error::AlreadyExists(_))), "{f:?}: {r:?}");
        assert_eq!(fs::read(dest.join("a.txt")).unwrap(), b"old");
        let o = Options {
            overwrite: true,
            ..Options::default()
        };
        extract_file(&archive, &dest, &o).unwrap();
        assert_eq!(fs::read(dest.join("a.txt")).unwrap(), b"new");
    }
}

#[test]
fn concatenated_members_extract_as_one_file() {
    // `cat a.gz b.gz` is a valid gzip file, and likewise for zstd frames.
    for f in single_formats().into_iter().filter(|&f| f != Format::Br) {
        let tmp = tempfile::tempdir().unwrap();
        let a = make(f, tmp.path(), "a", b"first-");
        let b = make(f, tmp.path(), "b", b"second");
        let mut joined = fs::read(a).unwrap();
        joined.extend(fs::read(b).unwrap());
        let dest = tmp.path().join("out");
        extract(&mut Cursor::new(joined), &dest, &Options::default()).unwrap();
        assert_eq!(
            fs::read(dest.join("data")).unwrap(),
            b"first-second",
            "{f:?}"
        );
    }
}
