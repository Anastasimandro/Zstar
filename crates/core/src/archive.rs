use std::io::{Read, Seek, Write};

use crate::error::{Error, Result};
use crate::format::{self, Format};

/// One item of an archive, as listed without extracting anything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Name as stored in the archive (not yet checked for safety).
    pub name: String,
    pub is_dir: bool,
    /// Uncompressed size in bytes.
    pub size: u64,
    pub compressed_size: u64,
    pub unix_mode: Option<u32>,
}

/// Reads an archive from any `Read + Seek` stream. Implementations know
/// nothing about directories, progress or cancellation: `ops` handles those.
pub trait ArchiveReader {
    /// Lists every entry in archive order. Needs no password.
    fn entries(&mut self) -> Result<Vec<Entry>>;

    /// Streams the content of entry `index` (as returned by `entries`) into
    /// `out` and returns the number of bytes written. A password is ignored
    /// for entries that are not encrypted.
    fn read_entry(
        &mut self,
        index: usize,
        password: Option<&[u8]>,
        out: &mut dyn Write,
    ) -> Result<u64>;
}

/// Writes an archive to any `Write + Seek` stream.
pub trait ArchiveWriter {
    /// Adds a directory entry. `name` uses `/` as separator.
    fn add_dir(&mut self, name: &str) -> Result<()>;

    /// Adds a file whose content is read from `data`. `size` is the expected
    /// length, used to choose between normal and large-file headers.
    fn add_file(
        &mut self,
        name: &str,
        unix_mode: Option<u32>,
        size: u64,
        data: &mut dyn Read,
    ) -> Result<u64>;

    /// Writes the trailing structures and releases the output stream.
    fn finish(self: Box<Self>) -> Result<()>;
}

/// Detects the format of `input` by its magic bytes and opens it.
pub fn open_reader<'a, R: Read + Seek + 'a>(mut input: R) -> Result<Box<dyn ArchiveReader + 'a>> {
    match format::detect(&mut input)? {
        Some(Format::Zip) => open_zip(input),
        Some(other) => Err(Error::Unsupported(other)),
        None => Err(Error::UnknownFormat),
    }
}

/// Starts a new archive of `format` on `output`.
///
/// RAR is deliberately absent here and must stay so: Zstar extracts RAR
/// archives but never creates them.
pub fn create_writer<'a, W: Write + Seek + 'a>(
    format: Format,
    output: W,
) -> Result<Box<dyn ArchiveWriter + 'a>> {
    match format {
        Format::Zip => create_zip(output),
        other => Err(Error::Unsupported(other)),
    }
}

#[cfg(feature = "zip")]
fn open_zip<'a, R: Read + Seek + 'a>(input: R) -> Result<Box<dyn ArchiveReader + 'a>> {
    Ok(Box::new(crate::zip_format::ZipReader::new(input)?))
}

#[cfg(not(feature = "zip"))]
fn open_zip<'a, R: Read + Seek + 'a>(_input: R) -> Result<Box<dyn ArchiveReader + 'a>> {
    Err(Error::Unsupported(Format::Zip))
}

#[cfg(feature = "zip")]
fn create_zip<'a, W: Write + Seek + 'a>(output: W) -> Result<Box<dyn ArchiveWriter + 'a>> {
    Ok(Box::new(crate::zip_format::ZipCreator::new(output)))
}

#[cfg(not(feature = "zip"))]
fn create_zip<'a, W: Write + Seek + 'a>(_output: W) -> Result<Box<dyn ArchiveWriter + 'a>> {
    Err(Error::Unsupported(Format::Zip))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn unknown_data_is_rejected() {
        let result = open_reader(Cursor::new(b"just some text".to_vec()));
        assert!(matches!(result.err(), Some(Error::UnknownFormat)));
    }

    #[test]
    fn known_but_unsupported_format_is_reported() {
        let result = open_reader(Cursor::new(b"Rar!\x1a\x07\x00 and more".to_vec()));
        assert!(matches!(
            result.err(),
            Some(Error::Unsupported(Format::Rar))
        ));
    }

    #[test]
    fn rar_can_never_be_created() {
        let result = create_writer(Format::Rar, Cursor::new(Vec::new()));
        assert!(matches!(
            result.err(),
            Some(Error::Unsupported(Format::Rar))
        ));
    }
}
