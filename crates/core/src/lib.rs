//! Zstar core: stream-based archive engine.
//!
//! Archives are read from `Read + Seek` streams and written to
//! `Write + Seek` streams, so callers are not limited to file paths. The
//! format-independent file work (safe extraction, `.part` files, progress,
//! cancellation) is in [`extract_all`], [`compress_to_file`] and friends.

mod archive;
mod error;
mod format;
mod ops;
mod path;
mod progress;
#[cfg(test)]
mod testutil;
#[cfg(feature = "zip")]
mod zip_format;

pub use archive::{ArchiveReader, ArchiveWriter, Entry, create_writer, open_reader};
pub use error::{Error, Result};
pub use format::{DETECT_LEN, Format, detect, detect_bytes};
pub use ops::{
    ExtractOptions, Summary, compress_to_file, compress_to_writer, extract_all, extract_file,
};
pub use path::safe_join;
pub use progress::{CancelToken, NoObserver, Observer, Progress};
