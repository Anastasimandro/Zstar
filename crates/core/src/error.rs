use std::fmt;
use std::io;
use std::path::PathBuf;

use crate::Format;

/// Which extraction limit was hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitKind {
    /// Total uncompressed size.
    TotalSize,
    /// Number of entries.
    Entries,
    /// Uncompressed size relative to the archive size.
    Ratio,
    /// Memory a decoder asks for (compression dictionary size).
    Memory,
}

impl LimitKind {
    /// The key of this limit in the `[limits]` table of the configuration file.
    pub fn config_key(self) -> &'static str {
        match self {
            LimitKind::TotalSize => "limits.max_total_bytes",
            LimitKind::Entries => "limits.max_entries",
            LimitKind::Ratio => "limits.max_ratio",
            LimitKind::Memory => "limits.max_memory_bytes",
        }
    }
}

/// Everything that can go wrong in `zstar-core`.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// An I/O error from the archive stream or the file system.
    Io(io::Error),
    /// The input is not a format this build can read.
    UnsupportedFormat(String),
    /// The format is known but its Cargo feature is disabled in this build.
    FeatureDisabled(Format),
    /// The archive is corrupt or violates its format.
    InvalidArchive(String),
    /// An entry name would escape the destination or is not valid on this platform.
    UnsafePath {
        /// Entry name as stored in the archive.
        name: String,
        /// Why the name was rejected.
        reason: &'static str,
    },
    /// A symbolic or hard link points outside the destination.
    UnsafeLink {
        /// Entry name as stored in the archive.
        name: String,
        /// Link target as stored in the archive.
        target: String,
    },
    /// A decompression-bomb limit was exceeded.
    LimitExceeded(LimitKind),
    /// A file to be written already exists and overwriting is off.
    AlreadyExists(PathBuf),
    /// A caller-supplied option is invalid for this operation.
    InvalidOption(String),
    /// The operation was cancelled through its [`CancelToken`](crate::CancelToken).
    Cancelled,
}

/// Result alias used across the crate.
pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::UnsupportedFormat(msg) => write!(f, "unsupported format: {msg}"),
            Error::FeatureDisabled(fmt) => {
                write!(f, "{} support is not enabled in this build", fmt.name())
            }
            Error::InvalidArchive(msg) => write!(f, "invalid archive: {msg}"),
            Error::UnsafePath { name, reason } => {
                write!(f, "unsafe entry name {name:?}: {reason}")
            }
            Error::UnsafeLink { name, target } => {
                write!(
                    f,
                    "unsafe link {name:?} -> {target:?}: points outside the destination"
                )
            }
            Error::LimitExceeded(kind) => {
                let what = match kind {
                    LimitKind::TotalSize => "total uncompressed size",
                    LimitKind::Entries => "number of entries",
                    LimitKind::Ratio => "compression ratio (possible decompression bomb)",
                    LimitKind::Memory => "memory a decoder asks for (compression dictionary size)",
                };
                write!(
                    f,
                    "extraction limit exceeded: {what} [{}]",
                    kind.config_key()
                )
            }
            Error::AlreadyExists(p) => write!(f, "{} already exists", p.display()),
            Error::InvalidOption(msg) => write!(f, "invalid option: {msg}"),
            Error::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        // Decoders that check their memory limit up front report it as `OutOfMemory`.
        if e.kind() == io::ErrorKind::OutOfMemory {
            Error::LimitExceeded(LimitKind::Memory)
        } else {
            Error::Io(e)
        }
    }
}

#[cfg(feature = "zip")]
impl From<zip::result::ZipError> for Error {
    fn from(e: zip::result::ZipError) -> Self {
        match e {
            zip::result::ZipError::Io(e) => Error::Io(e),
            other => Error::InvalidArchive(other.to_string()),
        }
    }
}
