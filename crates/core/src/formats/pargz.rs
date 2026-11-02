//! Gzip compression on several threads, in the style of pigz.
//!
//! The input is cut into fixed blocks. Each block is deflated independently, primed with the
//! last 32 KiB of the block before it (so matches still reach across block borders and the
//! output is as small as a single stream), and ended with a sync flush so the next block
//! starts on a byte boundary. The pieces are concatenated into one ordinary deflate stream,
//! so any gzip reader can read the result. Memory is a handful of blocks per thread, whatever
//! the input size.
//!
//! The workers are created for the most that can run, and each wave of blocks is shared out
//! among as many of them as the [`Workers`] allow at that moment. That is the safe point where
//! a change of thread limit or priority takes effect. Every block is compressed from the same
//! dictionary whichever worker gets it, so the result is always a valid stream with the same
//! content; its bytes can still differ slightly between thread counts.

use std::io::{self, Write};
use std::thread;

use flate2::{Compress, Compression, FlushCompress, Status};

use crate::control::Workers;

/// Uncompressed bytes per block. Small enough to keep memory low, large enough that the sync
/// flush marker between blocks (5 bytes) is lost in the noise.
const BLOCK: usize = 256 * 1024;
/// How far back deflate can reference.
const WINDOW: usize = 32 * 1024;

/// A gzip writer that compresses on several threads. Call [`ParGz::finish`] when done.
pub(crate) struct ParGz<W: Write> {
    out: W,
    workers: Workers,
    /// Two blocks per thread: one being compressed while the caller fills the next wave.
    bufs: Vec<Vec<u8>>,
    outs: Vec<Vec<u8>>,
    comps: Vec<Compress>,
    /// Block currently being filled.
    idx: usize,
    /// The last bytes of the previous wave, the dictionary of the next one's first block.
    tail: Vec<u8>,
    crc: crc32fast::Hasher,
    total: u64,
    header_done: bool,
}

impl<W: Write> ParGz<W> {
    pub(crate) fn new(out: W, level: u32, workers: Workers) -> Self {
        let threads = workers.pool;
        ParGz {
            out,
            workers,
            bufs: (0..threads * 2).map(|_| Vec::new()).collect(),
            outs: (0..threads * 2).map(|_| Vec::new()).collect(),
            comps: (0..threads)
                .map(|_| Compress::new(Compression::new(level), false))
                .collect(),
            idx: 0,
            tail: Vec::new(),
            crc: crc32fast::Hasher::new(),
            total: 0,
            header_done: false,
        }
    }

    /// Compresses blocks `0..=self.idx`; the last one ends the stream when `last` is set.
    fn flush_wave(&mut self, last: bool) -> io::Result<()> {
        if !self.header_done {
            // Fixed header: no name, no timestamp, "Unix" as the OS.
            self.out.write_all(&[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3])?;
            self.header_done = true;
        }
        let n = self.idx + 1;
        let active = self.workers.active();
        let workers = &self.workers;
        let final_idx = last.then_some(n - 1);
        let tail = &self.tail;
        let bufs = &self.bufs;
        let dict = |i: usize| -> &[u8] {
            let prev = if i == 0 { tail } else { &bufs[i - 1] };
            &prev[prev.len().saturating_sub(WINDOW)..]
        };

        let mut jobs: Vec<Vec<(usize, &mut Vec<u8>)>> = (0..active).map(|_| Vec::new()).collect();
        for (i, o) in self.outs.iter_mut().take(n).enumerate() {
            jobs[i % active].push((i, o));
        }
        let crcs: Vec<io::Result<crc32fast::Hasher>> = thread::scope(|scope| {
            let handles: Vec<_> = jobs
                .into_iter()
                .zip(self.comps.iter_mut())
                .map(|(items, comp)| {
                    let (bufs, dict) = (bufs, &dict);
                    scope.spawn(move || {
                        workers.sync_priority();
                        items
                            .into_iter()
                            .map(|(i, o)| {
                                deflate_block(comp, dict(i), &bufs[i], final_idx == Some(i), o)?;
                                let mut h = crc32fast::Hasher::new();
                                h.update(&bufs[i]);
                                Ok((i, h))
                            })
                            .collect::<Vec<io::Result<(usize, crc32fast::Hasher)>>>()
                    })
                })
                .collect();
            let mut by_block: Vec<Option<io::Result<crc32fast::Hasher>>> =
                (0..n).map(|_| None).collect();
            for h in handles {
                for r in h.join().unwrap_or_else(|_| {
                    vec![Err(io::Error::other("a compression thread panicked"))]
                }) {
                    match r {
                        Ok((i, hasher)) => by_block[i] = Some(Ok(hasher)),
                        Err(e) => by_block[0] = Some(Err(e)),
                    }
                }
            }
            by_block
                .into_iter()
                .map(|r| r.unwrap_or_else(|| Err(io::Error::other("missing block"))))
                .collect()
        });

        for (i, h) in crcs.into_iter().enumerate() {
            let h = h?;
            self.out.write_all(&self.outs[i])?;
            self.total += self.bufs[i].len() as u64;
            self.crc.combine(&h);
        }
        let last_block = &self.bufs[n - 1];
        self.tail.clear();
        self.tail
            .extend_from_slice(&last_block[last_block.len().saturating_sub(WINDOW)..]);
        Ok(())
    }

    /// Ends the stream and returns the inner writer.
    pub(crate) fn finish(mut self) -> io::Result<W> {
        self.flush_wave(true)?;
        let crc = std::mem::replace(&mut self.crc, crc32fast::Hasher::new()).finalize();
        self.out.write_all(&crc.to_le_bytes())?;
        // The gzip size field is the length modulo 2^32.
        self.out.write_all(&(self.total as u32).to_le_bytes())?;
        self.out.flush()?;
        Ok(self.out)
    }
}

impl<W: Write> Write for ParGz<W> {
    fn write(&mut self, mut data: &[u8]) -> io::Result<usize> {
        let written = data.len();
        while !data.is_empty() {
            if self.bufs[self.idx].len() == BLOCK {
                if self.idx + 1 == self.bufs.len() {
                    // More data is coming, so none of these blocks ends the stream.
                    self.flush_wave(false)?;
                    self.idx = 0;
                    self.bufs[0].clear();
                } else {
                    self.idx += 1;
                    self.bufs[self.idx].clear();
                }
            }
            let buf = &mut self.bufs[self.idx];
            let take = (BLOCK - buf.len()).min(data.len());
            buf.extend_from_slice(&data[..take]);
            data = &data[take..];
        }
        Ok(written)
    }

    /// Nothing can be flushed early: a block is only compressed once it is complete.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn deflate_block(
    comp: &mut Compress,
    dict: &[u8],
    data: &[u8],
    last: bool,
    out: &mut Vec<u8>,
) -> io::Result<()> {
    comp.reset();
    if !dict.is_empty() {
        comp.set_dictionary(dict).map_err(io::Error::other)?;
    }
    out.clear();
    out.reserve(data.len() / 2 + 1024);
    let flush = if last {
        FlushCompress::Finish
    } else {
        FlushCompress::Sync
    };
    let start = comp.total_in();
    loop {
        let consumed = (comp.total_in() - start) as usize;
        let status = comp
            .compress_vec(&data[consumed..], out, flush)
            .map_err(io::Error::other)?;
        let consumed = (comp.total_in() - start) as usize;
        match status {
            Status::StreamEnd => return Ok(()),
            // A sync flush has no end marker: it is finished when everything was consumed
            // and the output buffer still had room.
            _ if !last && consumed == data.len() && out.len() < out.capacity() => return Ok(()),
            _ => out.reserve(out.capacity().max(4096)),
        }
    }
}
