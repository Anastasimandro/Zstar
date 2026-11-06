//! Measurement harness for metadata-heavy operations (many small or empty files). Not shipped;
//! driven by `scripts/meta-bench.sh`. One operation per process, so peak memory is that
//! operation's own. Prints one `key=value` line on stdout.
//!
//! ```text
//! meta_bench create <zip|7z|tar|tar.gz> <input> <output> [--cancel-ms N] [--threads N]
//! meta_bench extract <archive> <dest> [--cancel-ms N] [--threads N]
//! ```
//!
//! * `first_progress_ms`: from the start of the call to the first progress callback.
//! * `cancel_latency_ms`: from `Control::cancel` to the return of the call (`n/a` when the
//!   operation ended before the cancel).
//! * `peak_rss_kib`: `VmHWM` of this process when the operation returns.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use zstar_core::{Error, Format, Options, create_file, extract_file};

fn status_kib(key: &str) -> u64 {
    fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix(key)?.split_whitespace().next()?.parse().ok())
        })
        .unwrap_or(0)
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut take = |flag: &str| {
        let at = args.iter().position(|a| a == flag)?;
        args.remove(at);
        Some(args.remove(at))
    };
    let cancel_ms: Option<u64> = take("--cancel-ms").and_then(|v| v.parse().ok());
    let threads: usize = take("--threads").and_then(|v| v.parse().ok()).unwrap_or(0);
    let op = args.first().cloned().unwrap_or_default();

    let start = Instant::now();
    let first = Arc::new(AtomicU64::new(0));
    let calls = Arc::new(AtomicU64::new(0));
    let (f, c) = (first.clone(), calls.clone());
    let mut opts = Options {
        threads,
        overwrite: true,
        ..Options::default()
    };
    opts.progress = Some(Arc::new(move |_| {
        let ns = start.elapsed().as_nanos() as u64;
        let _ = f.compare_exchange(0, ns.max(1), Relaxed, Relaxed);
        c.fetch_add(1, Relaxed);
    }));
    let cancelled_at = Arc::new(AtomicU64::new(0));
    if let Some(ms) = cancel_ms {
        let (control, at) = (opts.control.clone(), cancelled_at.clone());
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(ms));
            at.store((start.elapsed().as_nanos() as u64).max(1), Relaxed);
            control.cancel();
        });
    }

    let (what, result) = match op.as_str() {
        "create" => {
            let format = match args[1].as_str() {
                "zip" => Format::Zip,
                "7z" => Format::SevenZ,
                "tar" => Format::Tar,
                "tar.gz" => Format::TarGz,
                other => panic!("unknown format {other}"),
            };
            let input = PathBuf::from(&args[2]);
            let r = create_file(format, &[input], &PathBuf::from(&args[3]), &opts);
            (args[1].clone(), r.map(|()| 0u64))
        }
        "extract" => {
            let r = extract_file(&PathBuf::from(&args[1]), &PathBuf::from(&args[2]), &opts)
                .map(|s| s.files + s.dirs);
            ("extract".to_owned(), r)
        }
        _ => panic!("usage: meta_bench create|extract ..."),
    };
    let end = start.elapsed().as_nanos() as u64;
    let ms = |ns: u64| format!("{:.1}", ns as f64 / 1e6);
    let (outcome, entries) = match &result {
        Ok(n) => ("ok".to_owned(), *n),
        Err(Error::Cancelled) => ("cancelled".to_owned(), 0),
        Err(e) => (format!("error:{e}"), 0),
    };
    let cancel_at = cancelled_at.load(Relaxed);
    let latency = match (&result, cancel_at) {
        (Err(Error::Cancelled), t) if t > 0 => ms(end - t),
        _ => "n/a".to_owned(),
    };
    let first_ns = first.load(Relaxed);
    println!(
        "what={what} outcome={outcome} wall_ms={} first_progress_ms={} progress_calls={} \
         cancel_latency_ms={latency} entries={entries} peak_rss_kib={} end_rss_kib={}",
        ms(end),
        if first_ns == 0 {
            "n/a".to_owned()
        } else {
            ms(first_ns)
        },
        calls.load(Relaxed),
        status_kib("VmHWM:"),
        status_kib("VmRSS:"),
    );
}
