//! Streaming compressors shared by the single-file formats and `tar.<ext>`.
//!
//! A codec only knows how to wrap a stream; what is inside (one file, a tar) is the caller's
//! business. Both directions use fixed buffers, so memory does not depend on data size.

use std::io::{Read, Write};

#[cfg(any(feature = "gzip", feature = "zstd"))]
use crate::error::Error;
use crate::error::Result;
use crate::format::{ReadSeek, WriteSeek};

#[cfg(feature = "gzip")]
const READ_BUF: usize = 64 * 1024;

/// With neither compressor built in this has no values, which lets plain tar compile alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Codec {
    #[cfg(feature = "gzip")]
    Gzip,
    #[cfg(feature = "zstd")]
    Zstd,
}

#[cfg(not(any(feature = "gzip", feature = "zstd")))]
impl Codec {
    pub(crate) fn level(self, _requested: Option<i32>) -> Result<i32> {
        match self {}
    }

    pub(crate) fn with_reader<T>(
        self,
        _input: &mut dyn ReadSeek,
        _f: impl FnOnce(&mut dyn Read) -> Result<T>,
    ) -> Result<T> {
        match self {}
    }

    pub(crate) fn with_writer<T>(
        self,
        _out: &mut dyn WriteSeek,
        _level: i32,
        _threads: usize,
        _f: impl FnOnce(&mut dyn Write) -> Result<T>,
    ) -> Result<T> {
        match self {}
    }
}

#[cfg(any(feature = "gzip", feature = "zstd"))]
impl Codec {
    pub(crate) fn name(self) -> &'static str {
        match self {
            #[cfg(feature = "gzip")]
            Codec::Gzip => "gzip",
            #[cfg(feature = "zstd")]
            Codec::Zstd => "zstd",
        }
    }

    /// Validates `requested` against this codec's range, or returns its default level.
    pub(crate) fn level(self, requested: Option<i32>) -> Result<i32> {
        let (default, range) = match self {
            #[cfg(feature = "gzip")]
            Codec::Gzip => (6, 0..=9),
            #[cfg(feature = "zstd")]
            Codec::Zstd => (3, 1..=22),
        };
        match requested {
            None => Ok(default),
            Some(l) if range.contains(&l) => Ok(l),
            Some(l) => Err(Error::InvalidOption(format!(
                "{} compression level must be {}-{}, got {l}",
                self.name(),
                range.start(),
                range.end()
            ))),
        }
    }

    /// Runs `f` on the decompressed byte stream.
    pub(crate) fn with_reader<T>(
        self,
        input: &mut dyn ReadSeek,
        f: impl FnOnce(&mut dyn Read) -> Result<T>,
    ) -> Result<T> {
        match self {
            #[cfg(feature = "gzip")]
            Codec::Gzip => {
                let buffered = std::io::BufReader::with_capacity(READ_BUF, input);
                f(&mut flate2::bufread::MultiGzDecoder::new(buffered))
            }
            // The zstd decoder buffers its input itself.
            #[cfg(feature = "zstd")]
            Codec::Zstd => f(&mut zstd::stream::read::Decoder::new(input)?),
        }
    }

    /// Runs `f` on a compressing writer over `out`, then finishes the stream. Nothing is
    /// valid until `f` has succeeded and this returns, so a failure leaves a broken stream
    /// for the caller to discard.
    pub(crate) fn with_writer<T>(
        self,
        out: &mut dyn WriteSeek,
        level: i32,
        threads: usize,
        f: impl FnOnce(&mut dyn Write) -> Result<T>,
    ) -> Result<T> {
        // Only zstd compresses on several threads; gzip ignores `threads`.
        let _ = threads;
        match self {
            #[cfg(feature = "gzip")]
            Codec::Gzip => {
                let mut enc =
                    flate2::write::GzEncoder::new(out, flate2::Compression::new(level as u32));
                let value = f(&mut enc)?;
                enc.finish()?;
                Ok(value)
            }
            #[cfg(feature = "zstd")]
            Codec::Zstd => {
                let mut enc = zstd::stream::write::Encoder::new(out, level)?;
                enc.include_checksum(true)?;
                if threads > 1 {
                    enc.multithread(threads as u32)?;
                }
                let value = f(&mut enc)?;
                enc.finish()?;
                Ok(value)
            }
        }
    }
}
