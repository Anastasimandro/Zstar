mod common;

use std::fs;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::*;
use zstar_core::{Format, Options, Stats, create_file, extract_file};

fn with_threads(stats: bool) -> Options {
    Options {
        threads: 2,
        stats: stats.then(Stats::new),
        ..Options::default()
    }
}

#[test]
fn archive_bytes_are_identical_with_and_without_stats() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    make_tree(&src);
    let big = tmp.path().join("big.bin");
    write(&big, &pseudo(3 << 20, 7));
    for format in all_formats()
        .into_iter()
        .chain(single_formats().into_iter().filter(|&f| f != Format::Br))
    {
        let input = if format.is_single_file() {
            big.clone()
        } else {
            src.clone()
        };
        let plain = tmp.path().join(format!("plain{}", ext(format)));
        let timed = tmp.path().join(format!("timed{}", ext(format)));
        create_file(
            format,
            std::slice::from_ref(&input),
            &plain,
            &with_threads(false),
        )
        .unwrap();
        let opts = with_threads(true);
        create_file(format, &[input], &timed, &opts).unwrap();
        assert_eq!(
            fs::read(&plain).unwrap(),
            fs::read(&timed).unwrap(),
            "{format:?}"
        );
        let s = opts.stats_snapshot().unwrap();
        assert!(s.read_bytes > 0 && s.write_bytes > 0, "{format:?} {s:?}");
    }
}

#[test]
fn compress_counts_the_source_and_the_archive_exactly() {
    let tmp = tempfile::tempdir().unwrap();
    let big = tmp.path().join("big.bin");
    write(&big, &pseudo(1 << 20, 3));
    let out = tmp.path().join("big.bin.gz");
    let opts = Options {
        threads: 1,
        stats: Some(Stats::new()),
        ..Options::default()
    };
    create_file(Format::Gz, &[big], &out, &opts).unwrap();
    let s = opts.stats_snapshot().unwrap();
    assert_eq!(s.read_bytes, 1 << 20);
    assert_eq!(s.write_bytes, fs::metadata(&out).unwrap().len());
    // One thread: what is not reading or writing is compressing, and it all fits in `op`.
    assert!(s.read + s.write <= s.op, "{s:?}");
    assert!(!s.other().is_zero(), "{s:?}");
    assert!(
        s.codec_cpu.is_zero(),
        "the single-thread path does not report it: {s:?}"
    );
}

#[test]
fn parallel_gzip_reports_codec_cpu_time() {
    let tmp = tempfile::tempdir().unwrap();
    let big = tmp.path().join("big.bin");
    write(&big, &pseudo(4 << 20, 5));
    let out = tmp.path().join("big.bin.gz");
    let opts = Options {
        threads: 2,
        stats: Some(Stats::new()),
        ..Options::default()
    };
    create_file(Format::Gz, &[big], &out, &opts).unwrap();
    let s = opts.stats_snapshot().unwrap();
    assert!(!s.codec_cpu.is_zero(), "{s:?}");
    assert_eq!(s.threads_pool, 2);
}

#[test]
fn extract_counts_the_archive_and_the_files() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    make_tree(&src);
    let archive = tmp.path().join("t.tar.gz");
    create_file(
        Format::TarGz,
        std::slice::from_ref(&src),
        &archive,
        &Options::default(),
    )
    .unwrap();
    let dest = tmp.path().join("out");
    let opts = Options {
        stats: Some(Stats::new()),
        ..Options::default()
    };
    let summary = extract_file(&archive, &dest, &opts).unwrap();
    let s = opts.stats_snapshot().unwrap();
    assert_eq!(s.write_bytes, summary.bytes);
    assert!(s.read_bytes >= fs::metadata(&archive).unwrap().len() - 1024);
    assert!(s.read + s.write <= s.op, "{s:?}");
}

#[test]
fn the_open_hook_is_used_and_a_failing_hook_is_not_fatal() {
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("a.txt");
    write(&f, b"hello hello hello");
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let failing = Options {
        open_hook: Some(Arc::new(move |_: &Path| {
            seen.fetch_add(1, Ordering::Relaxed);
            Err(io::Error::other("no hint for you"))
        })),
        ..Options::default()
    };
    let out = tmp.path().join("a.txt.gz");
    create_file(Format::Gz, std::slice::from_ref(&f), &out, &failing).unwrap();
    assert!(calls.load(Ordering::Relaxed) >= 1);
    let dest = tmp.path().join("x");
    extract_file(&out, &dest, &failing).unwrap();
    assert_eq!(fs::read(dest.join("a.txt")).unwrap(), b"hello hello hello");
    assert!(
        calls.load(Ordering::Relaxed) >= 2,
        "the archive is opened through the hook too"
    );

    // Without a hook (the CLI's `--no-seq-hint`) the plain open gives the same result.
    let plain = Options::default();
    assert!(plain.open_hook.is_none());
    let dest_plain = tmp.path().join("y");
    extract_file(&out, &dest_plain, &plain).unwrap();
    assert_eq!(
        fs::read(dest_plain.join("a.txt")).unwrap(),
        b"hello hello hello"
    );

    let missing = tmp.path().join("missing.gz");
    assert!(extract_file(&missing, &dest, &failing).is_err());
}

#[test]
fn stats_are_off_by_default() {
    assert!(Options::default().stats.is_none());
    assert!(Options::default().stats_snapshot().is_none());
}
