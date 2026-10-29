//! Shared helpers for the integration tests: tree builders and comparers, plus hand-rolled
//! zip and tar writers that can produce archives no well-behaved library would write.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use zstar_core::{Format, handler};

pub fn all_formats() -> Vec<Format> {
    Format::ALL
        .into_iter()
        .filter(|&f| handler(f).is_ok())
        .collect()
}

pub fn ext(format: Format) -> &'static str {
    format.extensions()[0]
}

/// Deterministic, poorly compressible bytes (xorshift64*).
pub fn pseudo(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        out.extend_from_slice(&x.wrapping_mul(0x2545_F491_4F6C_DD1D).to_le_bytes());
    }
    out.truncate(len);
    out
}

pub fn write(path: &Path, data: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, data).unwrap();
}

/// A tree with nested directories, empty files and directories, a 5 MiB file, Unicode names,
/// 300 small files (to exercise parallel zip compression) and, on Unix, modes and symlinks.
pub fn make_tree(root: &Path) {
    write(&root.join("top.txt"), b"hello");
    write(&root.join("empty.txt"), b"");
    write(&root.join("dir/sub/deep.bin"), &pseudo(5 << 20, 7));
    write(&root.join("dir/ünï-日本.txt"), "unicode name".as_bytes());
    write(&root.join("dir/zeros.bin"), &vec![0u8; 3 << 20]);
    fs::create_dir_all(root.join("emptydir")).unwrap();
    for i in 0..300 {
        write(
            &root.join(format!("many/f{i:03}.txt")),
            &pseudo(100 + i * 7, i as u64),
        );
    }
    let when = UNIX_EPOCH + Duration::from_secs(1_592_222_222); // 2020-06-15, even seconds
    fs::File::options()
        .write(true)
        .open(root.join("top.txt"))
        .unwrap()
        .set_modified(when)
        .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        write(&root.join("dir/run.sh"), b"#!/bin/sh\necho hi\n");
        fs::set_permissions(root.join("dir/run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("sub/deep.bin", root.join("dir/link")).unwrap();
        std::os::unix::fs::symlink("../../top.txt", root.join("dir/sub/to_top")).unwrap();
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Node {
    Dir,
    File { len: u64, hash: u64, exec: bool },
    Link(String),
}

fn fnv(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
    })
}

pub fn snapshot(root: &Path) -> BTreeMap<String, Node> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, Node>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let rel = path
                .strip_prefix(base)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let meta = fs::symlink_metadata(&path).unwrap();
            if meta.file_type().is_symlink() {
                let target = fs::read_link(&path).unwrap().to_string_lossy().into_owned();
                out.insert(rel, Node::Link(target));
            } else if meta.is_dir() {
                out.insert(rel, Node::Dir);
                walk(base, &path, out);
            } else {
                #[cfg(unix)]
                let exec = {
                    use std::os::unix::fs::PermissionsExt;
                    meta.permissions().mode() & 0o100 != 0
                };
                #[cfg(not(unix))]
                let exec = false;
                let data = fs::read(&path).unwrap();
                out.insert(
                    rel,
                    Node::File {
                        len: meta.len(),
                        hash: fnv(&data),
                        exec,
                    },
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

pub fn find_part_files(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd {
            let p = e.unwrap().path();
            if p.extension().is_some_and(|x| x == "part") {
                out.push(p.clone());
            }
            if fs::symlink_metadata(&p).unwrap().is_dir() {
                walk(&p, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c ^= u32::from(b);
        for _ in 0..8 {
            c = if c & 1 != 0 {
                (c >> 1) ^ 0xEDB8_8320
            } else {
                c >> 1
            };
        }
    }
    !c
}

/// Builds zip files byte by byte, with stored (uncompressed) entries and arbitrary names.
#[derive(Default)]
pub struct RawZip {
    body: Vec<u8>,
    central: Vec<u8>,
    count: u16,
}

pub const UTF8_FLAG: u16 = 0x0800;

impl RawZip {
    pub fn new() -> Self {
        Self::default()
    }

    /// `mode` is the full Unix `st_mode` (file type bits included); `flags` is the general
    /// purpose bit flag (use [`UTF8_FLAG`] to mark the name as UTF-8).
    pub fn add(&mut self, name: &[u8], data: &[u8], mode: Option<u32>, flags: u16) -> &mut Self {
        let crc = crc32(data);
        let offset = self.body.len() as u32;
        let size = data.len() as u32;
        let mut local = Vec::new();
        local.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        local.extend_from_slice(&20u16.to_le_bytes());
        local.extend_from_slice(&flags.to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes()); // stored
        local.extend_from_slice(&0u16.to_le_bytes()); // time
        local.extend_from_slice(&0x21u16.to_le_bytes()); // date: 1980-01-01
        local.extend_from_slice(&crc.to_le_bytes());
        local.extend_from_slice(&size.to_le_bytes());
        local.extend_from_slice(&size.to_le_bytes());
        local.extend_from_slice(&(name.len() as u16).to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(name);
        self.body.extend_from_slice(&local);
        self.body.extend_from_slice(data);

        let c = &mut self.central;
        c.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        let made_by: u16 = if mode.is_some() { (3 << 8) | 20 } else { 20 };
        c.extend_from_slice(&made_by.to_le_bytes());
        c.extend_from_slice(&20u16.to_le_bytes());
        c.extend_from_slice(&flags.to_le_bytes());
        c.extend_from_slice(&0u16.to_le_bytes());
        c.extend_from_slice(&0u16.to_le_bytes());
        c.extend_from_slice(&0x21u16.to_le_bytes());
        c.extend_from_slice(&crc.to_le_bytes());
        c.extend_from_slice(&size.to_le_bytes());
        c.extend_from_slice(&size.to_le_bytes());
        c.extend_from_slice(&(name.len() as u16).to_le_bytes());
        c.extend_from_slice(&0u16.to_le_bytes()); // extra
        c.extend_from_slice(&0u16.to_le_bytes()); // comment
        c.extend_from_slice(&0u16.to_le_bytes()); // disk
        c.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        c.extend_from_slice(&(mode.unwrap_or(0) << 16).to_le_bytes());
        c.extend_from_slice(&offset.to_le_bytes());
        c.extend_from_slice(name);
        self.count += 1;
        self
    }

    pub fn file(&mut self, name: &str, data: &[u8]) -> &mut Self {
        self.add(name.as_bytes(), data, Some(0o100_644), UTF8_FLAG)
    }

    pub fn symlink(&mut self, name: &str, target: &str) -> &mut Self {
        self.add(
            name.as_bytes(),
            target.as_bytes(),
            Some(0o120_777),
            UTF8_FLAG,
        )
    }

    pub fn finish(&self) -> Vec<u8> {
        let mut out = self.body.clone();
        let central_offset = out.len() as u32;
        out.extend_from_slice(&self.central);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&self.count.to_le_bytes());
        out.extend_from_slice(&self.count.to_le_bytes());
        out.extend_from_slice(&(self.central.len() as u32).to_le_bytes());
        out.extend_from_slice(&central_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }
}

pub struct RawTarEntry<'a> {
    pub name: &'a str,
    /// `b'0'` file, `b'5'` directory, `b'2'` symlink, `b'1'` hard link.
    pub kind: u8,
    pub data: &'a [u8],
    pub link: &'a str,
}

pub fn tar_file<'a>(name: &'a str, data: &'a [u8]) -> RawTarEntry<'a> {
    RawTarEntry {
        name,
        kind: b'0',
        data,
        link: "",
    }
}

pub fn tar_symlink<'a>(name: &'a str, target: &'a str) -> RawTarEntry<'a> {
    RawTarEntry {
        name,
        kind: b'2',
        data: b"",
        link: target,
    }
}

pub fn tar_hardlink<'a>(name: &'a str, target: &'a str) -> RawTarEntry<'a> {
    RawTarEntry {
        name,
        kind: b'1',
        data: b"",
        link: target,
    }
}

/// Builds a ustar archive by hand, so names such as `../evil` are possible.
pub fn raw_tar(entries: &[RawTarEntry<'_>]) -> Vec<u8> {
    let mut out = Vec::new();
    for e in entries {
        let mut h = [0u8; 512];
        h[..e.name.len()].copy_from_slice(e.name.as_bytes());
        h[100..108].copy_from_slice(b"0000644\0");
        h[108..116].copy_from_slice(b"0000000\0");
        h[116..124].copy_from_slice(b"0000000\0");
        h[124..136].copy_from_slice(format!("{:011o}\0", e.data.len()).as_bytes());
        h[136..148].copy_from_slice(b"00000000000\0");
        h[148..156].fill(b' ');
        h[156] = e.kind;
        h[157..157 + e.link.len()].copy_from_slice(e.link.as_bytes());
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
        h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        out.extend_from_slice(&h);
        out.extend_from_slice(e.data);
        out.resize(out.len().div_ceil(512) * 512, 0);
    }
    out.extend_from_slice(&[0u8; 1024]);
    out
}
