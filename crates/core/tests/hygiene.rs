//! Page-cache hygiene on real operations (ADR 0012, step 3). A recording hook stands in for the
//! OS: what matters here is when core asks, and that asking changes no byte.

#![cfg(feature = "gzip")]

use std::fs;
use std::sync::{Arc, Mutex};

use zstar_core::{CacheHook, CacheOp, Control, Format, Options, create_file, extract_file};

type Log = Arc<Mutex<Vec<(u64, u64, CacheOp)>>>;

fn recording() -> (Options, Log) {
    let log: Log = Arc::default();
    let sink = log.clone();
    let hook: CacheHook = Arc::new(move |_, offset, len, op| {
        sink.lock().unwrap().push((offset, len, op));
    });
    let opts = Options {
        cache_hook: Some(hook),
        ..Options::default()
    };
    (opts, log)
}

/// Compressible but not trivial, so that the archive is a few MiB too.
fn data(len: usize) -> Vec<u8> {
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    (0..len)
        .map(|i| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            if i % 8 < 5 {
                b'a' + (i % 5) as u8
            } else {
                x as u8
            }
        })
        .collect()
}

/// The writeback windows in `log`, which must follow each other with no gap and no overlap,
/// and each one but the last must be released later. Releases of read windows are in the log
/// too (the archive being extracted is big enough to be tracked), so only started windows are
/// looked at. Returns where the last one ends.
fn check_windows(log: &[(u64, u64, CacheOp)]) -> u64 {
    let starts: Vec<_> = log
        .iter()
        .enumerate()
        .filter(|(_, w)| w.2 == CacheOp::StartWriteback)
        .collect();
    let mut next = 0;
    for (n, &(at, &(offset, len, _))) in starts.iter().enumerate() {
        assert_eq!(offset, next, "gap or overlap in {log:?}");
        next = offset + len;
        if n + 1 < starts.len() {
            assert!(
                log[at..].contains(&(offset, len, CacheOp::Release)),
                "window {offset}+{len} never released in {log:?}"
            );
        }
    }
    next
}

#[test]
fn extraction_releases_what_it_wrote_and_writes_the_same_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("big");
    let body = data(24 << 20);
    fs::write(&input, &body).unwrap();
    let gz = dir.path().join("big.gz");
    create_file(
        Format::Gz,
        std::slice::from_ref(&input),
        &gz,
        &Options::default(),
    )
    .unwrap();

    let plain = dir.path().join("plain");
    extract_file(&gz, &plain, &Options::default()).unwrap();

    let (opts, log) = recording();
    let hooked = dir.path().join("hooked");
    extract_file(&gz, &hooked, &opts).unwrap();

    assert_eq!(fs::read(hooked.join("big")).unwrap(), body);
    assert_eq!(
        fs::read(plain.join("big")).unwrap(),
        fs::read(hooked.join("big")).unwrap()
    );
    let log = log.lock().unwrap();
    let covered = check_windows(&log);
    // Everything was handed over: the windows plus the tail reach the end of the file.
    assert_eq!(covered, body.len() as u64, "{log:?}");
    assert!(
        log.iter()
            .filter(|w| w.2 == CacheOp::StartWriteback)
            .count()
            >= 3,
        "{log:?}"
    );
}

#[test]
fn a_bigger_io_depth_makes_bigger_windows() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("big");
    fs::write(&input, data(24 << 20)).unwrap();
    let gz = dir.path().join("big.gz");
    create_file(
        Format::Gz,
        std::slice::from_ref(&input),
        &gz,
        &Options::default(),
    )
    .unwrap();

    let first_window = |depth: usize| {
        let (mut opts, log) = recording();
        opts.control = Control::new();
        opts.control.set_chunk_ceiling(256 << 10);
        opts.control.set_io_depth(depth);
        let out = dir.path().join(format!("out{depth}"));
        extract_file(&gz, &out, &opts).unwrap();
        let log = log.lock().unwrap();
        log.iter()
            .find(|w| w.2 == CacheOp::StartWriteback)
            .map(|w| w.1)
            .unwrap()
    };
    let (small, large) = (first_window(2), first_window(32));
    assert!(large >= 4 * small, "{small} then {large}");
}

#[test]
fn creation_tracks_the_archive_it_writes_and_the_big_source_it_reads() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("big");
    fs::write(&input, data(24 << 20)).unwrap();

    let plain = dir.path().join("plain.gz");
    create_file(
        Format::Gz,
        std::slice::from_ref(&input),
        &plain,
        &Options::default(),
    )
    .unwrap();

    let (opts, log) = recording();
    let hooked = dir.path().join("hooked.gz");
    create_file(Format::Gz, std::slice::from_ref(&input), &hooked, &opts).unwrap();
    assert_eq!(fs::read(&plain).unwrap(), fs::read(&hooked).unwrap());

    let log = log.lock().unwrap();
    let out_len = fs::metadata(&hooked).unwrap().len();
    // The source is only ever released; the archive is started and released.
    assert!(
        log.iter()
            .any(|w| w.2 == CacheOp::Release && w.1 >= 1 << 20)
    );
    assert!(
        log.iter().any(|w| w.2 == CacheOp::StartWriteback),
        "{log:?}"
    );
    assert!(check_windows(&log) <= out_len);
}

#[test]
fn small_files_never_reach_the_hook() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("small");
    fs::write(&input, data(300_000)).unwrap();
    let (opts, log) = recording();
    let gz = dir.path().join("small.gz");
    create_file(Format::Gz, std::slice::from_ref(&input), &gz, &opts).unwrap();
    extract_file(&gz, &dir.path().join("out"), &opts).unwrap();
    assert!(log.lock().unwrap().is_empty());
}
