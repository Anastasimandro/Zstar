use std::fmt;
use std::io;
use std::path::PathBuf;

use crate::format::Format;

/// Everything that can go wrong in the core. Front ends match on the variant
/// to show a localized message; `Display` is English and meant for logs/CLI.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    Io(io::Error),
    /// The observer asked to stop.
    Cancelled,
    /// The magic bytes match no known format.
    UnknownFormat,
    /// The format is recognised but cannot be handled (not implemented,
    /// feature disabled, or not allowed, as with creating RAR files).
    Unsupported(Format),
    PasswordRequired,
    InvalidPassword,
    /// An entry name would escape the destination directory.
    UnsafePath(String),
    AlreadyExists(PathBuf),
    /// A file name cannot be stored as an entry name (not valid UTF-8).
    InvalidName(PathBuf),
    /// The archive is damaged or not valid for its format.
    Corrupt(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::Cancelled => f.write_str("operation cancelled"),
            Error::UnknownFormat => f.write_str("unknown archive format"),
            Error::Unsupported(format) => write!(f, "{format} archives are not supported"),
            Error::PasswordRequired => f.write_str("a password is required"),
            Error::InvalidPassword => f.write_str("the password is incorrect"),
            Error::UnsafePath(name) => write!(f, "unsafe path in archive: {name}"),
            Error::AlreadyExists(path) => write!(f, "already exists: {}", path.display()),
            Error::InvalidName(path) => {
                write!(f, "cannot use as an entry name: {}", path.display())
            }
            Error::Corrupt(message) => write!(f, "archive is damaged or invalid: {message}"),
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
    fn from(error: io::Error) -> Self {
        Error::Io(error)
    }
}
