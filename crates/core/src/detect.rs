//! Format detection by magic bytes (never by file extension).

use std::io::{Read, SeekFrom};

use crate::error::{Error, Result};
use crate::format::{Format, ReadSeek};

const ZIP_MAGICS: [&[u8]; 2] = [b"PK\x03\x04", b"PK\x05\x06"];
const GZIP_MAGIC: &[u8] = &[0x1f, 0x8b];
const ZSTD_MAGIC: &[u8] = &[0x28, 0xb5, 0x2f, 0xfd];
const TAR_BLOCK: usize = 512;

/// True when `block` is a tar header: its checksum field matches the sum of its bytes.
/// This accepts v7, ustar, GNU and pax archives alike.
fn is_tar_header(block: &[u8]) -> bool {
    if block.len() < TAR_BLOCK || block[0] == 0 {
        return false;
    }
    let field = &block[148..156];
    let digits: Vec<u8> = field
        .iter()
        .copied()
        .skip_while(|&b| b == b' ')
        .take_while(|b| (b'0'..=b'7').contains(b))
        .collect();
    let Some(stored) = std::str::from_utf8(&digits)
        .ok()
        .and_then(|s| u32::from_str_radix(s, 8).ok())
    else {
        return false;
    };
    let sum: u32 = block[..TAR_BLOCK]
        .iter()
        .enumerate()
        .map(|(i, &b)| {
            if (148..156).contains(&i) {
                u32::from(b' ')
            } else {
                u32::from(b)
            }
        })
        .sum();
    sum == stored
}

fn read_head(input: &mut dyn Read, buf: &mut [u8]) -> Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match input.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(n)
}

/// First tar block of the decompressed stream, or an error when decompression fails.
#[cfg(any(feature = "gzip", feature = "zstd"))]
fn tar_inside(decoder: &mut dyn Read) -> Result<bool> {
    let mut block = [0u8; TAR_BLOCK];
    // A stream that fails to decode in the first block is simply "not a tar".
    let n = read_head(decoder, &mut block).unwrap_or(0);
    Ok(is_tar_header(&block[..n]))
}

/// Identifies the archive format of `input` and rewinds it to the start.
///
/// Gzip and Zstandard streams are only reported as tar archives when their first
/// decompressed block is a valid tar header; plain `.gz`/`.zst` files are not archives.
pub fn detect(input: &mut dyn ReadSeek) -> Result<Format> {
    input.seek(SeekFrom::Start(0))?;
    let mut head = [0u8; TAR_BLOCK];
    let n = read_head(input, &mut head)?;
    let head = &head[..n];
    input.seek(SeekFrom::Start(0))?;

    if ZIP_MAGICS.iter().any(|m| head.starts_with(m)) {
        return Ok(Format::Zip);
    }
    if head.starts_with(GZIP_MAGIC) {
        #[cfg(feature = "gzip")]
        {
            let mut dec = flate2::read::MultiGzDecoder::new(&mut *input);
            let is_tar = tar_inside(&mut dec)?;
            drop(dec);
            input.seek(SeekFrom::Start(0))?;
            return if is_tar {
                Ok(Format::TarGz)
            } else {
                Err(Error::UnsupportedFormat(
                    "gzip file that does not contain a tar archive".into(),
                ))
            };
        }
        #[cfg(not(feature = "gzip"))]
        return Err(Error::FeatureDisabled(Format::TarGz));
    }
    if head.starts_with(ZSTD_MAGIC) {
        #[cfg(feature = "zstd")]
        {
            let is_tar = match zstd::stream::read::Decoder::new(&mut *input) {
                Ok(mut dec) => tar_inside(&mut dec)?,
                Err(_) => false,
            };
            input.seek(SeekFrom::Start(0))?;
            return if is_tar {
                Ok(Format::TarZst)
            } else {
                Err(Error::UnsupportedFormat(
                    "zstd file that does not contain a tar archive".into(),
                ))
            };
        }
        #[cfg(not(feature = "zstd"))]
        return Err(Error::FeatureDisabled(Format::TarZst));
    }
    if is_tar_header(head) {
        return Ok(Format::Tar);
    }
    Err(Error::UnsupportedFormat(
        "unrecognised file signature".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn tar_header(name: &str) -> [u8; TAR_BLOCK] {
        let mut b = [0u8; TAR_BLOCK];
        b[..name.len()].copy_from_slice(name.as_bytes());
        b[100..107].copy_from_slice(b"0000644");
        b[156] = b'0';
        let sum: u32 = b
            .iter()
            .enumerate()
            .map(|(i, &x)| {
                if (148..156).contains(&i) {
                    32
                } else {
                    u32::from(x)
                }
            })
            .sum();
        b[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        b
    }

    #[test]
    fn recognises_tar_by_checksum() {
        assert!(is_tar_header(&tar_header("hello.txt")));
        let mut corrupt = tar_header("hello.txt");
        corrupt[10] ^= 0xff;
        assert!(!is_tar_header(&corrupt));
        assert!(!is_tar_header(&[0u8; TAR_BLOCK]));
        assert!(!is_tar_header(b"short"));
    }

    #[test]
    fn detects_zip_and_tar_and_rejects_garbage() {
        let mut zip = Cursor::new(b"PK\x03\x04rest".to_vec());
        assert_eq!(detect(&mut zip).unwrap(), Format::Zip);
        let mut empty_zip = Cursor::new(b"PK\x05\x06\0\0\0\0".to_vec());
        assert_eq!(detect(&mut empty_zip).unwrap(), Format::Zip);

        let mut tar = Cursor::new(tar_header("a").to_vec());
        assert_eq!(detect(&mut tar).unwrap(), Format::Tar);
        assert_eq!(tar.position(), 0, "detect must rewind");

        let mut junk = Cursor::new(b"definitely not an archive".to_vec());
        assert!(matches!(
            detect(&mut junk),
            Err(Error::UnsupportedFormat(_))
        ));
        let mut empty = Cursor::new(Vec::new());
        assert!(matches!(
            detect(&mut empty),
            Err(Error::UnsupportedFormat(_))
        ));
    }

    #[cfg(feature = "gzip")]
    #[test]
    fn gzip_is_tar_gz_only_when_it_holds_a_tar() {
        use flate2::{Compression, write::GzEncoder};
        use std::io::Write;
        let gz = |data: &[u8]| {
            let mut e = GzEncoder::new(Vec::new(), Compression::fast());
            e.write_all(data).unwrap();
            Cursor::new(e.finish().unwrap())
        };
        assert_eq!(detect(&mut gz(&tar_header("x"))).unwrap(), Format::TarGz);
        assert!(matches!(
            detect(&mut gz(b"plain text")),
            Err(Error::UnsupportedFormat(_))
        ));
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn zstd_is_tar_zst_only_when_it_holds_a_tar() {
        let z = |data: &[u8]| Cursor::new(zstd::encode_all(data, 1).unwrap());
        assert_eq!(detect(&mut z(&tar_header("x"))).unwrap(), Format::TarZst);
        assert!(matches!(
            detect(&mut z(b"plain text")),
            Err(Error::UnsupportedFormat(_))
        ));
    }
}
