//! Format detection by magic bytes (never by file extension).

use std::io::{Read, SeekFrom};

use crate::error::{Error, Result};
use crate::format::{Format, ReadSeek};
use crate::formats::codec::Codec;
use crate::options::Limits;

const ZIP_MAGICS: [&[u8]; 2] = [b"PK\x03\x04", b"PK\x05\x06"];
const SEVENZ_MAGIC: &[u8] = &[b'7', b'z', 0xbc, 0xaf, 0x27, 0x1c];
const GZIP_MAGIC: &[u8] = &[0x1f, 0x8b];
const ZSTD_MAGIC: &[u8] = &[0x28, 0xb5, 0x2f, 0xfd];
const BZIP2_MAGIC: &[u8] = b"BZh"; // followed by the block size digit '1'..='9'
const XZ_MAGIC: &[u8] = &[0xfd, b'7', b'z', b'X', b'Z', 0x00];
const LZ4_MAGIC: &[u8] = &[0x04, 0x22, 0x4d, 0x18];
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

/// Whether the first block of the decompressed stream is a tar header. A stream that fails
/// to decode in that block is simply "not a tar".
fn tar_inside(decoder: &mut dyn Read) -> Result<bool> {
    let mut block = [0u8; TAR_BLOCK];
    let n = read_head(decoder, &mut block).unwrap_or(0);
    Ok(is_tar_header(&block[..n]))
}

/// The single-file and tar formats that share the compressor `head` starts with, if any.
fn signature(head: &[u8]) -> Option<(Format, Format)> {
    if head.starts_with(GZIP_MAGIC) {
        Some((Format::Gz, Format::TarGz))
    } else if head.starts_with(ZSTD_MAGIC) {
        Some((Format::Zst, Format::TarZst))
    } else if head.starts_with(BZIP2_MAGIC)
        && head.get(3).is_some_and(|b| (b'1'..=b'9').contains(b))
    {
        Some((Format::Bz2, Format::TarBz2))
    } else if head.starts_with(XZ_MAGIC) {
        Some((Format::Xz, Format::TarXz))
    } else if head.starts_with(LZ4_MAGIC) {
        Some((Format::Lz4, Format::TarLz4))
    } else {
        None
    }
}

/// The codec behind a single-file format, when this build has it.
fn codec_for(single: Format) -> Option<Codec> {
    match single {
        #[cfg(feature = "gzip")]
        Format::Gz => Some(Codec::Gzip),
        #[cfg(feature = "zstd")]
        Format::Zst => Some(Codec::Zstd),
        #[cfg(feature = "bzip2")]
        Format::Bz2 => Some(Codec::Bzip2),
        #[cfg(feature = "xz")]
        Format::Xz => Some(Codec::Xz),
        #[cfg(feature = "lz4")]
        Format::Lz4 => Some(Codec::Lz4),
        _ => None,
    }
}

/// Identifies the archive format of `input` and rewinds it to the start.
///
/// Compressed streams are reported as tar archives when their first decompressed block is a
/// valid tar header, and as single compressed files (gz, zst, bz2, xz, lz4) otherwise. Only
/// the magic bytes and that first block are checked, so a corrupt stream is still detected
/// here and fails later, when it is read. Brotli has no magic bytes and is never detected
/// here; see [`detect_named`].
pub fn detect(input: &mut dyn ReadSeek) -> Result<Format> {
    detect_limited(input, Limits::default().max_memory_bytes)
}

/// [`detect`] with the decoder memory limit of the caller, so that a stream that sizes its
/// dictionary at will cannot make detection allocate more than extraction would.
pub(crate) fn detect_limited(input: &mut dyn ReadSeek, max_memory: u64) -> Result<Format> {
    input.seek(SeekFrom::Start(0))?;
    let mut head = [0u8; TAR_BLOCK];
    let n = read_head(input, &mut head)?;
    let head = &head[..n];
    input.seek(SeekFrom::Start(0))?;

    if ZIP_MAGICS.iter().any(|m| head.starts_with(m)) {
        return Ok(Format::Zip);
    }
    if head.starts_with(SEVENZ_MAGIC) {
        #[cfg(feature = "sevenz")]
        return Ok(Format::SevenZ);
        #[cfg(not(feature = "sevenz"))]
        return Err(Error::FeatureDisabled(Format::SevenZ));
    }
    if let Some((single, tar)) = signature(head) {
        let Some(codec) = codec_for(single) else {
            return Err(Error::FeatureDisabled(single));
        };
        let is_tar = codec
            .with_reader(input, max_memory, |reader| tar_inside(reader))
            .unwrap_or(false);
        input.seek(SeekFrom::Start(0))?;
        return Ok(if is_tar { tar } else { single });
    }
    if is_tar_header(head) {
        return Ok(Format::Tar);
    }
    Err(Error::UnsupportedFormat(
        "unrecognised file signature".into(),
    ))
}

/// Like [`detect`], but falls back to the file name for formats that have no signature
/// (Brotli, `.br`). The name is only consulted when the content matches nothing else.
pub fn detect_named(input: &mut dyn ReadSeek, name: Option<&str>) -> Result<Format> {
    detect_named_limited(input, name, Limits::default().max_memory_bytes)
}

pub(crate) fn detect_named_limited(
    input: &mut dyn ReadSeek,
    name: Option<&str>,
    max_memory: u64,
) -> Result<Format> {
    match detect_limited(input, max_memory) {
        Err(Error::UnsupportedFormat(_)) if name.is_some_and(is_brotli_name) => Ok(Format::Br),
        other => other,
    }
}

fn is_brotli_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower
        .strip_suffix(".br")
        .is_some_and(|stem| !stem.is_empty())
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
        let mut seven = Cursor::new(b"7z\xbc\xaf\x27\x1c\0\x04rest".to_vec());
        assert_eq!(detect(&mut seven).unwrap(), Format::SevenZ);
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
        assert_eq!(detect(&mut gz(b"plain text")).unwrap(), Format::Gz);
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn zstd_is_tar_zst_only_when_it_holds_a_tar() {
        let z = |data: &[u8]| Cursor::new(zstd::encode_all(data, 1).unwrap());
        assert_eq!(detect(&mut z(&tar_header("x"))).unwrap(), Format::TarZst);
        assert_eq!(detect(&mut z(b"plain text")).unwrap(), Format::Zst);
    }
}
