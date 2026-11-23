//! RAR behaviour that needs no real archive: hand-built headers (the public RAR 5.0 technote
//! layout, CRC included) checked through the common format interface. These are not the
//! fixtures of the results table in docs/reports/rar-libarchive-v1.md; those live in
//! tests/fixtures/rar/ and are tested in `rar_fixtures.rs`.
#![cfg(feature = "rar")]

use std::io::Cursor;

use zstar_core::{Error, Format, Options, Password, detect, extract, list};

fn vint(mut n: u64) -> Vec<u8> {
    let mut b = Vec::new();
    loop {
        let x = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            b.push(x);
            return b;
        }
        b.push(x | 0x80);
    }
}

fn block5(kind: u64, body: &[u8]) -> Vec<u8> {
    let mut inner = vint(kind);
    inner.extend(vint(0));
    inner.extend_from_slice(body);
    let mut sized = vint(inner.len() as u64);
    sized.extend(inner);
    let mut out = crc32(&sized).to_le_bytes().to_vec();
    out.extend(sized);
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

const SIG5: &[u8] = b"Rar!\x1a\x07\x01\x00";
const SIG4: &[u8] = b"Rar!\x1a\x07\x00";

fn rar5(archive_flags: u64) -> Vec<u8> {
    let mut v = SIG5.to_vec();
    v.extend(block5(1, &vint(archive_flags)));
    v.extend(block5(5, &vint(0)));
    v
}

fn rar5_encrypted_headers() -> Vec<u8> {
    let mut v = SIG5.to_vec();
    let mut body = vint(0);
    body.push(15);
    body.extend([0u8; 24]);
    v.extend(block5(4, &body));
    v
}

fn rar4(flags: u16) -> Vec<u8> {
    let mut h = vec![0x73];
    h.extend(flags.to_le_bytes());
    h.extend(13u16.to_le_bytes());
    h.extend([0u8; 6]);
    let mut v = SIG4.to_vec();
    v.extend(((crc32(&h) & 0xffff) as u16).to_le_bytes());
    v.extend(h);
    v
}

fn list_all(data: Vec<u8>, opts: &Options) -> Result<usize, Error> {
    let mut n = 0;
    list(&mut Cursor::new(data), opts, &mut |_| n += 1)?;
    Ok(n)
}

#[test]
fn both_generations_are_detected_by_magic() {
    assert_eq!(detect(&mut Cursor::new(rar5(0))).unwrap(), Format::Rar);
    assert_eq!(detect(&mut Cursor::new(rar4(0))).unwrap(), Format::Rar);
}

#[test]
fn empty_rar5_lists_and_extracts_nothing() {
    assert_eq!(list_all(rar5(0), &Options::default()).unwrap(), 0);
    let dest = tempfile::tempdir().unwrap();
    let s = extract(&mut Cursor::new(rar5(0)), dest.path(), &Options::default()).unwrap();
    assert_eq!((s.files, s.dirs, s.bytes), (0, 0, 0));
}

#[test]
fn volumes_get_a_typed_error() {
    for data in [rar5(1), rar4(1)] {
        let r = list_all(data, &Options::default());
        assert!(
            matches!(r, Err(Error::MultiVolumeUnsupported(Format::Rar))),
            "{r:?}"
        );
    }
}

#[test]
fn encrypted_headers_get_a_typed_error_and_nothing_is_written() {
    for data in [rar5_encrypted_headers(), rar4(0x80)] {
        let r = list_all(data.clone(), &Options::default());
        assert!(
            matches!(r, Err(Error::EncryptedUnsupported(Format::Rar))),
            "{r:?}"
        );
        let dest = tempfile::tempdir().unwrap();
        let e = extract(&mut Cursor::new(data), dest.path(), &Options::default()).unwrap_err();
        assert_eq!(e.to_string(), "encrypted RAR not supported");
        assert_eq!(std::fs::read_dir(dest.path()).unwrap().count(), 0);
    }
}

#[test]
fn a_password_is_refused_never_used_or_asked_for() {
    let opts = Options {
        password: Some(Password::new("hunter2")),
        ..Options::default()
    };
    let e = list_all(rar5(0), &opts).unwrap_err();
    assert!(matches!(e, Error::InvalidOption(_)), "{e:?}");
    assert!(!e.to_string().contains("hunter2"));
}

#[test]
fn damaged_input_is_an_error_not_a_crash() {
    let good = rar5(0);
    // Every truncation and every single-byte corruption of the stub must return, not abort.
    for cut in 0..good.len() {
        let _ = list_all(good[..cut].to_vec(), &Options::default());
    }
    for i in 0..good.len() {
        for flip in [0x01u8, 0x80, 0xff] {
            let mut d = good.clone();
            d[i] ^= flip;
            let _ = list_all(d, &Options::default());
        }
    }
}
