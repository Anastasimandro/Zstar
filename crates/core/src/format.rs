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
    /// 7z.
    SevenZ,
    /// Uncompressed tar.
    Tar,
    /// Gzip-compressed tar.
    TarGz,
    /// Zstandard-compressed tar.
    TarZst,
    /// Bzip2-compressed tar.
    TarBz2,
    /// XZ-compressed tar.
    TarXz,
    /// LZ4-compressed tar.
    TarLz4,
    /// A single file compressed with gzip.
    Gz,
    /// A single file compressed with Zstandard.
    Zst,
    /// A single file compressed with bzip2.
    Bz2,
    /// A single file compressed with XZ.
    Xz,
    /// A single file compressed with LZ4 (frame format).
    Lz4,
    /// A single file compressed with Brotli. Brotli streams carry no signature, so this one
    /// is recognised by the file name only.
    Br,
}

impl Format {
    /// Every format, whether or not this build supports it. The tar variants come before the
    /// single-file ones so that `a.tar.gz` is matched as tar.gz and not as gz.
    pub const ALL: [Format; 14] = [
        Format::Zip,
        Format::SevenZ,
        Format::Tar,
        Format::TarGz,
        Format::TarZst,
        Format::TarBz2,
        Format::TarXz,
        Format::TarLz4,
        Format::Gz,
        Format::Zst,
        Format::Bz2,
        Format::Xz,
        Format::Lz4,
        Format::Br,
    ];

    /// True for formats that compress one file with no container around it. They accept
    /// exactly one regular file and cannot hold directories.
    pub fn is_single_file(self) -> bool {
        matches!(
            self,
            Format::Gz | Format::Zst | Format::Bz2 | Format::Xz | Format::Lz4 | Format::Br
        )
    }

    /// Short human-readable name, also accepted by [`Format::from_name`].
    pub fn name(self) -> &'static str {
        match self {
            Format::Zip => "zip",
            Format::SevenZ => "7z",
            Format::Tar => "tar",
            Format::TarGz => "tar.gz",
            Format::TarZst => "tar.zst",
            Format::TarBz2 => "tar.bz2",
            Format::TarXz => "tar.xz",
            Format::TarLz4 => "tar.lz4",
            Format::Gz => "gz",
            Format::Zst => "zst",
            Format::Bz2 => "bz2",
            Format::Xz => "xz",
            Format::Lz4 => "lz4",
            Format::Br => "br",
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
            Format::SevenZ => &["7z"],
            Format::Tar => &["tar"],
            Format::TarGz => &["tar.gz", "tgz"],
            Format::TarZst => &["tar.zst", "tzst", "tar.zstd"],
            Format::TarBz2 => &["tar.bz2", "tbz2", "tbz", "tar.bz"],
            Format::TarXz => &["tar.xz", "txz"],
            Format::TarLz4 => &["tar.lz4", "tlz4"],
            Format::Gz => &["gz"],
            Format::Zst => &["zst", "zstd"],
            Format::Bz2 => &["bz2"],
            Format::Xz => &["xz"],
            Format::Lz4 => &["lz4"],
            Format::Br => &["br"],
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
        #[cfg(feature = "sevenz")]
        Format::SevenZ => Ok(&crate::formats::sevenz::SevenZFormat),
        #[cfg(feature = "tar")]
        Format::Tar => Ok(&crate::formats::tar_format::TarFormat::PLAIN),
        #[cfg(all(feature = "tar", feature = "gzip"))]
        Format::TarGz => Ok(&crate::formats::tar_format::TarFormat::GZIP),
        #[cfg(all(feature = "tar", feature = "zstd"))]
        Format::TarZst => Ok(&crate::formats::tar_format::TarFormat::ZSTD),
        #[cfg(all(feature = "tar", feature = "bzip2"))]
        Format::TarBz2 => Ok(&crate::formats::tar_format::TarFormat::BZIP2),
        #[cfg(all(feature = "tar", feature = "xz"))]
        Format::TarXz => Ok(&crate::formats::tar_format::TarFormat::XZ),
        #[cfg(all(feature = "tar", feature = "lz4"))]
        Format::TarLz4 => Ok(&crate::formats::tar_format::TarFormat::LZ4),
        #[cfg(feature = "gzip")]
        Format::Gz => Ok(&crate::formats::single::SingleFormat::GZ),
        #[cfg(feature = "zstd")]
        Format::Zst => Ok(&crate::formats::single::SingleFormat::ZST),
        #[cfg(feature = "bzip2")]
        Format::Bz2 => Ok(&crate::formats::single::SingleFormat::BZ2),
        #[cfg(feature = "xz")]
        Format::Xz => Ok(&crate::formats::single::SingleFormat::XZ),
        #[cfg(feature = "lz4")]
        Format::Lz4 => Ok(&crate::formats::single::SingleFormat::LZ4),
        #[cfg(feature = "brotli")]
        Format::Br => Ok(&crate::formats::single::SingleFormat::BR),
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
        assert_eq!(f("a.gz"), Some(Format::Gz));
        assert_eq!(f("a.txt.ZST"), Some(Format::Zst));
        assert_eq!(f("a.tar.gz"), Some(Format::TarGz), "tar.gz wins over gz");
        assert_eq!(f(".gz"), None);
        assert_eq!(f("a.tar.bz2"), Some(Format::TarBz2));
        assert_eq!(f("a.TBZ2"), Some(Format::TarBz2));
        assert_eq!(f("a.tar.xz"), Some(Format::TarXz));
        assert_eq!(f("a.tar.lz4"), Some(Format::TarLz4));
        assert_eq!(f("a.bz2"), Some(Format::Bz2));
        assert_eq!(f("a.xz"), Some(Format::Xz));
        assert_eq!(f("a.lz4"), Some(Format::Lz4));
        assert_eq!(f("a.br"), Some(Format::Br));
        assert_eq!(f("a.7z"), Some(Format::SevenZ));
    }

    #[test]
    fn names_round_trip() {
        for fmt in Format::ALL {
            assert_eq!(Format::from_name(fmt.name()), Some(fmt));
        }
    }
}
