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
