//! 7z specifics: hostile names, damage in the middle of a stream, the decoder memory limit,
//! and interoperability with the real `7z` tool when it is installed.

mod common;

use std::fs;
use std::io::Cursor;
use std::path::Path;
use std::process::Command;

use common::*;
use zstar_core::{
    EntryKind, Error, Format, LimitKind, Options, SourceEntry, create_file, extract, extract_file,
    handler, list_file,
};

fn available() -> bool {
    handler(Format::SevenZ).is_ok()
}

fn source(path: &Path, name: &str, size: u64) -> SourceEntry {
    SourceEntry {
        path: path.into(),
        name: name.into(),
        kind: EntryKind::File,
        size,
        mode: Some(0o644),
        mtime: None,
        link_target: None,
    }
}

fn make(dir: &Path, data: &[u8]) -> std::path::PathBuf {
    let src = dir.join("f.bin");
    write(&src, data);
    let archive = dir.join("a.7z");
    create_file(Format::SevenZ, &[src], &archive, &Options::default()).unwrap();
    archive
}

#[test]
fn hostile_entry_names_are_rejected_and_nothing_escapes() {
    if !available() {
        return;
    }
    for bad in [
        "../evil.txt",
        "a/../../evil.txt",
        "/abs/evil.txt",
        "a/./../../evil.txt",
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real.txt");
        write(&real, b"payload");
        let mut out = Cursor::new(Vec::new());
        handler(Format::SevenZ)
            .unwrap()
            .create(&[source(&real, bad, 7)], &mut out, &Options::default())
            .unwrap();
        out.set_position(0);

        let dest = tmp.path().join("dest");
        let r = extract(&mut out, &dest, &Options::default());
        assert!(matches!(r, Err(Error::UnsafePath { .. })), "{bad}: {r:?}");
        assert!(!tmp.path().join("evil.txt").exists(), "{bad} escaped");
        assert!(!Path::new("/abs/evil.txt").exists());
        assert!(find_part_files(tmp.path()).is_empty(), "{bad}: .part left");
    }
}

#[test]
fn damage_inside_the_stream_is_caught_and_leaves_nothing() {
    if !available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let archive = make(tmp.path(), &pseudo(3 << 20, 21));
    let good = fs::read(&archive).unwrap();
    for (what, at) in [
        ("start", 40),
        ("middle", good.len() / 2),
        ("late", good.len() * 3 / 4),
    ] {
        let mut bad = good.clone();
        bad[at] ^= 0xff;
        let dest = tmp.path().join(format!("dest-{what}"));
        let r = extract(&mut Cursor::new(bad), &dest, &Options::default());
        assert!(r.is_err(), "{what}: damaged 7z extracted successfully");
        assert!(!dest.join("f.bin").exists(), "{what}: damaged file kept");
        assert!(find_part_files(&dest).is_empty(), "{what}: .part left");
    }
}

#[test]
fn a_dictionary_above_the_memory_limit_is_refused_before_decoding() {
    // Default presets use an 8 MiB dictionary, so a 1 MiB limit must trip for 7z and xz alike.
    for format in [Format::SevenZ, Format::Xz, Format::TarXz] {
        if handler(format).is_err() {
            continue;
        }
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("f.bin");
        write(&src, &pseudo(1 << 20, 4));
        let archive = tmp.path().join(format!("a.{}", ext(format)));
        create_file(format, &[src], &archive, &Options::default()).unwrap();

        let mut o = Options::default();
        o.limits.max_memory_bytes = 1 << 20;
        let dest = tmp.path().join("dest");
        let r = extract_file(&archive, &dest, &o);
        assert!(
            matches!(r, Err(Error::LimitExceeded(LimitKind::Memory))),
            "{format:?}: {r:?}"
        );
        assert!(find_part_files(&dest).is_empty());

        // The message must name the limit and where to raise it.
        let msg = r.unwrap_err().to_string();
        assert!(msg.contains("limits.max_memory_bytes"), "{msg}");

        // The same archive is fine under the default limit.
        extract_file(&archive, &tmp.path().join("ok"), &Options::default()).unwrap();
    }
}

#[test]
fn entry_count_limit_applies_before_extraction_starts() {
    if !available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("proj");
    for i in 0..20 {
        write(&src.join(format!("f{i}.txt")), b"x");
    }
    let archive = tmp.path().join("a.7z");
    create_file(Format::SevenZ, &[src], &archive, &Options::default()).unwrap();
    let mut o = Options::default();
    o.limits.max_entries = 5;
    let dest = tmp.path().join("dest");
    let r = extract_file(&archive, &dest, &o);
    assert!(
        matches!(r, Err(Error::LimitExceeded(LimitKind::Entries))),
        "{r:?}"
    );
    assert!(!dest.exists() || fs::read_dir(&dest).unwrap().count() == 0);
}

#[test]
fn empty_files_directories_and_solid_blocks_round_trip() {
    if !available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("proj");
    write(&src.join("empty"), b"");
    write(&src.join("d/e/f.txt"), b"nested");
    fs::create_dir_all(src.join("only-a-dir")).unwrap();
    for i in 0..400 {
        write(
            &src.join(format!("many/{i:03}.txt")),
            format!("file {i}").as_bytes(),
        );
    }
    let archive = tmp.path().join("a.7z");
    create_file(
        Format::SevenZ,
        std::slice::from_ref(&src),
        &archive,
        &Options::default(),
    )
    .unwrap();
    let mut n = 0;
    list_file(&archive, &Options::default(), &mut |_| n += 1).unwrap();
    // proj, empty, d, d/e, d/e/f.txt, only-a-dir, many + 400 files
    assert_eq!(n, 7 + 400, "entries listed");
    extract_file(&archive, &tmp.path().join("out"), &Options::default()).unwrap();
    assert_eq!(snapshot(&src), snapshot(&tmp.path().join("out/proj")));
}

fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("--help")
        .output()
        .is_ok_and(|o| !o.stdout.is_empty() || !o.stderr.is_empty())
}

#[test]
fn the_real_7z_tool_reads_ours_and_we_read_its_archives() {
    if !available() {
        return;
    }
    let tool = ["7z", "7zz", "7za"].into_iter().find(|t| have(t));
    let Some(tool) = tool else {
        eprintln!("skipped: no 7z tool installed");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("proj");
    write(&src.join("a.bin"), &pseudo(2 << 20, 8));
    write(&src.join("sub/b.txt"), b"hello");
    write(&src.join("sub/ünï.txt"), "unicode".as_bytes());
    write(&src.join("empty"), b"");
    fs::create_dir_all(src.join("emptydir")).unwrap();

    // ours -> 7z: test and extract
    let ours = tmp.path().join("ours.7z");
    create_file(
        Format::SevenZ,
        std::slice::from_ref(&src),
        &ours,
        &Options::default(),
    )
    .unwrap();
    let t = Command::new(tool).arg("t").arg(&ours).output().unwrap();
    assert!(
        t.status.success(),
        "{tool} t: {}",
        String::from_utf8_lossy(&t.stdout)
    );
    let theirs_out = tmp.path().join("by-7z");
    let x = Command::new(tool)
        .arg("x")
        .arg(format!("-o{}", theirs_out.display()))
        .arg(&ours)
        .output()
        .unwrap();
    assert!(x.status.success(), "{}", String::from_utf8_lossy(&x.stdout));
    assert_eq!(snapshot(&src), snapshot(&theirs_out.join("proj")));

    // 7z -> ours
    let theirs = tmp.path().join("theirs.7z");
    let a = Command::new(tool)
        .current_dir(tmp.path())
        .arg("a")
        .arg(&theirs)
        .arg("proj")
        .output()
        .unwrap();
    assert!(a.status.success(), "{}", String::from_utf8_lossy(&a.stdout));
    let out = tmp.path().join("by-us");
    extract_file(&theirs, &out, &Options::default()).unwrap();
    assert_eq!(snapshot(&src), snapshot(&out.join("proj")));
}
