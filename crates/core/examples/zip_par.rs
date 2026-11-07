//! EXPERIMENT (zip-upgrade-ab): what upstream's own parallel creation does, for the A/B report.
//!
//! zip_par <input-dir> <output.zip> <deflate|zstd> [capacity]
//!
//! Builds the full entry list (upstream's API takes one), then calls
//! `ZipWriter::par_append_from_paths`. Zstar's own path validation, limits, cancellation and
//! ordering are not in play here: this is a measurement of the upstream primitive only.

use std::fs::File;
use std::io::BufWriter;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use zip::write::{FileOptions, SimpleFileOptions};
use zip::{CompressionMethod, ZipWriter};

fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, PathBuf)>) {
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .expect("read_dir")
        .map(|e| e.expect("entry").path())
        .collect();
    names.sort();
    for p in names {
        if p.is_dir() {
            walk(&p, root, out);
        } else {
            let rel = p
                .strip_prefix(root)
                .expect("prefix")
                .to_string_lossy()
                .replace('\\', "/");
            out.push((rel, p));
        }
    }
}

fn peak_rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmHWM:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        })
        .unwrap_or(0)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (input, output, method) = (
        PathBuf::from(&args[0]),
        PathBuf::from(&args[1]),
        args[2].as_str(),
    );
    let method = match method {
        "zstd" => CompressionMethod::Zstd,
        _ => CompressionMethod::Deflated,
    };
    let start = std::time::Instant::now();
    let mut files = Vec::new();
    walk(&input, &input, &mut files);
    let options: SimpleFileOptions = FileOptions::default().compression_method(method);
    let entries: Vec<_> = files
        .into_iter()
        .map(|(name, path)| (name, path, options))
        .collect();
    let n = entries.len();
    let mut zw = ZipWriter::new(BufWriter::new(File::create(&output).expect("create")));
    match args
        .get(3)
        .and_then(|c| c.parse::<usize>().ok())
        .and_then(NonZeroUsize::new)
    {
        Some(cap) => zw.par_append_from_paths_with_capacity(entries, cap),
        None => zw.par_append_from_paths(entries),
    }
    .expect("par_append");
    zw.finish().expect("finish");
    println!(
        "entries={n} wall_ms={} peak_rss_kib={} size={}",
        start.elapsed().as_millis(),
        peak_rss_kib(),
        std::fs::metadata(&output).map(|m| m.len()).unwrap_or(0)
    );
}
