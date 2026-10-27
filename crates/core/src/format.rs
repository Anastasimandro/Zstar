use std::fmt;
use std::io::{Read, Seek, SeekFrom};

use crate::error::Result;

/// Archive and compression formats recognised by magic bytes. Detection does
/// not imply support: see `open_reader` and `create_writer`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Format {
    Zip,
    SevenZip,
    Rar,
    Gzip,
    Bzip2,
    Xz,
    Zstd,
    Tar,
}

impl Format {
    pub fn name(self) -> &'static str {
        match self {
            Format::Zip => "zip",
            Format::SevenZip => "7z",
            Format::Rar => "rar",
            Format::Gzip => "gzip",
            Format::Bzip2 => "bzip2",
            Format::Xz => "xz",
            Format::Zstd => "zstd",
            Format::Tar => "tar",
        }
    }
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Bytes needed to recognise every format (the tar magic sits at offset 257).
pub const DETECT_LEN: usize = 262;

/// Recognises a format from the first bytes of a stream.
pub fn detect_bytes(head: &[u8]) -> Option<Format> {
    if head.starts_with(b"PK\x03\x04")
        || head.starts_with(b"PK\x05\x06")
        || head.starts_with(b"PK\x07\x08")
    {
        Some(Format::Zip)
    } else if head.starts_with(b"Rar!\x1a\x07") {
        Some(Format::Rar)
    } else if head.starts_with(&[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C]) {
        Some(Format::SevenZip)
    } else if head.starts_with(&[0x1F, 0x8B]) {
        Some(Format::Gzip)
    } else if head.starts_with(b"BZh") {
        Some(Format::Bzip2)
    } else if head.starts_with(&[0xFD, b'7', b'z', b'X', b'Z', 0x00]) {
        Some(Format::Xz)
    } else if head.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]) {
        Some(Format::Zstd)
    } else if head.len() >= DETECT_LEN && &head[257..262] == b"ustar" {
        Some(Format::Tar)
    } else {
        None
    }
}

/// Recognises the format of a stream and puts the cursor back where it was.
pub fn detect<R: Read + Seek>(input: &mut R) -> Result<Option<Format>> {
    let start = input.stream_position()?;
    let mut head = [0u8; DETECT_LEN];
    let mut filled = 0;
    while filled < head.len() {
        match input.read(&mut head[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    input.seek(SeekFrom::Start(start))?;
    Ok(detect_bytes(&head[..filled]))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn recognises_magic_bytes() {
        assert_eq!(detect_bytes(b"PK\x03\x04rest"), Some(Format::Zip));
        assert_eq!(detect_bytes(b"PK\x05\x06"), Some(Format::Zip));
        assert_eq!(detect_bytes(b"Rar!\x1a\x07\x00"), Some(Format::Rar));
        assert_eq!(detect_bytes(b"Rar!\x1a\x07\x01\x00"), Some(Format::Rar));
        assert_eq!(
            detect_bytes(&[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C, 0, 4]),
            Some(Format::SevenZip)
        );
        assert_eq!(detect_bytes(&[0x1F, 0x8B, 8]), Some(Format::Gzip));
        assert_eq!(detect_bytes(b"BZh9"), Some(Format::Bzip2));
        assert_eq!(
            detect_bytes(&[0xFD, b'7', b'z', b'X', b'Z', 0, 0]),
            Some(Format::Xz)
        );
        assert_eq!(
            detect_bytes(&[0x28, 0xB5, 0x2F, 0xFD, 0]),
            Some(Format::Zstd)
        );
    }

    #[test]
    fn recognises_tar_at_offset_257() {
        let mut block = vec![0u8; 512];
        block[257..262].copy_from_slice(b"ustar");
        assert_eq!(detect_bytes(&block), Some(Format::Tar));
    }

    #[test]
    fn unknown_and_short_inputs() {
        assert_eq!(detect_bytes(b""), None);
        assert_eq!(detect_bytes(b"P"), None);
        assert_eq!(detect_bytes(b"hello world"), None);
    }

    #[test]
    fn detect_restores_the_cursor() {
        let mut data = Cursor::new(b"xxPK\x03\x04tail".to_vec());
        data.set_position(2);
        assert_eq!(detect(&mut data).unwrap(), Some(Format::Zip));
        assert_eq!(data.position(), 2);
    }
}
