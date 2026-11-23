use std::fmt;
use std::io;

/// Why a RAR archive could not be read.
#[derive(Debug)]
#[non_exhaustive]
pub enum RarError {
    /// The underlying stream failed.
    Io(io::Error),
    /// Data or headers are encrypted. RAR passwords are not supported.
    Encrypted,
    /// The archive is one volume of a multi-volume set. A single stream cannot supply the others.
    MultiVolume,
    /// The stream does not start with a RAR signature.
    NotRar,
    /// Valid RAR that libarchive cannot decode (for example solid RAR3 or RAR filter blocks).
    Unsupported(String),
    /// Damaged or truncated data.
    Corrupt(String),
    /// A bug or a panic caught at the C boundary. Never expected.
    Internal(String),
}

impl fmt::Display for RarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RarError::Io(e) => write!(f, "I/O error: {e}"),
            RarError::Encrypted => f.write_str("encrypted RAR not supported"),
            RarError::MultiVolume => f.write_str("multi-volume RAR not supported"),
            RarError::NotRar => f.write_str("not a RAR archive"),
            RarError::Unsupported(m) => write!(f, "unsupported RAR feature: {m}"),
            RarError::Corrupt(m) => write!(f, "corrupt RAR archive: {m}"),
            RarError::Internal(m) => write!(f, "internal RAR reader error: {m}"),
        }
    }
}

impl std::error::Error for RarError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RarError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for RarError {
    fn from(e: io::Error) -> Self {
        RarError::Io(e)
    }
}
