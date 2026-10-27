//! Zip backend, built on the `zip` crate.

use std::io::{self, Read, Seek, Write};

use zip::result::ZipError;
use zip::write::SimpleFileOptions;
use zip::{ZipArchive, ZipWriter};

use crate::archive::{ArchiveReader, ArchiveWriter, Entry};
use crate::error::{Error, Result};

/// Size of the one copy buffer each reader/writer allocates and reuses.
const BUF_SIZE: usize = 64 * 1024;

fn map_error(error: ZipError) -> Error {
    match error {
        ZipError::Io(e) => Error::Io(e),
        ZipError::InvalidPassword => Error::InvalidPassword,
        ZipError::UnsupportedArchive(message) if message == ZipError::PASSWORD_REQUIRED => {
            Error::PasswordRequired
        }
        other => Error::Corrupt(other.to_string()),
    }
}

fn copy_stream(input: &mut dyn Read, output: &mut dyn Write, buf: &mut [u8]) -> io::Result<u64> {
    let mut total = 0u64;
    loop {
        let n = match input.read(buf) {
            Ok(0) => return Ok(total),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        output.write_all(&buf[..n])?;
        total += n as u64;
    }
}

pub struct ZipReader<R: Read + Seek> {
    archive: ZipArchive<R>,
    buf: Box<[u8]>,
}

impl<R: Read + Seek> ZipReader<R> {
    pub fn new(input: R) -> Result<Self> {
        Ok(Self {
            archive: ZipArchive::new(input).map_err(map_error)?,
            buf: vec![0; BUF_SIZE].into_boxed_slice(),
        })
    }
}

impl<R: Read + Seek> ArchiveReader for ZipReader<R> {
    fn entries(&mut self) -> Result<Vec<Entry>> {
        let mut entries = Vec::with_capacity(self.archive.len());
        for index in 0..self.archive.len() {
            // `by_index_raw` does not decrypt or decompress, so listing an
            // encrypted archive works without a password.
            let file = self.archive.by_index_raw(index).map_err(map_error)?;
            entries.push(Entry {
                name: file.name().to_owned(),
                is_dir: file.is_dir(),
                size: file.size(),
                compressed_size: file.compressed_size(),
                unix_mode: file.unix_mode(),
            });
        }
        Ok(entries)
    }

    fn read_entry(
        &mut self,
        index: usize,
        password: Option<&[u8]>,
        out: &mut dyn Write,
    ) -> Result<u64> {
        let mut file = match password {
            Some(password) => self.archive.by_index_decrypt(index, password),
            None => self.archive.by_index(index),
        }
        .map_err(map_error)?;
        copy_stream(&mut file, out, &mut self.buf).map_err(Error::Io)
    }
}

pub struct ZipCreator<W: Write + Seek> {
    zip: ZipWriter<W>,
    buf: Box<[u8]>,
}

impl<W: Write + Seek> ZipCreator<W> {
    pub fn new(output: W) -> Self {
        Self {
            zip: ZipWriter::new(output),
            buf: vec![0; BUF_SIZE].into_boxed_slice(),
        }
    }
}

impl<W: Write + Seek> ArchiveWriter for ZipCreator<W> {
    fn add_dir(&mut self, name: &str) -> Result<()> {
        self.zip
            .add_directory(name, SimpleFileOptions::default())
            .map_err(map_error)
    }

    fn add_file(
        &mut self,
        name: &str,
        unix_mode: Option<u32>,
        size: u64,
        data: &mut dyn Read,
    ) -> Result<u64> {
        // The compression method is left at its default, which is Deflate
        // because that is the only codec compiled in.
        let mut options = SimpleFileOptions::default().large_file(size >= u64::from(u32::MAX));
        if let Some(mode) = unix_mode {
            options = options.unix_permissions(mode);
        }
        self.zip.start_file(name, options).map_err(map_error)?;
        copy_stream(data, &mut self.zip, &mut self.buf).map_err(Error::Io)
    }

    fn finish(self: Box<Self>) -> Result<()> {
        let ZipCreator { zip, .. } = *self;
        zip.finish().map_err(map_error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Cursor;

    use super::*;
    use crate::archive::open_reader;
    use crate::error::Error;
    use crate::format::{Format, detect};
    use crate::ops::{ExtractOptions, compress_to_file, extract_all, extract_file};
    use crate::progress::NoObserver;
    use crate::testutil::TempDir;

    fn sample_bytes(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn compress_then_extract_roundtrip() {
        let tmp = TempDir::new();
        let input = tmp.path().join("in");
        fs::create_dir_all(input.join("sub")).unwrap();
        fs::create_dir_all(input.join("empty")).unwrap();
        fs::write(input.join("a.txt"), b"hello zip").unwrap();
        fs::write(input.join("sub").join("b.bin"), sample_bytes(300_000)).unwrap();
        fs::write(input.join("sub").join("zero.txt"), b"").unwrap();

        let archive = tmp.path().join("out.zip");
        let summary = compress_to_file(
            Format::Zip,
            &archive,
            std::slice::from_ref(&input),
            false,
            &mut NoObserver,
        )
        .unwrap();
        assert_eq!(summary.entries, 6);
        assert_eq!(summary.bytes, 9 + 300_000);
        assert!(archive.is_file());
        assert!(!tmp.path().join("out.zip.part").exists());

        let mut file = fs::File::open(&archive).unwrap();
        assert_eq!(detect(&mut file).unwrap(), Some(Format::Zip));

        let dest = tmp.path().join("dest");
        let extracted =
            extract_file(&archive, &dest, &ExtractOptions::default(), &mut NoObserver).unwrap();
        assert_eq!(extracted.entries, 6);
        assert_eq!(extracted.bytes, 9 + 300_000);
        assert_eq!(fs::read(dest.join("in/a.txt")).unwrap(), b"hello zip");
        assert_eq!(
            fs::read(dest.join("in/sub/b.bin")).unwrap(),
            sample_bytes(300_000)
        );
        assert_eq!(fs::read(dest.join("in/sub/zero.txt")).unwrap(), b"");
        assert!(dest.join("in/empty").is_dir());
    }

    #[test]
    fn listing_reports_names_and_sizes() {
        let tmp = TempDir::new();
        let input = tmp.path().join("one.txt");
        fs::write(&input, b"12345").unwrap();
        let archive = tmp.path().join("one.zip");
        compress_to_file(Format::Zip, &archive, &[input], false, &mut NoObserver).unwrap();

        let mut reader = open_reader(fs::File::open(&archive).unwrap()).unwrap();
        let entries = reader.entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "one.txt");
        assert_eq!(entries[0].size, 5);
        assert!(!entries[0].is_dir);
    }

    #[test]
    fn zip_slip_entries_are_rejected_before_writing_anything() {
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        zip.start_file("fine.txt", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"ok").unwrap();
        zip.start_file("../evil.txt", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"pwned").unwrap();
        let bytes = zip.finish().unwrap().into_inner();

        let tmp = TempDir::new();
        let dest = tmp.path().join("dest");
        let mut reader = open_reader(Cursor::new(bytes)).unwrap();
        let result = extract_all(
            &mut *reader,
            &dest,
            &ExtractOptions::default(),
            &mut NoObserver,
        );

        assert!(matches!(result, Err(Error::UnsafePath(_))));
        assert!(!tmp.path().join("evil.txt").exists());
        assert!(!dest.join("fine.txt").exists());
    }

    #[test]
    fn damaged_archive_is_reported_as_corrupt() {
        let result = open_reader(Cursor::new(b"PK\x03\x04 this is not a zip".to_vec()));
        assert!(matches!(result.err(), Some(Error::Corrupt(_))));
    }
}
