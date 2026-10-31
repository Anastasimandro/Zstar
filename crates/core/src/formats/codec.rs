//! Streaming compressors shared by the single-file formats and `tar.<ext>`.
//!
//! A codec only knows how to wrap a stream; what is inside (one file, a tar) is the caller's
//! business. Both directions use fixed buffers, so memory does not depend on data size.

// Without a compressor feature this module is empty on purpose and nothing in it is used.
#![cfg_attr(
    not(any(
        feature = "gzip",
        feature = "zstd",
        feature = "bzip2",
        feature = "xz",
        feature = "lz4",
        feature = "brotli"
    )),
    allow(dead_code)
)]

use std::io::{Read, Write};

use crate::format::{ReadSeek, WriteSeek};

/// With no compressor built in this has no values, which lets plain tar compile alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Codec {
    #[cfg(feature = "gzip")]
    Gzip,
    #[cfg(feature = "zstd")]
    Zstd,
    #[cfg(feature = "bzip2")]
    Bzip2,
    #[cfg(feature = "xz")]
    Xz,
    #[cfg(feature = "lz4")]
    Lz4,
    #[cfg(feature = "brotli")]
    Brotli,
}

#[cfg(not(any(
    feature = "gzip",
    feature = "zstd",
    feature = "bzip2",
    feature = "xz",
    feature = "lz4",
    feature = "brotli"
)))]
impl Codec {
    pub(crate) fn level(self, _requested: Option<i32>) -> crate::error::Result<i32> {
        match self {}
    }

    pub(crate) fn with_reader<T>(
        self,
        _input: &mut dyn ReadSeek,
        _f: impl FnOnce(&mut dyn Read) -> crate::error::Result<T>,
    ) -> crate::error::Result<T> {
        match self {}
    }

    pub(crate) fn with_writer<T>(
        self,
        _out: &mut dyn WriteSeek,
        _level: i32,
        _threads: usize,
        _f: impl FnOnce(&mut dyn Write) -> crate::error::Result<T>,
    ) -> crate::error::Result<T> {
        match self {}
    }
}

#[cfg(any(
    feature = "gzip",
    feature = "zstd",
    feature = "bzip2",
    feature = "xz",
    feature = "lz4",
    feature = "brotli"
))]
mod imp {
    #[cfg(feature = "brotli")]
    use std::cell::Cell;
    #[allow(unused_imports)] // a zstd-only build never touches `io`
    use std::io;

    use super::*;
    use crate::error::{Error, Result};

    #[cfg(any(
        feature = "gzip",
        feature = "bzip2",
        feature = "xz",
        feature = "lz4",
        feature = "brotli"
    ))]
    const READ_BUF: usize = 64 * 1024;

    /// Records that a write failed. Used where a compressor swallows errors when it closes.
    #[cfg(feature = "brotli")]
    struct Watched<'a, W> {
        inner: W,
        failed: &'a Cell<bool>,
    }

    #[cfg(feature = "brotli")]
    impl<W: Write> Write for Watched<'_, W> {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.inner.write(buf).inspect_err(|_| self.failed.set(true))
        }

        fn flush(&mut self) -> io::Result<()> {
            self.inner.flush().inspect_err(|_| self.failed.set(true))
        }
    }

    /// Reads every LZ4 frame of a stream, one after the other, like the `lz4` tool does.
    /// `lz4_flex`'s own decoder stops after the first frame and would silently drop the rest
    /// of a concatenated file. Whatever follows a frame must itself be a valid frame, so
    /// trailing garbage is an error.
    #[cfg(feature = "lz4")]
    struct MultiFrame<R: io::BufRead> {
        decoder: Option<lz4_flex::frame::FrameDecoder<R>>,
    }

    #[cfg(feature = "lz4")]
    impl<R: io::BufRead> Read for MultiFrame<R> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            loop {
                let Some(decoder) = self.decoder.as_mut() else {
                    return Ok(0);
                };
                let n = decoder.read(buf)?;
                if n > 0 || buf.is_empty() {
                    return Ok(n);
                }
                // End of a frame: another one follows only if input is left.
                let mut rest = self.decoder.take().map(|d| d.into_inner()).unwrap();
                if rest.fill_buf()?.is_empty() {
                    return Ok(0);
                }
                self.decoder = Some(lz4_flex::frame::FrameDecoder::new(rest));
            }
        }
    }

    impl Codec {
        pub(crate) fn name(self) -> &'static str {
            match self {
                #[cfg(feature = "gzip")]
                Codec::Gzip => "gzip",
                #[cfg(feature = "zstd")]
                Codec::Zstd => "zstd",
                #[cfg(feature = "bzip2")]
                Codec::Bzip2 => "bzip2",
                #[cfg(feature = "xz")]
                Codec::Xz => "xz",
                #[cfg(feature = "lz4")]
                Codec::Lz4 => "lz4",
                #[cfg(feature = "brotli")]
                Codec::Brotli => "brotli",
            }
        }

        /// Validates `requested` against this codec's range, or returns its default level.
        pub(crate) fn level(self, requested: Option<i32>) -> Result<i32> {
            #[cfg(feature = "lz4")]
            if self == Codec::Lz4 {
                return match requested {
                    None => Ok(0),
                    Some(_) => Err(Error::InvalidOption(
                        "lz4 has a single speed setting and takes no compression level".into(),
                    )),
                };
            }
            let (default, range) = match self {
                #[cfg(feature = "gzip")]
                Codec::Gzip => (6, 0..=9),
                #[cfg(feature = "zstd")]
                Codec::Zstd => (3, 1..=22),
                #[cfg(feature = "bzip2")]
                Codec::Bzip2 => (9, 1..=9),
                #[cfg(feature = "xz")]
                Codec::Xz => (6, 0..=9),
                // Brotli's own default (11) is several times slower than 6 for ~5% less size.
                #[cfg(feature = "brotli")]
                Codec::Brotli => (6, 0..=11),
                #[cfg(feature = "lz4")]
                Codec::Lz4 => (0, 0..=0), // handled above
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
                    let buffered = io::BufReader::with_capacity(READ_BUF, input);
                    f(&mut flate2::bufread::MultiGzDecoder::new(buffered))
                }
                // The zstd decoder buffers its input itself.
                #[cfg(feature = "zstd")]
                Codec::Zstd => f(&mut zstd::stream::read::Decoder::new(input)?),
                #[cfg(feature = "bzip2")]
                Codec::Bzip2 => {
                    let buffered = io::BufReader::with_capacity(READ_BUF, input);
                    f(&mut bzip2::bufread::MultiBzDecoder::new(buffered))
                }
                #[cfg(feature = "xz")]
                Codec::Xz => {
                    let buffered = io::BufReader::with_capacity(READ_BUF, input);
                    f(&mut lzma_rust2::XzReader::new(buffered, true))
                }
                #[cfg(feature = "lz4")]
                Codec::Lz4 => {
                    let buffered = io::BufReader::with_capacity(READ_BUF, input);
                    f(&mut MultiFrame {
                        decoder: Some(lz4_flex::frame::FrameDecoder::new(buffered)),
                    })
                }
                #[cfg(feature = "brotli")]
                Codec::Brotli => f(&mut brotli::Decompressor::new(input, READ_BUF)),
            }
        }

        /// Runs `f` on a compressing writer over `out`, then finishes the stream. Nothing is
        /// valid until `f` has succeeded and this returns, so a failure leaves a broken
        /// stream for the caller to discard.
        pub(crate) fn with_writer<T>(
            self,
            out: &mut dyn WriteSeek,
            level: i32,
            threads: usize,
            f: impl FnOnce(&mut dyn Write) -> Result<T>,
        ) -> Result<T> {
            // Only zstd and xz compress on several threads; the rest ignore `threads`, and
            // lz4 has no level.
            let _ = (threads, level);
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
                #[cfg(feature = "bzip2")]
                Codec::Bzip2 => {
                    let mut enc =
                        bzip2::write::BzEncoder::new(out, bzip2::Compression::new(level as u32));
                    let value = f(&mut enc)?;
                    enc.finish()?;
                    Ok(value)
                }
                #[cfg(feature = "xz")]
                Codec::Xz => {
                    let options = lzma_rust2::XzOptions::with_preset(level as u32);
                    if threads > 1 {
                        // Independent blocks of 3x the dictionary, as the xz tool does for -T.
                        let mut options = options;
                        let block = u64::from(options.lzma_options.dict_size) * 3;
                        options.set_block_size(std::num::NonZeroU64::new(block));
                        let mut enc = lzma_rust2::XzWriterMt::new(out, options, threads as u32)?;
                        let value = f(&mut enc)?;
                        enc.finish()?;
                        Ok(value)
                    } else {
                        let mut enc = lzma_rust2::XzWriter::new(out, options)?;
                        let value = f(&mut enc)?;
                        enc.finish()?;
                        Ok(value)
                    }
                }
                #[cfg(feature = "lz4")]
                Codec::Lz4 => {
                    use lz4_flex::frame::{BlockSize, FrameEncoder, FrameInfo};
                    // The content checksum is what lets us notice corruption on extraction.
                    let info = FrameInfo::new()
                        .block_size(BlockSize::Max1MB)
                        .content_checksum(true);
                    let mut enc = FrameEncoder::with_frame_info(info, out);
                    let value = f(&mut enc)?;
                    enc.finish().map_err(io::Error::from)?;
                    Ok(value)
                }
                #[cfg(feature = "brotli")]
                Codec::Brotli => {
                    // `CompressorWriter` ignores write errors while closing, which would
                    // leave a truncated file that looks complete. Watch for them ourselves.
                    let failed = Cell::new(false);
                    let watched = Watched {
                        inner: out,
                        failed: &failed,
                    };
                    let mut enc =
                        brotli::CompressorWriter::new(watched, 64 * 1024, level as u32, 22);
                    let value = f(&mut enc)?;
                    let _ = enc.into_inner(); // closes the stream; errors are caught by `failed`
                    if failed.get() {
                        return Err(Error::Io(io::Error::other(
                            "writing the brotli stream failed while closing it",
                        )));
                    }
                    Ok(value)
                }
            }
        }
    }
}
