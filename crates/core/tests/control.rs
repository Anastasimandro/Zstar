//! The control handle: changing threads, profile, priority or throughput while an operation
//! runs must never change its result, and cancelling must leave nothing half written.
//!
//! Nothing here depends on timing. Changes are made from the progress callback once a fixed
//! share of the work is done, and the assertions are about the result, never about speed.

#![cfg(all(feature = "zip", feature = "gzip"))]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

use zstar_core::{
    Control, DriveKind, Error, Format, Options, Priority, Profile, Progress, ProgressFn,
    WorkerHook, create_file, extract_file,
};

/// Deterministic bytes in alternating runs of noise and text, so that blocks differ.
fn sample(len: usize) -> Vec<u8> {
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut out = Vec::with_capacity(len + (1 << 17));
    while out.len() < len {
        for _ in 0..8192 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            out.extend_from_slice(&x.to_le_bytes());
        }
        for i in 0..4096 {
            out.extend_from_slice(format!("line {i} of some text\n").as_bytes());
        }
    }
    out.truncate(len);
    out
}

/// A tree of `files` small files of different sizes.
fn make_tree(root: &Path, files: usize) {
    for i in 0..files {
        let dir = root.join(format!("d{}", i % 7));
        fs::create_dir_all(&dir).unwrap();
        let body: Vec<u8> = (0..(i * 37) % 20_000)
            .map(|b| (b as u8).wrapping_mul(i as u8 | 1))
            .collect();
        fs::write(dir.join(format!("f{i}.bin")), body).unwrap();
    }
}

/// Every file under `root`, by path relative to it.
fn walk(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn go(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                go(root, &path, out);
            } else {
                let rel = path.strip_prefix(root).unwrap().to_path_buf();
                out.insert(rel, fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    go(root, root, &mut out);
    out
}

fn has_part_file(root: &Path) -> bool {
    walk(root)
        .keys()
        .any(|p| p.extension().is_some_and(|e| e == "part"))
}

/// A progress callback that runs `act(share)` with the finished share of the work in
/// quarters (0 to 4). It may be called from several threads at once.
fn on_quarters(act: impl Fn(u64) + Send + Sync + 'static) -> ProgressFn {
    Arc::new(move |p: &Progress<'_>| {
        let (done, total) = match (p.bytes_total, p.entries_total) {
            (Some(t), _) if t > 0 && p.entries_total == Some(1) => (p.bytes_done, t),
            (_, Some(t)) if t > 0 => (p.entries_done, t),
            _ => return,
        };
        act(done * 4 / total);
    })
}

/// Runs each step once, in order, when the share it waits for is reached.
struct Steps(AtomicUsize);

impl Steps {
    fn due(&self, step: usize, share: u64, reached: u64) -> bool {
        reached >= share
            && self
                .0
                .compare_exchange(step, step + 1, Relaxed, Relaxed)
                .is_ok()
    }
}

fn recorder() -> (WorkerHook, Arc<Mutex<Vec<Priority>>>) {
    let heard = Arc::new(Mutex::new(Vec::new()));
    let log = heard.clone();
    (Arc::new(move |p| log.lock().unwrap().push(p)), heard)
}

#[test]
fn gzip_round_trips_whatever_the_threads_do_while_it_runs() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("data.bin");
    let data = sample(12 << 20);
    fs::write(&input, &data).unwrap();
    let compress = |name: &str, opts: &Options| {
        let out = dir.path().join(name);
        create_file(Format::Gz, std::slice::from_ref(&input), &out, opts).unwrap();
        out
    };
    let base = Options {
        threads: 4,
        level: Some(1),
        ..Options::default()
    };
    let steady = compress("steady.gz", &base);

    let control = Control::new();
    let steps = Arc::new(Steps(AtomicUsize::new(0)));
    let (hook, heard) = recorder();
    let (c, s) = (control.clone(), steps.clone());
    let live = Options {
        control: control.clone(),
        worker_hook: Some(hook),
        progress: Some(on_quarters(move |share| {
            if s.due(0, 1, share) {
                c.set_threads(1);
            }
            if s.due(1, 2, share) {
                c.set_profile(Profile::Gentle);
            }
            if s.due(2, 3, share) {
                c.set_profile(Profile::Fast);
            }
        })),
        ..base
    };
    let changed = compress("live.gz", &live);

    // The bytes may differ by a few from a run with a steady thread count (zlib state per
    // worker), so what is checked is that both decompress to the input.
    assert_eq!(steps.0.load(Relaxed), 3, "all three changes were made");
    let heard = heard.lock().unwrap();
    assert!(heard.contains(&Priority::Normal) && heard.contains(&Priority::Low));

    for (gz, out) in [(&steady, "out-steady"), (&changed, "out-live")] {
        let dest = dir.path().join(out);
        extract_file(gz, &dest, &Options::default()).unwrap();
        let name = gz.file_stem().unwrap();
        assert_eq!(fs::read(dest.join(name)).unwrap(), data);
    }
}

#[test]
fn zip_extraction_survives_limit_and_profile_changes() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    make_tree(&src, 300);
    let archive = dir.path().join("a.zip");
    create_file(
        Format::Zip,
        std::slice::from_ref(&src),
        &archive,
        &Options::default(),
    )
    .unwrap();

    let control = Control::new();
    let steps = Arc::new(Steps(AtomicUsize::new(0)));
    let (hook, heard) = recorder();
    let (c, s) = (control.clone(), steps.clone());
    let opts = Options {
        threads: 4,
        profile: Profile::Fast,
        drive: DriveKind::Ssd,
        control: control.clone(),
        worker_hook: Some(hook),
        progress: Some(on_quarters(move |share| {
            if s.due(0, 1, share) {
                c.set_threads(1);
            }
            if s.due(1, 2, share) {
                c.set_profile(Profile::Gentle);
            }
            if s.due(2, 3, share) {
                c.set_profile(Profile::Fast);
            }
        })),
        ..Options::default()
    };
    let dest = dir.path().join("out");
    extract_file(&archive, &dest, &opts).unwrap();

    assert_eq!(steps.0.load(Relaxed), 3);
    assert_eq!(walk(&dest.join("src")), walk(&src));
    assert!(heard.lock().unwrap().contains(&Priority::Low));
    assert!(!has_part_file(&dest));
}

#[test]
fn workers_above_the_limit_do_not_outlive_the_work() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    make_tree(&src, 120);
    let archive = dir.path().join("a.zip");
    create_file(
        Format::Zip,
        std::slice::from_ref(&src),
        &archive,
        &Options::default(),
    )
    .unwrap();

    // Three of the four workers are held back from the start and the limit never rises.
    let opts = Options {
        threads: 4,
        drive: DriveKind::Ssd,
        ..Options::default()
    };
    opts.control.set_threads(1);
    let dest = dir.path().join("out");
    extract_file(&archive, &dest, &opts).unwrap();
    assert_eq!(walk(&dest.join("src")), walk(&src));
}

#[test]
fn an_hdd_extracts_one_file_at_a_time_and_still_gets_everything() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    make_tree(&src, 120);
    let archive = dir.path().join("a.zip");
    create_file(
        Format::Zip,
        std::slice::from_ref(&src),
        &archive,
        &Options::default(),
    )
    .unwrap();

    for drive in [DriveKind::Hdd, DriveKind::Unknown] {
        let opts = Options {
            threads: 4,
            drive,
            ..Options::default()
        };
        let dest = dir.path().join(format!("out-{}", drive.name()));
        extract_file(&archive, &dest, &opts).unwrap();
        assert_eq!(walk(&dest.join("src")), walk(&src));
    }
}

#[test]
fn a_throughput_limit_changes_the_pace_and_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("data.bin");
    let data = sample(4 << 20);
    fs::write(&input, &data).unwrap();
    let compress = |name: &str, opts: &Options| {
        let out = dir.path().join(name);
        create_file(Format::Gz, std::slice::from_ref(&input), &out, opts).unwrap();
        out
    };
    let base = Options {
        threads: 2,
        level: Some(1),
        ..Options::default()
    };
    let free = compress("free.gz", &base);

    // Slow for the first half, then the limit is lifted while the operation runs.
    let control = Control::new();
    control.set_rate_limit(16 << 20);
    let c = control.clone();
    let limited = Options {
        control: control.clone(),
        progress: Some(on_quarters(move |share| {
            if share >= 2 {
                c.set_rate_limit(0);
            }
        })),
        ..base
    };
    let slowed = compress("slow.gz", &limited);
    assert_eq!(control.rate_limit(), 0);
    assert_eq!(fs::read(free).unwrap(), fs::read(slowed).unwrap());
}

#[test]
fn cancel_during_a_live_gzip_run_leaves_neither_output_nor_part_file() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("data.bin");
    fs::write(&input, sample(12 << 20)).unwrap();

    let control = Control::new();
    let c = control.clone();
    let opts = Options {
        threads: 4,
        level: Some(1),
        control: control.clone(),
        progress: Some(on_quarters(move |share| {
            if share >= 2 {
                c.cancel();
            }
        })),
        ..Options::default()
    };
    let out = dir.path().join("x.gz");
    let err = create_file(Format::Gz, &[input], &out, &opts).unwrap_err();
    assert!(matches!(err, Error::Cancelled), "{err:?}");
    assert!(control.is_cancelled());
    assert!(!out.exists());
    assert!(!dir.path().join("x.gz.part").exists());
}

#[test]
fn cancel_reaches_workers_that_are_held_back() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    make_tree(&src, 300);
    let archive = dir.path().join("a.zip");
    create_file(Format::Zip, &[src], &archive, &Options::default()).unwrap();

    let control = Control::new();
    control.set_threads(1);
    let c = control.clone();
    let opts = Options {
        threads: 4,
        drive: DriveKind::Ssd,
        control: control.clone(),
        progress: Some(on_quarters(move |share| {
            if share >= 1 {
                c.cancel();
            }
        })),
        ..Options::default()
    };
    let dest = dir.path().join("out");
    let err = extract_file(&archive, &dest, &opts).unwrap_err();
    assert!(matches!(err, Error::Cancelled), "{err:?}");
    assert!(!has_part_file(&dest));
}
