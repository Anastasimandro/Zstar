//! Single-file compression: one compressed stream holding one file, no container.
//!
//! Extraction goes through the shared [`Session`], so the `.part` rename, the bomb limits and
//! cancellation behave exactly as they do for archives.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use crate::error::{Error, LimitKind, Result};
use crate::extract::{CHUNK, Session, Summary};
use crate::format::{ArchiveFormat, EntryInfo, EntryKind, Format, ReadSeek, WriteSeek};
use crate::formats::codec::Codec;
use crate::formats::stream_len;
use crate::options::Options;
use crate::progress::Tracker;
use crate::sources::SourceEntry;

pub(crate) struct SingleFormat {
    codec: Codec,
    format: Format,
}

impl SingleFormat {
    #[cfg(feature = "gzip")]
    pub(crate) const GZ: SingleFormat = SingleFormat {
        codec: Codec::Gzip,
        format: Format::Gz,
    };
    #[cfg(feature = "zstd")]
    pub(crate) const ZST: SingleFormat = SingleFormat {
        codec: Codec::Zstd,
        format: Format::Zst,
    };

    #[cfg(feature = "bzip2")]
    pub(crate) const BZ2: SingleFormat = SingleFormat {
        codec: Codec::Bzip2,
        format: Format::Bz2,
    };
    #[cfg(feature = "xz")]
    pub(crate) const XZ: SingleFormat = SingleFormat {
        codec: Codec::Xz,
        format: Format::Xz,
    };
    #[cfg(feature = "lz4")]
    pub(crate) const LZ4: SingleFormat = SingleFormat {
        codec: Codec::Lz4,
        format: Format::Lz4,
    };
    #[cfg(feature = "brotli")]
    pub(crate) const BR: SingleFormat = SingleFormat {
        codec: Codec::Brotli,
        format: Format::Br,
    };

    /// Name of the extracted file: the archive's name without its extension.
    ///
    /// A stream has no file name, so [`Options::name_hint`] supplies it; without one, or when
    /// the hint has no matching extension to strip, the result cannot be the input's own name.
    fn output_name(&self, opts: &Options) -> String {
        let Some(hint) = opts.name_hint.as_deref().filter(|h| !h.is_empty()) else {
            return "data".into();
        };
        let lower = hint.to_ascii_lowercase();
        for ext in self.format.extensions() {
            if let Some(stem) = lower.strip_suffix(ext)
                && let Some(stem) = stem.strip_suffix('.')
                && !stem.is_empty()
            {
                return hint[..stem.len()].to_owned();
            }
        }
        format!("{hint}.out")
    }
}

/// Reads into `buf`, retrying on `Interrupted`; `Ok(0)` is the end of the stream.
fn read_chunk(src: &mut dyn Read, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        match src.read(buf) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            other => return other,
        }
    }
}

fn only_one_file(sources: &[SourceEntry], what: &str) -> Result<()> {
    match sources {
        [one] if one.kind == EntryKind::File => Ok(()),
        _ => Err(Error::InvalidOption(format!(
            "{what} stores exactly one file; use a tar format for several files or directories"
        ))),
    }
}

impl ArchiveFormat for SingleFormat {
    fn format(&self) -> Format {
        self.format
    }

    fn list(
        &self,
        input: &mut dyn ReadSeek,
        opts: &Options,
        visit: &mut dyn FnMut(EntryInfo),
    ) -> Result<()> {
        super::no_password(opts, self.format)?;
        let compressed = stream_len(input)?;
        // The uncompressed size is not reliably stored, so measure it. The extraction size
        // limit applies, which keeps a bomb from tying up a listing.
        let size = self
            .codec
            .with_reader(input, opts.limits.max_memory_bytes, |reader| {
                let mut buf = vec![0u8; CHUNK];
                let mut size = 0u64;
                loop {
                    opts.check_cancel()?;
                    let n = read_chunk(reader, &mut buf)?;
                    if n == 0 {
                        return Ok(size);
                    }
                    size += n as u64;
                    if size > opts.limits.max_total_bytes {
                        return Err(Error::LimitExceeded(LimitKind::TotalSize));
                    }
                }
            })?;
        visit(EntryInfo {
            name: self.output_name(opts),
            kind: EntryKind::File,
            size,
            compressed_size: Some(compressed),
            mode: None,
            mtime: None,
            link_target: None,
        });
        Ok(())
    }

    fn extract(&self, input: &mut dyn ReadSeek, dest: &Path, opts: &Options) -> Result<Summary> {
        super::no_password(opts, self.format)?;
        let input_len = stream_len(input)?;
        let name = self.output_name(opts);
        self.codec
            .with_reader(input, opts.limits.max_memory_bytes, |reader| {
                let session = Session::new(dest, opts, input_len, None, Some(1))?;
                session.begin_entry()?;
                let rel = session.rel_path(name.as_bytes())?;
                let mut buf = vec![0u8; CHUNK];
                session.write_file(&rel, &name, reader, None, None, &mut buf)?;
                session.finish()
            })
    }

    fn create(
        &self,
        sources: &[SourceEntry],
        out: &mut dyn WriteSeek,
        opts: &Options,
    ) -> Result<()> {
        super::no_password(opts, self.format)?;
        let level = self.codec.level(opts.level)?;
        only_one_file(sources, self.format.name())?;
        let src = &sources[0];
        let tracker = Tracker::new(opts, Some(src.size), Some(1));
        let mut file = File::open(&src.path)?;
        self.codec
            .with_writer(out, level, opts.effective_threads(), |writer| {
                let mut buf = vec![0u8; CHUNK];
                loop {
                    opts.check_cancel()?;
                    let n = read_chunk(&mut file, &mut buf)?;
                    if n == 0 {
                        break;
                    }
                    writer.write_all(&buf[..n])?;
                    tracker.add_bytes(n as u64, &src.name);
                }
                tracker.entry_done(&src.name);
                Ok(())
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(format: &SingleFormat, hint: Option<&str>) -> String {
        let opts = Options {
            name_hint: hint.map(str::to_owned),
            ..Options::default()
        };
        format.output_name(&opts)
    }

    #[cfg(feature = "gzip")]
    #[test]
    fn output_name_strips_the_extension_case_insensitively() {
        let gz = &SingleFormat::GZ;
        assert_eq!(name(gz, Some("notes.txt.gz")), "notes.txt");
        assert_eq!(name(gz, Some("Notes.TXT.GZ")), "Notes.TXT");
        assert_eq!(name(gz, Some("plain")), "plain.out");
        assert_eq!(name(gz, Some(".gz")), ".gz.out");
        assert_eq!(name(gz, Some("")), "data");
        assert_eq!(name(gz, None), "data");
    }
}
