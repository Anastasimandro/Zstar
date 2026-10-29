//! One module per archive format. Each is gated by its own Cargo feature.

use std::io::{self, SeekFrom};

use crate::format::ReadSeek;

#[cfg(feature = "tar")]
pub(crate) mod tar_format;
#[cfg(feature = "zip")]
pub(crate) mod zip_format;

/// Length of a seekable stream, leaving the position where it was.
pub(crate) fn stream_len(input: &mut dyn ReadSeek) -> io::Result<u64> {
    let here = input.stream_position()?;
    let end = input.seek(SeekFrom::End(0))?;
    input.seek(SeekFrom::Start(here))?;
    Ok(end)
}
