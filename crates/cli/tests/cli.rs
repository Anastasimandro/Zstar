use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn zstar(args: &[&str], cwd: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zstar"))
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn compress_list_extract_round_trip_for_every_format() {
    for ext in ["zip", "tar", "tar.gz", "tgz", "tar.zst"] {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path();
        fs::create_dir_all(cwd.join("proj/sub")).unwrap();
        fs::write(cwd.join("proj/a.txt"), "alpha").unwrap();
        fs::write(cwd.join("proj/sub/b.txt"), "beta").unwrap();

        let archive = format!("out.{ext}");
        let o = zstar(&["compress", "-j", "2", &archive, "proj"], cwd);
        assert!(o.status.success(), "{ext}: {}", stderr(&o));

        let o = zstar(&["list", &archive], cwd);
        assert!(o.status.success(), "{ext}: {}", stderr(&o));
        let listing = stdout(&o);
        assert!(
            listing.contains("proj/a.txt") && listing.contains("proj/sub/b.txt"),
            "{ext}: {listing}"
        );

        let o = zstar(&["extract", "-o", "dest", &archive], cwd);
        assert!(o.status.success(), "{ext}: {}", stderr(&o));
        assert_eq!(
            fs::read_to_string(cwd.join("dest/proj/sub/b.txt")).unwrap(),
            "beta"
        );
    }
}

#[test]
fn refuses_to_overwrite_without_force() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path();
    fs::write(cwd.join("f.txt"), "x").unwrap();
    assert!(zstar(&["compress", "o.zip", "f.txt"], cwd).status.success());
    let o = zstar(&["compress", "o.zip", "f.txt"], cwd);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("--force"), "{}", stderr(&o));
    assert!(
        zstar(&["compress", "-f", "o.zip", "f.txt"], cwd)
            .status
            .success()
    );
}

#[test]
fn bomb_limit_gives_a_hint_and_failure_status() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path();
    fs::write(cwd.join("zeros.bin"), vec![0u8; 8 << 20]).unwrap();
    assert!(
        zstar(&["compress", "z.tar.zst", "zeros.bin"], cwd)
            .status
            .success()
    );
    let o = zstar(
        &["extract", "--max-size", "1M", "-o", "d", "z.tar.zst"],
        cwd,
    );
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("--max-size"), "{}", stderr(&o));
    assert!(!cwd.join("d/zeros.bin").exists());
}

#[test]
fn usage_errors_exit_with_status_2() {
    let tmp = tempfile::tempdir().unwrap();
    for args in [
        &["bogus"][..],
        &["extract"],
        &["compress", "x.unknown", "f"],
    ] {
        let o = zstar(args, tmp.path());
        assert_ne!(o.status.code(), Some(0), "{args:?}");
    }
    assert_eq!(zstar(&["bogus"], tmp.path()).status.code(), Some(2));
    assert!(zstar(&["--help"], tmp.path()).status.success());
    assert!(stdout(&zstar(&["--version"], tmp.path())).starts_with("zstar "));
}

#[test]
fn rejects_a_malicious_archive() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path();
    // A minimal tar whose only entry is "../evil.txt".
    let mut h = [0u8; 512];
    h[..11].copy_from_slice(b"../evil.txt");
    h[100..108].copy_from_slice(b"0000644\0");
    h[124..136].copy_from_slice(b"00000000000\0");
    h[148..156].fill(b' ');
    h[156] = b'0';
    h[257..263].copy_from_slice(b"ustar\0");
    let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
    h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    let mut tar = h.to_vec();
    tar.extend_from_slice(&[0u8; 1024]);
    fs::create_dir(cwd.join("work")).unwrap();
    fs::write(cwd.join("evil.tar"), tar).unwrap();

    let o = zstar(&["extract", "-o", "work/out", "evil.tar"], cwd);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("unsafe"), "{}", stderr(&o));
    assert!(!cwd.join("work/evil.txt").exists());
}

#[test]
fn drive_info_explains_its_answer() {
    let tmp = tempfile::tempdir().unwrap();
    // The output file does not exist yet: the closest existing parent is classified.
    let missing = tmp.path().join("later").join("out.zip");
    for path in [tmp.path(), missing.as_path()] {
        let o = zstar(&["drive-info", &path.to_string_lossy()], tmp.path());
        assert!(o.status.success(), "{}", stderr(&o));
        let out = stdout(&o);
        // CI runners are virtual machines, so the class itself is not asserted.
        assert!(
            ["class: ssd", "class: hdd", "class: unknown"]
                .iter()
                .any(|c| out.contains(c)),
            "{out}"
        );
        assert!(out.contains("reason: ") && out.contains("time: "), "{out}");
    }
}

#[test]
fn drive_detection_does_not_break_operations() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path();
    fs::write(cwd.join("a.txt"), "alpha").unwrap();
    // No --drive: the OS is asked for both the source and the (not yet existing) output.
    let o = zstar(&["compress", "a.zip", "a.txt"], cwd);
    assert!(o.status.success(), "{}", stderr(&o));
    let o = zstar(&["extract", "-o", "dest", "a.zip"], cwd);
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(fs::read_to_string(cwd.join("dest/a.txt")).unwrap(), "alpha");
}

/// Parses a `--stats` block into its keys and values, failing on any line that is not
/// `key: value`.
fn parse_stats(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(|l| {
            let (k, v) = l
                .split_once(": ")
                .unwrap_or_else(|| panic!("bad line {l:?}"));
            (k.to_owned(), v.to_owned())
        })
        .collect()
}

fn stats_value<'a>(block: &'a [(String, String)], key: &str) -> &'a str {
    &block.iter().find(|(k, _)| k == key).unwrap().1
}

const STATS_KEYS: [&str; 19] = [
    "stats_version",
    "operation",
    "format",
    "profile",
    "drive_kind",
    "drive_source",
    "same_device",
    "threads_pool",
    "threads_start",
    "wall_ms",
    "read_ms",
    "write_ms",
    "codec_cpu_ms",
    "other_ms",
    "parked_ms",
    "bytes_in",
    "bytes_out",
    "throughput_mib_s",
    "throughput_basis",
];

#[test]
fn stats_block_is_stable_parses_and_is_absent_without_the_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join("proj")).unwrap();
    fs::write(cwd.join("proj/a.txt"), "alpha".repeat(10_000)).unwrap();

    let o = zstar(&["compress", "-j", "2", "plain.tar.gz", "proj"], cwd);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(!stderr(&o).contains("stats_version"), "{}", stderr(&o));

    let o = zstar(
        &["compress", "-j", "2", "--stats", "timed.tar.gz", "proj"],
        cwd,
    );
    assert!(o.status.success(), "{}", stderr(&o));
    let err = stderr(&o);
    let block = err
        .lines()
        .skip_while(|l| !l.starts_with("stats_version"))
        .take(STATS_KEYS.len())
        .collect::<Vec<_>>()
        .join("\n");
    let block = parse_stats(&block);
    let keys: Vec<&str> = block.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, STATS_KEYS);
    assert_eq!(stats_value(&block, "operation"), "compress");
    assert_eq!(stats_value(&block, "format"), "tar.gz");
    assert_eq!(stats_value(&block, "bytes_in"), "50000");
    assert_eq!(stats_value(&block, "same_device"), "yes");
    assert_eq!(stats_value(&block, "drive_source"), "detected");
    for key in ["wall_ms", "read_ms", "write_ms", "other_ms", "parked_ms"] {
        stats_value(&block, key).parse::<f64>().unwrap();
    }
    // The flag only reports: the archive is the same byte for byte.
    assert_eq!(
        fs::read(cwd.join("plain.tar.gz")).unwrap(),
        fs::read(cwd.join("timed.tar.gz")).unwrap()
    );

    let o = zstar(&["extract", "--stats", "-o", "dest", "timed.tar.gz"], cwd);
    assert!(o.status.success(), "{}", stderr(&o));
    let err = stderr(&o);
    let lines = err
        .lines()
        .skip_while(|l| !l.starts_with("stats_version"))
        .take(STATS_KEYS.len())
        .collect::<Vec<_>>()
        .join("\n");
    let block = parse_stats(&lines);
    assert_eq!(stats_value(&block, "operation"), "extract");
    assert_eq!(stats_value(&block, "bytes_out"), "50000");
    assert_eq!(stats_value(&block, "format"), "detected");
}

#[test]
fn no_seq_hint_is_a_documented_diagnostic_that_changes_no_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path();
    fs::create_dir_all(cwd.join("proj")).unwrap();
    fs::write(cwd.join("proj/a.txt"), "alpha".repeat(10_000)).unwrap();

    let o = zstar(&["--help"], cwd);
    assert!(stdout(&o).contains("--no-seq-hint"), "{}", stdout(&o));

    let o = zstar(&["compress", "-j", "2", "hinted.tar.gz", "proj"], cwd);
    assert!(o.status.success(), "{}", stderr(&o));
    let o = zstar(
        &[
            "compress",
            "-j",
            "2",
            "--no-seq-hint",
            "plain.tar.gz",
            "proj",
        ],
        cwd,
    );
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(
        fs::read(cwd.join("hinted.tar.gz")).unwrap(),
        fs::read(cwd.join("plain.tar.gz")).unwrap()
    );

    let o = zstar(
        &["extract", "--no-seq-hint", "-o", "dest", "plain.tar.gz"],
        cwd,
    );
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(
        fs::read(cwd.join("dest/proj/a.txt")).unwrap(),
        "alpha".repeat(10_000).into_bytes()
    );
}

#[test]
fn stats_is_refused_where_it_means_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let o = zstar(&["list", "--stats", "x.zip"], tmp.path());
    assert!(!o.status.success());
    assert!(stderr(&o).contains("--stats applies to compress and extract"));
}
