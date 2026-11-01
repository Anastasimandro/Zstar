//! One module per archive format. Each is gated by its own Cargo feature.

use std::io::{self, SeekFrom};

use crate::format::ReadSeek;

pub(crate) mod codec;
#[cfg(feature = "sevenz")]
pub(crate) mod sevenz;
#[cfg(any(
    feature = "gzip",
    feature = "zstd",
    feature = "bzip2",
    feature = "xz",
    feature = "lz4",
    feature = "brotli"
))]
pub(crate) mod single;
#[cfg(feature = "tar")]
pub(crate) mod tar_format;
#[cfg(feature = "zip")]
pub(crate) mod zip_format;

/// Formats without encryption refuse a password instead of silently ignoring it.
#[cfg(any(
    feature = "tar",
    feature = "gzip",
    feature = "zstd",
    feature = "bzip2",
    feature = "xz",
    feature = "lz4",
    feature = "brotli"
))]
pub(crate) fn no_password(opts: &crate::Options, format: crate::Format) -> crate::Result<()> {
    if opts.password.is_some() {
        return Err(crate::Error::InvalidOption(format!(
            "{} cannot be encrypted; use zip or 7z for a password",
            format.name()
        )));
    }
    Ok(())
}

/// Length of a seekable stream, leaving the position where it was.
pub(crate) fn stream_len(input: &mut dyn ReadSeek) -> io::Result<u64> {
    let here = input.stream_position()?;
    let end = input.seek(SeekFrom::End(0))?;
    input.seek(SeekFrom::Start(here))?;
    Ok(end)
}
