//! The real RAR fixtures (koni_rar, MIT, see tests/fixtures/rar/LICENSE-koni_rar) listed and
//! extracted through the common interface. Each file ends as one of three outcomes: extracts OK,
//! typed error, or wrong output. Content is checked by CRC-32 only: the expected value is the
//! one stored in the archive's own file header, read here, never taken from another tool.
//!
//! The test prints one line per fixture (`cargo test ... --test rar_fixtures -- --nocapture`)
//! and fails if an outcome differs from `PINNED`, so a change in libarchive or in our handler
//! shows up. A pinned `wrong` or `error:` is a recorded fact, not an approval: the report in
//! docs/reports/rar-libarchive-v1.md lists them as they are.
#![cfg(feature = "rar")]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use zstar_core::{Error, Options, extract_file, list_file};

/// Outcome class per fixture: `ok`, `wrong`, or `error:<Error variant>`.
const PINNED: &[(&str, &str)] = &[
    ("best_solid.rar", "error:UnsupportedFormat"),
    ("enc_rar4.rar", "error:EncryptedUnsupported"),
    ("enc_rar4_store.rar", "error:EncryptedUnsupported"),
    ("encrypted.rar", "error:EncryptedUnsupported"),
    ("encrypted_compressed.rar", "error:EncryptedUnsupported"),
    ("encrypted_headers.rar", "error:EncryptedUnsupported"),
    ("encrypted_solid.rar", "error:EncryptedUnsupported"),
    ("filter_audio.rar", "ok"),
    ("filter_delta.rar", "ok"),
    ("filter_e8.rar", "ok"),
    ("filter_rgb.rar", "ok"),
    ("hp_rar4.rar", "error:EncryptedUnsupported"),
    ("hp_rar4_store.rar", "error:EncryptedUnsupported"),
    ("normal.rar", "ok"),
    ("ppmd_rar4.rar", "ok"),
    ("ppmd_rar4_runs.rar", "error:InvalidArchive"),
    ("ppmd_switch.rar", "ok"),
    ("rar2_audio.rar", "error:InvalidArchive"),
    ("rar2_lz.rar", "error:InvalidArchive"),
    ("rar2_lz_repeat.rar", "error:InvalidArchive"),
    ("rar2_solid.rar", "error:UnsupportedFormat"),
    ("solid_ppmd.rar", "error:UnsupportedFormat"),
    ("solid_rar4.rar", "error:UnsupportedFormat"),
    ("store.rar", "ok"),
];

const CRC_TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                (c >> 1) ^ 0xEDB8_8320
            } else {
                c >> 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

fn crc32(data: &[u8]) -> u32 {
    !data.iter().fold(!0u32, |c, &b| {
        CRC_TABLE[((c ^ u32::from(b)) & 0xff) as usize] ^ (c >> 8)
    })
}

/// A file entry as the archive's own header declares it.
struct Declared {
    name: String,
    size: u64,
    crc: Option<u32>,
    encrypted: bool,
}

#[derive(Default)]
struct Headers {
    generation: &'static str,
    files: Vec<Declared>,
    dirs: usize,
    encrypted_headers: bool,
    solid: bool,
    volume: bool,
    /// Compression facts for the report: RAR4 `unpver/method`, RAR5 `version/method/dict`.
    codecs: BTreeSet<String>,
}

fn le16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn le32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn vint(b: &[u8], at: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for shift in (0..70).step_by(7) {
        let x = *b.get(*at)?;
        *at += 1;
        v |= u64::from(x & 0x7f) << shift;
        if x & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

fn norm(name: &[u8]) -> String {
    String::from_utf8_lossy(name).replace('\\', "/")
}

fn parse_rar4(b: &[u8]) -> Option<Headers> {
    let mut h = Headers {
        generation: "RAR4",
        ..Headers::default()
    };
    let mut p = 7;
    while p + 7 <= b.len() {
        let kind = b[p + 2];
        let flags = le16(b, p + 3)?;
        let head = usize::from(le16(b, p + 5)?);
        let add = if flags & 0x8000 != 0 {
            u64::from(le32(b, p + 7)?)
        } else {
            0
        };
        match kind {
            0x73 => {
                h.volume |= flags & 0x01 != 0;
                h.solid |= flags & 0x08 != 0;
                if flags & 0x80 != 0 {
                    h.encrypted_headers = true;
                    return Some(h);
                }
            }
            0x74 => {
                let big = flags & 0x100 != 0;
                let name_len = usize::from(le16(b, p + 26)?);
                let name_at = p + 32 + if big { 8 } else { 0 };
                let mut size = u64::from(le32(b, p + 11)?);
                if big {
                    size |= u64::from(le32(b, p + 32)?) << 32;
                }
                h.codecs
                    .insert(format!("unpver {}/method {:#x}", b[p + 24], b[p + 25]));
                if flags & 0xe0 == 0xe0 {
                    h.dirs += 1;
                } else {
                    let name = norm(b.get(name_at..name_at + name_len)?);
                    h.files.push(Declared {
                        name,
                        size,
                        crc: le32(b, p + 16),
                        encrypted: flags & 0x04 != 0,
                    });
                }
            }
            0x7b => break,
            _ => {}
        }
        p += head + usize::try_from(add).ok()?;
    }
    Some(h)
}

fn parse_rar5(b: &[u8]) -> Option<Headers> {
    let mut h = Headers {
        generation: "RAR5",
        ..Headers::default()
    };
    let mut p = 8;
    while p + 4 < b.len() {
        let mut q = p + 4;
        let size = usize::try_from(vint(b, &mut q)?).ok()?;
        let body = q;
        let end = body.checked_add(size)?;
        let kind = vint(b, &mut q)?;
        let flags = vint(b, &mut q)?;
        let extra = if flags & 1 != 0 {
            usize::try_from(vint(b, &mut q)?).ok()?
        } else {
            0
        };
        let data = if flags & 2 != 0 { vint(b, &mut q)? } else { 0 };
        match kind {
            1 => {
                let af = vint(b, &mut q)?;
                h.volume |= af & 1 != 0;
                h.solid |= af & 4 != 0;
            }
            2 => {
                let ff = vint(b, &mut q)?;
                let unpacked = vint(b, &mut q)?;
                vint(b, &mut q)?;
                if ff & 2 != 0 {
                    q += 4;
                }
                let crc = if ff & 4 != 0 { le32(b, q) } else { None };
                if ff & 4 != 0 {
                    q += 4;
                }
                let comp = vint(b, &mut q)?;
                vint(b, &mut q)?;
                let name_len = usize::try_from(vint(b, &mut q)?).ok()?;
                let name = norm(b.get(q..q + name_len)?);
                h.codecs.insert(format!(
                    "v{}/method {}/dict {} KiB",
                    comp & 0x3f,
                    (comp >> 7) & 7,
                    128u64 << ((comp >> 10) & 0xf)
                ));
                // Extra records sit at the end of the header; type 1 is file encryption.
                let mut e = end.checked_sub(extra)?;
                let mut encrypted = false;
                while e < end {
                    let rec = usize::try_from(vint(b, &mut e)?).ok()?;
                    let rec_end = e.checked_add(rec)?;
                    encrypted |= vint(b, &mut e)? == 1;
                    e = rec_end;
                }
                if ff & 1 != 0 {
                    h.dirs += 1;
                } else {
                    h.files.push(Declared {
                        name,
                        size: unpacked,
                        crc,
                        encrypted,
                    });
                }
            }
            4 => {
                h.encrypted_headers = true;
                return Some(h);
            }
            5 => break,
            _ => {}
        }
        p = end.checked_add(usize::try_from(data).ok()?)?;
    }
    Some(h)
}

fn parse(b: &[u8]) -> Option<Headers> {
    if b.starts_with(b"Rar!\x1a\x07\x01\x00") {
        parse_rar5(b)
    } else if b.starts_with(b"Rar!\x1a\x07\x00") {
        parse_rar4(b)
    } else {
        None
    }
}

fn variant(e: &Error) -> String {
    let d = format!("{e:?}");
    let end = d.find(['(', ' ', '{']).unwrap_or(d.len());
    d[..end].to_owned()
}

/// `(class, detail)` for one fixture.
fn run(path: &Path, h: &Headers) -> (String, String) {
    let opts = Options::default();
    let mut listed = Vec::new();
    let list_err = list_file(path, &opts, &mut |i| listed.push(i.name)).err();
    // Extraction runs even when listing failed: what it leaves in the destination is recorded.
    let dest = tempfile::tempdir().unwrap();
    let extracted = extract_file(path, dest.path(), &opts);
    let left: Vec<String> = fs::read_dir(dest.path()).map_or_else(
        |_| Vec::new(),
        |d| {
            d.map(|x| x.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        },
    );
    let summary = match (list_err, extracted) {
        (Some(l), Err(e)) => {
            return (
                format!("error:{}", variant(&l)),
                format!("list: {l}; extract: {e}; left in destination: {left:?}"),
            );
        }
        (Some(l), Ok(_)) => {
            return (
                "wrong".into(),
                format!("list failed ({l}) but extract succeeded"),
            );
        }
        (None, Err(e)) => {
            return (
                format!("error:{}", variant(&e)),
                format!("extract: {e}; left in destination: {left:?}"),
            );
        }
        (None, Ok(s)) => s,
    };
    let mut wrong = Vec::new();
    if listed.len() != h.files.len() + h.dirs {
        wrong.push(format!(
            "listed {} entries, headers declare {}",
            listed.len(),
            h.files.len() + h.dirs
        ));
    }
    if summary.files != h.files.len() as u64 {
        wrong.push(format!(
            "extracted {} files, headers declare {}",
            summary.files,
            h.files.len()
        ));
    }
    for f in &h.files {
        let got = fs::read(dest.path().join(&f.name));
        match (got, f.crc, f.encrypted) {
            (Err(e), ..) => wrong.push(format!("{}: not extracted ({e})", f.name)),
            (Ok(_), _, true) => wrong.push(format!("{}: encrypted entry extracted", f.name)),
            (Ok(d), Some(want), false) => {
                if d.len() as u64 != f.size || crc32(&d) != want {
                    wrong.push(format!(
                        "{}: {} B crc {:08x}, header says {} B crc {want:08x}",
                        f.name,
                        d.len(),
                        crc32(&d),
                        f.size
                    ));
                }
            }
            (Ok(_), None, false) => {
                wrong.push(format!("{}: header has no CRC-32 to check", f.name))
            }
        }
    }
    if wrong.is_empty() {
        (
            "ok".into(),
            format!("{} files, {} dirs, CRC-32 matches", h.files.len(), h.dirs),
        )
    } else {
        ("wrong".into(), wrong.join("; "))
    }
}

fn fixtures() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/rar");
    let mut v: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "rar"))
        .collect();
    v.sort();
    v
}

#[test]
fn every_fixture_ends_as_ok_typed_error_or_wrong_output() {
    let files = fixtures();
    assert_eq!(files.len(), 24, "expected the 24 koni_rar fixtures");
    let mut mismatches = Vec::new();
    for path in &files {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let bytes = fs::read(path).unwrap();
        let h = parse(&bytes).unwrap_or_else(|| panic!("{name}: header walk failed"));
        let (class, detail) = run(path, &h);
        println!(
            "{name} | {} solid={} volume={} enc_headers={} | {} | {class} | {detail}",
            h.generation,
            h.solid,
            h.volume,
            h.encrypted_headers,
            h.codecs.iter().cloned().collect::<Vec<_>>().join(", "),
        );
        match PINNED.iter().find(|(n, _)| *n == name) {
            Some((_, want)) if *want == class => {}
            Some((_, want)) => mismatches.push(format!("{name}: pinned {want}, got {class}")),
            None => mismatches.push(format!("{name}: not pinned, got {class}")),
        }
    }
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}
