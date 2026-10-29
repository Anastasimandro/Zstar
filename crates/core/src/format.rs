use std::io::{Read, Seek, Write};
use std::path::Path;
use std::time::SystemTime;

use crate::error::{Error, Result};
use crate::extract::Summary;
use crate::options::Options;
use crate::sources::SourceEntry;

/// An archive format known to Zstar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Format {
    /// ZIP.
    Zip,
    /// Uncompressed tar.
    Tar,
    /// Gzip-compressed tar.
    TarGz,
    /// Zstandard-compressed tar.
    TarZst,
}

impl Format {
    /// Every format, whether or not this build supports it.
    pub const ALL: [Format; 4] = [Format::Zip, Format::Tar, Format::TarGz, Format::TarZst];

    /// Short human-readable name, also accepted by [`Format::from_name`].
    pub fn name(self) -> &'static str {
        match self {
            Format::Zip => "zip",
            Format::Tar => "tar",
            Format::TarGz => "tar.gz",
            Format::TarZst => "tar.zst",
        }
    }

    /// Parses a name as returned by [`Format::name`].
    pub fn from_name(name: &str) -> Option<Format> {
        Format::ALL.into_iter().find(|f| f.name() == name)
    }

    /// File extensions that identify the format, lowercase, without the dot.
    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            Format::Zip => &["zip"],
            Format::Tar => &["tar"],
            Format::TarGz => &["tar.gz", "tgz"],
            Format::TarZst => &["tar.zst", "tzst", "tar.zstd"],
        }
    }

    /// Guesses the format from a file name. Used to pick the format when creating.
    pub fn from_path(path: &Path) -> Option<Format> {
        let name = path.file_name()?.to_str()?.to_ascii_lowercase();
        Format::ALL.into_iter().find(|f| {
            f.extensions().iter().any(|ext| {
                name.strip_suffix(ext)
                    .is_some_and(|stem| stem.ends_with('.') && stem.len() > 1)
            })
        })
    }
}

/// What kind of thing an archive entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// Regular file.
    File,
    /// Directory.
    Dir,
    /// Symbolic link.
    Symlink,
    /// Hard link to another entry of the same archive.
    Hardlink,
    /// Anything else (device, FIFO, ...). Never extracted.
    Other,
}

/// One archive entry, as shown by `list`.
#[derive(Debug, Clone)]
pub struct EntryInfo {
    /// Name as stored (after charset decoding), not sanitised.
    pub name: String,
    /// Entry type.
    pub kind: EntryKind,
    /// Uncompressed size in bytes.
    pub size: u64,
    /// Compressed size, when the format records one per entry.
    pub compressed_size: Option<u64>,
    /// Unix permission bits, when recorded.
    pub mode: Option<u32>,
    /// Modification time, when recorded.
    pub mtime: Option<SystemTime>,
    /// Target of a symbolic or hard link, when known.
    pub link_target: Option<String>,
}

/// A readable, seekable, sendable byte stream.
pub trait ReadSeek: Read + Seek + Send {}
impl<T: Read + Seek + Send + ?Sized> ReadSeek for T {}

/// A writable, seekable, sendable byte stream.
pub trait WriteSeek: Write + Seek + Send {}
impl<T: Write + Seek + Send + ?Sized> WriteSeek for T {}

/// The interface every archive format implements.
///
/// Adding a format means adding one implementation of this trait and one arm in
/// [`handler`]; nothing else in the crate changes.
pub trait ArchiveFormat: Sync {
    /// The format this handler implements.
    fn format(&self) -> Format;

    /// Calls `visit` for every entry, without extracting anything.
    fn list(
        &self,
        input: &mut dyn ReadSeek,
        opts: &Options,
        visit: &mut dyn FnMut(EntryInfo),
    ) -> Result<()>;

    /// Extracts into `dest`, enforcing the path-safety rules and the limits in `opts`.
    fn extract(&self, input: &mut dyn ReadSeek, dest: &Path, opts: &Options) -> Result<Summary>;

    /// Writes `sources` as a new archive.
    fn create(
        &self,
        sources: &[SourceEntry],
        out: &mut dyn WriteSeek,
        opts: &Options,
    ) -> Result<()>;
}

/// Returns the handler for `format`, or [`Error::FeatureDisabled`].
pub fn handler(format: Format) -> Result<&'static dyn ArchiveFormat> {
    match format {
        #[cfg(feature = "zip")]
        Format::Zip => Ok(&crate::formats::zip_format::ZipFormat),
        #[cfg(feature = "tar")]
        Format::Tar => Ok(&crate::formats::tar_format::TarFormat::PLAIN),
        #[cfg(all(feature = "tar", feature = "gzip"))]
        Format::TarGz => Ok(&crate::formats::tar_format::TarFormat::GZIP),
        #[cfg(all(feature = "tar", feature = "zstd"))]
        Format::TarZst => Ok(&crate::formats::tar_format::TarFormat::ZSTD),
        #[allow(unreachable_patterns)]
        other => Err(Error::FeatureDisabled(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_path_matches_extensions_case_insensitively() {
        let f = |s: &str| Format::from_path(Path::new(s));
        assert_eq!(f("a.zip"), Some(Format::Zip));
        assert_eq!(f("dir/A.TAR"), Some(Format::Tar));
        assert_eq!(f("a.tar.gz"), Some(Format::TarGz));
        assert_eq!(f("a.tgz"), Some(Format::TarGz));
        assert_eq!(f("a.tar.zst"), Some(Format::TarZst));
        assert_eq!(f("a.tzst"), Some(Format::TarZst));
        assert_eq!(f("a.txt"), None);
        assert_eq!(f("zip"), None);
        assert_eq!(f(".zip"), None);
        assert_eq!(f("a.gz"), None);
    }

    #[test]
    fn names_round_trip() {
        for fmt in Format::ALL {
            assert_eq!(Format::from_name(fmt.name()), Some(fmt));
        }
    }
}
