mod common;

use std::fs;
use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

use common::*;
use zstar_core::{
    EntryKind, Error, Format, Options, collect_sources, create_file, detect, extract_file,
    list_file,
};

fn opts(threads: usize) -> Options {
    Options {
        threads,
        ..Options::default()
    }
}

fn mtime_secs(p: &Path) -> u64 {
    fs::metadata(p)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn round_trip(format: Format, threads: usize) {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("proj");
    make_tree(&src);
    let archive = tmp.path().join(format!("out.{}", ext(format)));
    let dest = tmp.path().join("dest");

    create_file(format, std::slice::from_ref(&src), &archive, &opts(threads)).unwrap();
    assert!(archive.exists());
    assert!(
        find_part_files(tmp.path()).is_empty(),
        "no .part files after create"
    );
    assert_eq!(
        detect(&mut fs::File::open(&archive).unwrap()).unwrap(),
        format
    );

    // list agrees with what was collected
    let mut listed = Vec::new();
    let detected = list_file(&archive, &opts(threads), &mut |e| listed.push(e)).unwrap();
    assert_eq!(detected, format);
    let sources =
        collect_sources(std::slice::from_ref(&src), std::slice::from_ref(&archive)).unwrap();
    assert_eq!(listed.len(), sources.len());
    let big = listed
        .iter()
        .find(|e| e.name.ends_with("deep.bin"))
        .unwrap();
    assert_eq!((big.kind, big.size), (EntryKind::File, 5 << 20));

    // extract, with progress reporting
    let max_bytes = Arc::new(AtomicU64::new(0));
    let seen = Arc::clone(&max_bytes);
    let mut o = opts(threads);
    o.progress = Some(Arc::new(move |p| {
        seen.fetch_max(p.bytes_done, Ordering::Relaxed);
    }));
    let summary = extract_file(&archive, &dest, &o).unwrap();

    assert_eq!(
        snapshot(&src),
        snapshot(&dest.join("proj")),
        "{format:?} threads={threads}"
    );
    assert!(
        find_part_files(&dest).is_empty(),
        "no .part files after extract"
    );
    assert_eq!(max_bytes.load(Ordering::Relaxed), summary.bytes);
    let expected_bytes: u64 = sources.iter().map(|s| s.size).sum();
    assert_eq!(summary.bytes, expected_bytes);
    assert_eq!(summary.skipped, 0);

    // mtime survives (zip stores 2-second resolution)
    let (a, b) = (
        mtime_secs(&src.join("top.txt")),
        mtime_secs(&dest.join("proj/top.txt")),
    );
    assert!(a.abs_diff(b) <= 2, "mtime {a} vs {b}");
}

#[test]
fn round_trip_every_format_single_threaded() {
    for f in all_formats() {
        round_trip(f, 1);
    }
}

#[test]
fn round_trip_every_format_multi_threaded() {
    for f in all_formats() {
        round_trip(f, 4);
    }
}

#[test]
fn at_least_the_four_first_formats_are_built_in() {
    assert_eq!(all_formats(), Format::ALL.to_vec());
}

#[test]
fn archives_made_with_different_thread_counts_extract_identically() {
    for f in all_formats() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        make_tree(&src);
        let (a, b) = (
            tmp.path().join(format!("a.{}", ext(f))),
            tmp.path().join(format!("b.{}", ext(f))),
        );
        create_file(f, std::slice::from_ref(&src), &a, &opts(1)).unwrap();
        create_file(f, std::slice::from_ref(&src), &b, &opts(4)).unwrap();
        extract_file(&a, &tmp.path().join("da"), &opts(1)).unwrap();
        extract_file(&b, &tmp.path().join("db"), &opts(4)).unwrap();
        assert_eq!(
            snapshot(&tmp.path().join("da")),
            snapshot(&tmp.path().join("db"))
        );
    }
}

#[test]
fn compression_levels_are_validated_per_format() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("f.txt");
    write(&src, &vec![b'a'; 100_000]);
    let level = |l| Options {
        level: Some(l),
        ..Options::default()
    };
    for f in all_formats() {
        let out = tmp.path().join(format!("o.{}", ext(f)));
        let valid = match f {
            Format::Tar => None,
            Format::TarZst => Some((1, 22)),
            _ => Some((0, 9)),
        };
        match valid {
            None => assert!(matches!(
                create_file(f, std::slice::from_ref(&src), &out, &level(5)),
                Err(Error::InvalidOption(_))
            )),
            Some((lo, hi)) => {
                for l in [lo, hi] {
                    create_file(
                        f,
                        std::slice::from_ref(&src),
                        &out,
                        &Options {
                            overwrite: true,
                            ..level(l)
                        },
                    )
                    .unwrap();
                }
                assert!(matches!(
                    create_file(
                        f,
                        std::slice::from_ref(&src),
                        &out,
                        &Options {
                            overwrite: true,
                            ..level(hi + 1)
                        }
                    ),
                    Err(Error::InvalidOption(_))
                ));
            }
        }
    }
}

#[test]
fn deep_paths_beyond_260_characters_round_trip() {
    for f in all_formats() {
        let tmp = tempfile::tempdir().unwrap();
        // canonicalize: on Windows this yields a \\?\ path, which allows long paths
        let root = fs::canonicalize(tmp.path()).unwrap();
        let mut deep = root.join("src");
        for i in 0..12 {
            deep.push(format!("level-{i:02}-{}", "x".repeat(30)));
        }
        write(&deep.join("file.txt"), b"deep");
        assert!(deep.to_string_lossy().len() > 400);

        let archive = root.join(format!("deep.{}", ext(f)));
        create_file(f, &[root.join("src")], &archive, &opts(2)).unwrap();
        extract_file(&archive, &root.join("out"), &opts(2)).unwrap();
        assert_eq!(snapshot(&root.join("src")), snapshot(&root.join("out/src")));
    }
}

#[test]
fn output_inside_an_input_is_not_archived_into_itself() {
    for f in all_formats() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        write(&src.join("a.txt"), b"a");
        let archive = src.join(format!("self.{}", ext(f)));
        create_file(f, std::slice::from_ref(&src), &archive, &opts(1)).unwrap();
        let mut names = Vec::new();
        list_file(&archive, &opts(1), &mut |e| {
            names.push(e.name.trim_end_matches('/').to_owned())
        })
        .unwrap();
        assert_eq!(names, ["proj", "proj/a.txt"], "{f:?}");
    }
}

#[test]
fn create_refuses_to_replace_an_existing_archive_unless_asked() {
    for f in all_formats() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("f.txt");
        write(&src, b"x");
        let out = tmp.path().join(format!("o.{}", ext(f)));
        fs::write(&out, b"precious").unwrap();
        let r = create_file(f, std::slice::from_ref(&src), &out, &Options::default());
        assert!(matches!(r, Err(Error::AlreadyExists(_))));
        assert_eq!(fs::read(&out).unwrap(), b"precious");
        create_file(
            f,
            std::slice::from_ref(&src),
            &out,
            &Options {
                overwrite: true,
                ..Options::default()
            },
        )
        .unwrap();
        assert_ne!(fs::read(&out).unwrap(), b"precious");
    }
}

#[test]
fn extraction_does_not_clobber_existing_files_unless_asked() {
    for f in all_formats() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("proj");
        write(&src.join("a.txt"), b"new");
        let archive = tmp.path().join(format!("o.{}", ext(f)));
        create_file(f, std::slice::from_ref(&src), &archive, &opts(1)).unwrap();

        let dest = tmp.path().join("dest");
        write(&dest.join("proj/a.txt"), b"old");
        let r = extract_file(&archive, &dest, &opts(1));
        assert!(matches!(r, Err(Error::AlreadyExists(_))), "{f:?}: {r:?}");
        assert_eq!(fs::read(dest.join("proj/a.txt")).unwrap(), b"old");
        assert!(find_part_files(&dest).is_empty());

        extract_file(
            &archive,
            &dest,
            &Options {
                overwrite: true,
                ..opts(1)
            },
        )
        .unwrap();
        assert_eq!(fs::read(dest.join("proj/a.txt")).unwrap(), b"new");
    }
}

#[test]
fn streams_work_without_touching_the_file_system_for_the_archive() {
    // The archive side is a plain in-memory stream, as the stream-based API promises.
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("proj");
    write(&src.join("a.txt"), b"in memory");
    let sources = collect_sources(std::slice::from_ref(&src), &[]).unwrap();
    for f in all_formats() {
        let h = zstar_core::handler(f).unwrap();
        let mut buf = Cursor::new(Vec::new());
        h.create(&sources, &mut buf, &Options::default()).unwrap();
        buf.set_position(0);
        let dest = tmp.path().join(format!("mem-{}", f.name()));
        zstar_core::extract(&mut buf, &dest, &Options::default()).unwrap();
        assert_eq!(fs::read(dest.join("proj/a.txt")).unwrap(), b"in memory");
    }
}
