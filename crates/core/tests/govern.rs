//! The governor on real operations: the counters are live while the operation runs, the
//! knobs it sets reach the workers, and none of it changes what is written (ADR 0012).
//!
//! The throughput limit makes the operation last long enough to see several ticks. Nothing
//! asserts a speed, only that the counters moved before the end and that the bytes match.

#![cfg(feature = "gzip")]

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::Duration;

use zstar_core::{
    Control, Format, Mode, Options, Stats, TuneBounds, Tuner, create_file, extract_file,
    start_governor,
};

fn bounds() -> TuneBounds {
    TuneBounds {
        max_workers: 2,
        max_io_depth: 8,
        min_chunk: 4096,
        max_chunk: 256 << 10,
    }
}

/// Deterministic, compressible but not trivial.
fn data(len: usize) -> Vec<u8> {
    let mut x: u64 = 0x2545_F491_4F6C_DD1D;
    (0..len)
        .map(|i| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            if i % 64 < 40 {
                b'a' + (i % 7) as u8
            } else {
                x as u8
            }
        })
        .collect()
}

fn limited(stats: &Stats, rate: u64) -> Options {
    let control = Control::new();
    control.set_rate_limit(rate);
    Options {
        control,
        stats: Some(stats.clone()),
        ..Options::default()
    }
}

fn write_input(dir: &Path, len: usize) -> std::path::PathBuf {
    let input = dir.join("input.txt");
    fs::write(&input, data(len)).unwrap();
    input
}

#[test]
fn counters_move_while_the_operation_runs_and_the_knobs_reach_the_control() {
    let dir = tempfile::tempdir().unwrap();
    let input = write_input(dir.path(), 10 << 20);
    let out = dir.path().join("out.gz");

    let stats = Stats::new();
    // One thread: its tally is the calling thread's, which only ends with the operation.
    // Parallel gzip closes a tally per wave, which would hide a missing live flush.
    let opts = Options {
        threads: 1,
        ..limited(&stats, 12 << 20)
    };
    let tuner = Tuner::new(Mode::Foreground, bounds(), None);
    let running = start_governor(&opts, tuner, None).unwrap().unwrap();

    let done = AtomicBool::new(false);
    let mut looks = Vec::new();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            // Look from outside while the operation is still running.
            for wait in [150, 250] {
                std::thread::sleep(Duration::from_millis(wait));
                if !done.load(Relaxed) {
                    looks.extend(opts.stats_snapshot());
                }
            }
        });
        create_file(Format::Gz, std::slice::from_ref(&input), &out, &opts).unwrap();
        done.store(true, Relaxed);
    });
    let tuner = running.stop().unwrap();

    let [first, second] = looks[..] else {
        panic!("the operation ended before the looks; make it longer: {looks:?}");
    };
    assert!(second.io_ops > first.io_ops && second.read_bytes > first.read_bytes);
    // The calling thread's tally only ends with the operation, so the busy time that grows
    // between two looks was handed over live. (The tree walker's tally ends early and gives
    // a constant, which is why this compares two looks and not one against zero.)
    assert!(
        second.busy > first.busy + Duration::from_millis(5),
        "busy not flushed live: {first:?} then {second:?}"
    );
    assert!(tuner.knobs().workers >= 1);
    assert!(opts.control.chunk() >= 4096 && opts.control.chunk() <= bounds().max_chunk);
}

#[test]
fn changing_the_chunk_size_while_it_runs_changes_no_byte() {
    let dir = tempfile::tempdir().unwrap();
    let input = write_input(dir.path(), 3 << 20);

    let plain = dir.path().join("plain.gz");
    create_file(
        Format::Gz,
        std::slice::from_ref(&input),
        &plain,
        &Options::default(),
    )
    .unwrap();

    let moved = dir.path().join("moved.gz");
    let stats = Stats::new();
    let opts = limited(&stats, 8 << 20);
    opts.control.set_chunk_ceiling(256 << 10);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for chunk in [4096, 256 << 10, 16 << 10, 100_000, 0] {
                std::thread::sleep(Duration::from_millis(60));
                opts.control.set_chunk(chunk);
            }
        });
        create_file(Format::Gz, std::slice::from_ref(&input), &moved, &opts).unwrap();
    });
    assert_eq!(fs::read(&plain).unwrap(), fs::read(&moved).unwrap());

    // The same on the way back.
    let (a, b) = (dir.path().join("a"), dir.path().join("b"));
    extract_file(&plain, &a, &Options::default()).unwrap();
    let opts = limited(&Stats::new(), 8 << 20);
    opts.control.set_chunk_ceiling(256 << 10);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for chunk in [4096, 256 << 10, 8192, 0] {
                std::thread::sleep(Duration::from_millis(60));
                opts.control.set_chunk(chunk);
            }
        });
        extract_file(&plain, &b, &opts).unwrap();
    });
    assert_eq!(
        fs::read(a.join("plain")).unwrap(),
        fs::read(b.join("plain")).unwrap()
    );
    assert_eq!(fs::read(b.join("plain")).unwrap(), data(3 << 20));
}
