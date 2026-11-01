//! Multithreaded gzip: the output must be an ordinary gzip file that reads back byte for byte,
//! whatever the size of the input relative to the internal block size.
#![cfg(feature = "gzip")]

mod common;

use std::fs;
use std::process::Command;

use common::*;
use zstar_core::{Format, Options, create_file, extract_file};

const BLOCK: usize = 256 * 1024;

/// Half text-like, half random, so both compressible and stored-like blocks occur.
fn sample(len: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(len);
    let mut i = 0u64;
    while data.len() < len {
        if (data.len() / 100_000) % 2 == 0 {
            data.extend_from_slice(format!("line {i} of some repetitive text\n").as_bytes());
        } else {
            data.extend_from_slice(&pseudo(4096, i));
        }
        i += 1;
    }
    data.truncate(len);
    data
}

fn compress(data: &[u8], threads: usize, format: Format) -> Vec<u8> {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("data.bin");
    write(&src, data);
    let out = tmp.path().join(format!("a.{}", ext(format)));
    let opts = Options {
        threads,
        ..Options::default()
    };
    create_file(format, &[src], &out, &opts).unwrap();
    assert!(find_part_files(tmp.path()).is_empty());
    fs::read(&out).unwrap()
}

#[test]
fn every_size_around_the_block_boundaries_round_trips() {
    for len in [
        0,
        1,
        BLOCK - 1,
        BLOCK,
        BLOCK + 1,
        4 * BLOCK - 1,
        4 * BLOCK, // exactly one wave for two threads
        4 * BLOCK + 1,
        9 * BLOCK + 12345,
    ] {
        let data = sample(len);
        for threads in [2, 3] {
            let tmp = tempfile::tempdir().unwrap();
            let gz = tmp.path().join("data.bin.gz");
            fs::write(&gz, compress(&data, threads, Format::Gz)).unwrap();
            let dest = tmp.path().join("out");
            extract_file(&gz, &dest, &Options::default()).unwrap();
            assert_eq!(
                fs::read(dest.join("data.bin")).unwrap(),
                data,
                "len {len}, {threads} threads"
            );
        }
    }
}

#[test]
fn the_real_gzip_tool_accepts_the_output_and_size_matches_one_thread() {
    let data = sample(6 * BLOCK + 99);
    let parallel = compress(&data, 4, Format::Gz);
    let single = compress(&data, 1, Format::Gz);
    // Blocks are primed with the previous 32 KiB, so the loss is only the sync markers.
    let (p, s) = (parallel.len() as f64, single.len() as f64);
    assert!(p <= s * 1.01, "parallel {p} vs single {s}");

    let tmp = tempfile::tempdir().unwrap();
    let gz = tmp.path().join("p.gz");
    fs::write(&gz, &parallel).unwrap();
    if let Ok(out) = Command::new("gzip").arg("-dc").arg(&gz).output() {
        assert!(out.status.success(), "gzip rejected the output");
        assert_eq!(out.stdout, data);
    }
}

#[test]
fn tar_gz_uses_the_same_writer() {
    if zstar_core::handler(Format::TarGz).is_err() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("proj");
    make_tree(&src);
    let out = tmp.path().join("a.tar.gz");
    let opts = Options {
        threads: 4,
        ..Options::default()
    };
    create_file(Format::TarGz, std::slice::from_ref(&src), &out, &opts).unwrap();
    let dest = tmp.path().join("dest");
    extract_file(&out, &dest, &Options::default()).unwrap();
    assert_eq!(snapshot(&src), snapshot(&dest.join("proj")));
    if let Ok(t) = Command::new("gzip").arg("-t").arg(&out).output() {
        assert!(t.status.success());
    }
}
