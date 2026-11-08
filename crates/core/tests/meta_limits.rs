//! The streaming walk and the metadata limits: archives are the same as from a full list,
//! every limit is a clear error (never a skipped entry), and nothing is left half written.

mod common;

use std::fs;
use std::io::Cursor;
use std::path::Path;

use common::*;
use zstar_core::{
    Control, Error, Format, LimitKind, Limits, Options, collect_sources, create_file, extract,
    extract_file, handler,
};

fn limited(f: impl FnOnce(&mut Limits)) -> Options {
    let mut o = Options {
        overwrite: true,
        ..Options::default()
    };
    f(&mut o.limits);
    o
}

fn expect_limit(r: Result<impl std::fmt::Debug, Error>, kind: LimitKind) {
    match r {
        Err(Error::LimitExceeded(k)) => assert_eq!(k, kind),
        other => panic!("expected LimitExceeded({kind:?}), got {other:?}"),
    }
}

#[test]
fn streaming_a_tree_gives_the_same_archive_as_writing_its_list() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    make_tree(&src);
    let opts = Options::default();
    for format in all_formats() {
        let archive = tmp.path().join(format!("s{}", ext(format)));
        create_file(format, std::slice::from_ref(&src), &archive, &opts).unwrap();
        let sources = collect_sources(std::slice::from_ref(&src), &[]).unwrap();
        let mut listed = Cursor::new(Vec::new());
        handler(format)
            .unwrap()
            .create(&sources, &mut listed, &opts)
            .unwrap();
        assert_eq!(
            fs::read(&archive).unwrap(),
            listed.into_inner(),
            "{format:?}: streamed and listed archives differ"
        );
    }
}

#[test]
fn a_tree_larger_than_the_queue_is_archived_completely() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("big");
    for i in 0..3000 {
        write(&src.join(format!("d{:02}/f{i:04}", i % 30)), b"x");
    }
    let archive = tmp.path().join("big.zip");
    create_file(
        Format::Zip,
        std::slice::from_ref(&src),
        &archive,
        &Options::default(),
    )
    .unwrap();
    let dest = tmp.path().join("out");
    let summary = extract_file(&archive, &dest, &Options::default()).unwrap();
    assert_eq!(summary.files, 3000);
    assert_eq!(snapshot(&src), snapshot(&dest.join("big")));
}

#[test]
fn entry_limit_on_creation_is_an_error_and_leaves_nothing_behind() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    make_tree(&src);
    for format in all_formats() {
        let archive = tmp.path().join(format!("l{}", ext(format)));
        let opts = limited(|l| l.max_entries = 5);
        expect_limit(
            create_file(format, std::slice::from_ref(&src), &archive, &opts),
            LimitKind::Entries,
        );
        assert!(!archive.exists(), "{format:?} left an archive");
        assert!(
            find_part_files(tmp.path()).is_empty(),
            "{format:?} left a .part"
        );
    }
}

#[test]
fn name_depth_and_metadata_limits_on_creation() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    write(&src.join("a/b/c/d/leaf.txt"), b"x");
    write(&src.join("a/a-rather-long-file-name.txt"), b"y");
    let out = tmp.path().join("o.zip");
    let zip = |o: &Options| create_file(Format::Zip, std::slice::from_ref(&src), &out, o);

    expect_limit(zip(&limited(|l| l.max_depth = 4)), LimitKind::Depth);
    zip(&limited(|l| l.max_depth = 6)).unwrap();
    expect_limit(
        zip(&limited(|l| l.max_path_bytes = 20)),
        LimitKind::PathLength,
    );
    zip(&limited(|l| l.max_path_bytes = 64)).unwrap();
    // Walk listings and the zip writer's records both count against the metadata limit.
    expect_limit(
        zip(&limited(|l| l.max_metadata_bytes = Some(100))),
        LimitKind::Metadata,
    );
    zip(&limited(|l| l.max_metadata_bytes = Some(1 << 20))).unwrap();
    // `Limits::unlimited` turns every one of them off.
    let mut opts = limited(|_| {});
    opts.limits = Limits::unlimited();
    zip(&opts).unwrap();
}

#[test]
fn a_missing_input_is_the_same_error_and_leaves_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("o.zip");
    let err = create_file(
        Format::Zip,
        &[tmp.path().join("nope")],
        &out,
        &Options::default(),
    )
    .unwrap_err();
    assert!(
        matches!(&err, Error::Io(e) if e.kind() == std::io::ErrorKind::NotFound),
        "{err:?}"
    );
    assert!(!out.exists() && find_part_files(tmp.path()).is_empty());
}

#[test]
fn a_cancelled_creation_returns_cancelled_and_cleans_up() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    make_tree(&src);
    for format in all_formats() {
        let opts = Options::default();
        let control: Control = opts.control.clone();
        control.cancel();
        let archive = tmp.path().join(format!("c{}", ext(format)));
        let err = create_file(format, std::slice::from_ref(&src), &archive, &opts).unwrap_err();
        assert!(matches!(err, Error::Cancelled), "{format:?}: {err:?}");
        assert!(!archive.exists());
    }
    assert!(find_part_files(tmp.path()).is_empty());
}

fn long_name(components: usize, each: usize) -> String {
    vec!["x".repeat(each); components].join("/")
}

#[test]
fn zip_extraction_refuses_long_names_and_deep_names_before_writing() {
    let tmp = tempfile::tempdir().unwrap();
    for (name, opts, kind) in [
        (
            long_name(1, 100),
            limited(|l| l.max_path_bytes = 64),
            LimitKind::PathLength,
        ),
        (
            long_name(8, 1),
            limited(|l| l.max_depth = 4),
            LimitKind::Depth,
        ),
    ] {
        let mut zip = RawZip::new();
        zip.file("fine.txt", b"ok").file(&name, b"data");
        let dest = tmp.path().join("dest");
        expect_limit(extract(&mut Cursor::new(zip.finish()), &dest, &opts), kind);
        assert!(
            !dest.join("fine.txt").exists(),
            "the whole archive is refused before any file is written"
        );
        let _ = fs::remove_dir_all(&dest);
    }
}

#[test]
fn zip_extraction_limits_the_metadata_the_plan_holds() {
    let mut zip = RawZip::new();
    for i in 0..200 {
        zip.file(&format!("dir/file-with-a-name-{i:04}.txt"), b"x");
    }
    let bytes = zip.finish();
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("d");
    expect_limit(
        extract(
            &mut Cursor::new(bytes.clone()),
            &dest,
            &limited(|l| l.max_metadata_bytes = Some(2000)),
        ),
        LimitKind::Metadata,
    );
    assert!(!dest.exists() || fs::read_dir(&dest).unwrap().next().is_none());
    let summary = extract(&mut Cursor::new(bytes), &dest, &Options::default()).unwrap();
    assert_eq!(summary.files, 200);
}

#[test]
fn tar_extraction_refuses_long_names_too() {
    if handler(Format::Tar).is_err() {
        return;
    }
    let name = long_name(1, 90);
    let tar = raw_tar(&[tar_file("fine.txt", b"ok"), tar_file(&name, b"data")]);
    let tmp = tempfile::tempdir().unwrap();
    let opts = limited(|l| l.max_path_bytes = 64);
    let r = extract(&mut Cursor::new(tar), &tmp.path().join("d"), &opts);
    expect_limit(r, LimitKind::PathLength);
}

/// Names that differ only by case still make extraction serial and correct: the compact
/// collision keys must find them.
#[test]
fn case_only_collisions_are_still_found_with_compact_keys() {
    let mut zip = RawZip::new();
    zip.file("Dir/File.txt", b"first")
        .file("dir/file.txt", b"second");
    let tmp = tempfile::tempdir().unwrap();
    let opts = Options {
        threads: 4,
        ..Options::default()
    };
    let dest = tmp.path().join("d");
    // On a case-sensitive file system both land. On an insensitive one (Windows, macOS) the
    // second would overwrite the first, so extraction must refuse it: an error, never a
    // silent loss.
    match extract(&mut Cursor::new(zip.finish()), &dest, &opts) {
        Ok(summary) => {
            assert_eq!(summary.files, 2);
            assert_exists(&dest.join("Dir/File.txt"));
            assert_exists(&dest.join("dir/file.txt"));
        }
        Err(e) => assert!(
            matches!(e, Error::AlreadyExists(_)),
            "unexpected error: {e:?}"
        ),
    }
}

fn assert_exists(p: &Path) {
    assert!(p.exists(), "{} should exist", p.display());
}

#[test]
fn the_metadata_allowance_follows_the_memory_limit_unless_it_is_set() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    write(&src.join("a/file-with-a-name.txt"), b"x");
    let out = tmp.path().join("o.zip");
    let zip = |o: &Options| create_file(Format::Zip, std::slice::from_ref(&src), &out, o);

    // Nothing set: the memory limit is the metadata limit, so a tiny one refuses the tree.
    assert_eq!(
        Limits::default().metadata_limit(),
        Limits::default().max_memory_bytes
    );
    expect_limit(
        zip(&limited(|l| l.max_memory_bytes = 100)),
        LimitKind::Metadata,
    );
    // An explicit metadata limit wins over the memory limit, up or down.
    zip(&limited(|l| {
        l.max_memory_bytes = 100;
        l.max_metadata_bytes = Some(1 << 20);
    }))
    .unwrap();
    expect_limit(
        zip(&limited(|l| l.max_metadata_bytes = Some(100))),
        LimitKind::Metadata,
    );
    // No limits means no metadata limit either.
    assert_eq!(Limits::unlimited().metadata_limit(), u64::MAX);
}

#[test]
fn small_devices_get_smaller_defaults_and_big_ones_the_usual_ones() {
    const GIB: u64 = 1 << 30;
    let usual = Limits::default();
    assert_eq!(
        (usual.max_memory_bytes, usual.max_entries),
        (GIB, 10_000_000)
    );
    for ram in [4 * GIB, 8 * GIB, 64 * GIB] {
        let l = Limits::for_ram(ram);
        assert_eq!(l.max_memory_bytes, usual.max_memory_bytes);
        assert_eq!(l.max_entries, usual.max_entries);
        assert_eq!(l.metadata_limit(), usual.metadata_limit());
    }
    // 2 and 3 GiB phones: a quarter of the RAM, entries in proportion.
    let two = Limits::for_ram(2 * GIB);
    assert_eq!(two.metadata_limit(), 512 << 20);
    assert_eq!(two.max_entries, 5_000_000);
    let three = Limits::for_ram(3 * GIB);
    assert_eq!(three.metadata_limit(), 768 << 20);
    assert_eq!(three.max_entries, 7_500_000);
    // Never below a floor, whatever the figure; the other limits are untouched.
    let tiny = Limits::for_ram(0);
    assert_eq!(tiny.max_memory_bytes, 64 << 20);
    assert_eq!(tiny.max_depth, usual.max_depth);
    assert_eq!(tiny.max_path_bytes, usual.max_path_bytes);
    assert_eq!(tiny.max_total_bytes, usual.max_total_bytes);
}

#[test]
fn every_limit_error_names_its_key_and_how_to_raise_it() {
    for (kind, key) in [
        (LimitKind::TotalSize, "max_total_bytes"),
        (LimitKind::Entries, "max_entries"),
        (LimitKind::Ratio, "max_ratio"),
        (LimitKind::Memory, "max_memory_bytes"),
        (LimitKind::PathLength, "max_path_bytes"),
        (LimitKind::Depth, "max_depth"),
        (LimitKind::Metadata, "max_metadata_bytes"),
    ] {
        let text = Error::LimitExceeded(kind).to_string();
        assert!(text.contains(key), "{kind:?}: {text}");
    }
}
