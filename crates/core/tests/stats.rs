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

fn regular_files(root: &Path) -> u64 {
    fs::read_dir(root)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            let ft = e.file_type().unwrap();
            if ft.is_dir() {
                regular_files(&e.path())
            } else {
                u64::from(ft.is_file())
            }
        })
        .sum()
}

/// Every part of thread time, excluding the parked time the control counts on its own.
fn thread_time(s: &zstar_core::StatsSnapshot) -> std::time::Duration {
    s.busy + s.wait_input + s.wait_output + s.wait_workers + s.finish
}

#[test]
fn every_operation_reports_its_dominant_stage_and_exact_counts() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    make_tree(&src);
    let files_in_tree = regular_files(&src);
    let big = tmp.path().join("big.bin");
    write(&big, &pseudo(3 << 20, 7));
    for format in all_formats()
        .into_iter()
        .chain(single_formats().into_iter().filter(|&f| f != Format::Br))
    {
        let single = format.is_single_file();
        let input = if single { big.clone() } else { src.clone() };
        let expected = if single { 1 } else { files_in_tree };
        let out = tmp.path().join(format!("a{}", ext(format)));

        let opts = with_threads(true);
        create_file(format, &[input], &out, &opts).unwrap();
        let c = opts.stats_snapshot().unwrap();
        assert!(c.dominant().is_some(), "{format:?} create {c:?}");
        // The calling thread's tally covers the whole operation; workers add to it.
        assert!(thread_time(&c) + c.parked >= c.op, "{format:?} {c:?}");
        assert!(
            thread_time(&c)
                <= c.op * (c.threads_pool.max(1) as u32 + 1) + std::time::Duration::from_millis(50),
            "{format:?} {c:?}"
        );
        assert!(c.opens >= expected, "{format:?} {c:?}");
        assert!(!c.finish.is_zero(), "{format:?} {c:?}");
        if format == Format::SevenZ {
            // Solid blocks: files are not handled one by one.
            assert_eq!((c.files, c.latency_p50), (0, None), "{c:?}");
        } else {
            assert_eq!(c.files, expected, "{format:?} {c:?}");
            let (p50, p95) = (c.latency_p50.unwrap(), c.latency_p95.unwrap());
            assert!(p50 <= p95, "{format:?} {c:?}");
        }

        let dest = tmp.path().join(format!("x{}", ext(format)));
        let opts = with_threads(true);
        let summary = extract_file(&out, &dest, &opts).unwrap();
        let e = opts.stats_snapshot().unwrap();
        assert!(e.dominant().is_some(), "{format:?} extract {e:?}");
        assert!(thread_time(&e) + e.parked >= e.op, "{format:?} {e:?}");
        assert_eq!(e.files, summary.files, "{format:?} {e:?}");
        assert_eq!(e.renames, summary.files, "{format:?} {e:?}");
        assert!(
            e.mkdirs >= 1 && e.stat_calls >= summary.files,
            "{format:?} {e:?}"
        );
        assert!(e.opens > summary.files, "{format:?} {e:?}");
        assert!(e.latency_p50.unwrap() <= e.latency_p95.unwrap());
    }
}

#[test]
fn formats_with_a_queue_report_its_occupancy_and_the_others_none() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    make_tree(&src);
    let big = tmp.path().join("big.bin");
    write(&big, &pseudo(4 << 20, 5));
    for (format, input) in [(Format::Zip, &src), (Format::Gz, &big)] {
        let out = tmp.path().join(format!("q{}", ext(format)));
        let opts = with_threads(true);
        create_file(format, std::slice::from_ref(input), &out, &opts).unwrap();
        let s = opts.stats_snapshot().unwrap();
        assert!(s.queue_samples > 0 && s.queue_max >= 1, "{format:?} {s:?}");
        assert!(s.queue_avg().unwrap() <= s.queue_max as f64, "{s:?}");
    }
    let out = tmp.path().join("plain.tar");
    let opts = with_threads(true);
    create_file(Format::Tar, std::slice::from_ref(&src), &out, &opts).unwrap();
    let s = opts.stats_snapshot().unwrap();
    assert_eq!((s.queue_samples, s.queue_avg()), (0, None), "{s:?}");
}

#[test]
fn a_cancelled_or_failed_file_is_not_counted_as_done() {
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("a.txt");
    write(&f, b"data");
    let out = tmp.path().join("a.txt.gz");
    let opts = with_threads(true);
    create_file(Format::Gz, std::slice::from_ref(&f), &out, &opts).unwrap();
    // Extracting over an existing file without `overwrite` fails before any file is done.
    let dest = tmp.path().join("d");
    write(&dest.join("a.txt"), b"old");
    let opts = with_threads(true);
    assert!(extract_file(&out, &dest, &opts).is_err());
    let s = opts.stats_snapshot().unwrap();
    assert_eq!((s.files, s.renames, s.latency_p50), (0, 0, None), "{s:?}");
}
