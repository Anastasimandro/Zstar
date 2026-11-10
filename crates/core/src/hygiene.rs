//! Page-cache hygiene for big transfers: when to tell the OS that a stretch of a file is done
//! with (ADR 0012, step 3). Core makes no OS calls: the caller's [`CacheHook`] turns each
//! request into one, the way `open_hook` turns an open into a sequential-read hint.
//!
//! A transfer is cut into windows of `io_depth` requests of `chunk` bytes (see
//! [`Control::set_io_depth`]). Written data: when a window is full, the one before it is
//! released (waited for, then dropped from the cache) and this one starts being written, so
//! at most about two windows are ever dirty. Data that is only read: a full window is released
//! at once. Files smaller than a window never reach the hook, so extracting many small files
//! costs nothing extra.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::Arc;

use crate::control::Control;
use crate::options::Options;
use crate::stats::Timed;

/// What the engine asks the OS to do with a stretch of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheOp {
    /// Start writing its dirty pages to the drive and return at once.
    StartWriteback,
    /// Wait until it has been written (a clean range returns at once), then drop it from the
    /// page cache.
    Release,
}

/// Called with the file, the offset and length of a range (length 0: up to the end of the
/// file) and what to do with it. Best effort: it cannot fail an operation.
pub type CacheHook = Arc<dyn Fn(&File, u64, u64, CacheOp) + Send + Sync>;

/// Requests per window when the control sets no `io_depth`.
const DEFAULT_DEPTH: u64 = 4;
/// Smallest and largest window, in bytes.
const MIN_WINDOW: u64 = 1 << 20;
const MAX_WINDOW: u64 = 64 << 20;
/// A file smaller than this never gets a window of its own.
pub(crate) const TRACK_MIN: u64 = 4 << 20;

/// The window for requests of `chunk` bytes under the control's current `io_depth`.
pub(crate) fn window_bytes(control: &Control, chunk: usize) -> u64 {
    let depth = match control.io_depth() {
        0 => DEFAULT_DEPTH,
        d => d as u64,
    };
    depth
        .saturating_mul(chunk as u64)
        .clamp(MIN_WINDOW, MAX_WINDOW)
}

/// Follows one sequential pass over a file and tells the hook when a window is full.
pub(crate) struct Windows {
    hook: CacheHook,
    writes: bool,
    /// Where the window being filled starts, and how far the pass has got.
    start: u64,
    pos: u64,
    /// The last window whose writeback was started and that waits to be released.
    prev: Option<(u64, u64)>,
    /// Some window was closed, so the file is big enough for the end to matter.
    used: bool,
}

impl Windows {
    pub(crate) fn new(hook: CacheHook, writes: bool) -> Self {
        Windows {
            hook,
            writes,
            start: 0,
            pos: 0,
            prev: None,
            used: false,
        }
    }

    /// `n` more bytes were moved at the current position.
    pub(crate) fn advance(&mut self, file: &File, n: u64, window: u64) {
        self.pos = self.pos.saturating_add(n);
        let len = self.pos - self.start;
        if len < window {
            return;
        }
        self.used = true;
        if self.writes {
            if let Some((offset, len)) = self.prev.take() {
                (self.hook)(file, offset, len, CacheOp::Release);
            }
            (self.hook)(file, self.start, len, CacheOp::StartWriteback);
            self.prev = Some((self.start, len));
        } else {
            (self.hook)(file, self.start, len, CacheOp::Release);
        }
        self.start = self.pos;
    }

    /// The position jumped (a seek): what was moved so far in this window is left to the
    /// kernel, and the next window starts at `to`.
    pub(crate) fn seek(&mut self, to: u64) {
        self.start = to;
        self.pos = to;
    }

    /// The pass is over. Only a file that filled a window gets anything: the previous
    /// window is released, and the tail is started (written) or released (read).
    pub(crate) fn finish(&mut self, file: &File) {
        if !self.used {
            return;
        }
        if let Some((offset, len)) = self.prev.take() {
            (self.hook)(file, offset, len, CacheOp::Release);
        }
        if self.pos > self.start {
            let op = if self.writes {
                CacheOp::StartWriteback
            } else {
                CacheOp::Release
            };
            (self.hook)(file, self.start, self.pos - self.start, op);
        }
    }
}

/// A reader or writer over a file that reports its windows to the hook. Without a hook (or for
/// a small file) it is the inner value and one `Option` check per call.
pub(crate) struct Hygienic<T> {
    inner: T,
    state: Option<Tracked>,
}

struct Tracked {
    windows: Windows,
    /// Another descriptor of the same file: the hook needs a `File`, the inner value owns the
    /// first one. Ranges and page cache belong to the file, not to the descriptor.
    file: File,
    control: Control,
}

impl Hygienic<Timed<File>> {
    /// Takes `file`, timed when stats are on. Tracking starts only when `opts` has a hook and,
    /// for a file that is read, when it is at least [`TRACK_MIN`] long; a file that is written
    /// is tracked from its first byte, since its size is not known yet.
    pub(crate) fn open(file: File, opts: &Options, writes: bool) -> Self {
        let state = opts.cache_hook.clone().and_then(|hook| {
            let big = writes || file.metadata().is_ok_and(|m| m.len() >= TRACK_MIN);
            let copy = big.then(|| file.try_clone().ok()).flatten()?;
            Some(Tracked {
                windows: Windows::new(hook, writes),
                file: copy,
                control: opts.control.clone(),
            })
        });
        Hygienic {
            inner: Timed::new(file, opts.stats.clone()),
            state,
        }
    }
}

impl<T> Hygienic<T> {
    fn moved(&mut self, n: usize, request: usize) {
        if let Some(t) = &mut self.state {
            let window = window_bytes(&t.control, request);
            t.windows.advance(&t.file, n as u64, window);
        }
    }
}

impl<T: Read> Read for Hygienic<T> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.moved(n, buf.len());
        Ok(n)
    }
}

impl<T: Write> Write for Hygienic<T> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.moved(n, buf.len());
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<T: Seek> Seek for Hygienic<T> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let at = self.inner.seek(pos)?;
        if let Some(t) = &mut self.state {
            t.windows.seek(at);
        }
        Ok(at)
    }
}

impl<T> Drop for Hygienic<T> {
    fn drop(&mut self) {
        if let Some(t) = &mut self.state {
            t.windows.finish(&t.file);
        }
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for Hygienic<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hygienic")
            .field("inner", &self.inner)
            .field("tracked", &self.state.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    type Log = Arc<Mutex<Vec<(u64, u64, CacheOp)>>>;

    fn recorder() -> (CacheHook, Log) {
        let log: Log = Arc::default();
        let sink = log.clone();
        let hook: CacheHook = Arc::new(move |_, offset, len, op| {
            sink.lock().unwrap().push((offset, len, op));
        });
        (hook, log)
    }

    fn file() -> File {
        tempfile::tempfile().unwrap()
    }

    use CacheOp::{Release, StartWriteback};

    #[test]
    fn written_windows_are_started_then_released_one_window_later() {
        let (hook, log) = recorder();
        let f = file();
        let mut w = Windows::new(hook, true);
        // A window closes on the first request that reaches 1000 bytes: here, at 1200.
        for _ in 0..3 {
            w.advance(&f, 400, 1000);
        }
        assert_eq!(*log.lock().unwrap(), vec![(0, 1200, StartWriteback)]);
        for _ in 0..3 {
            w.advance(&f, 400, 1000);
        }
        assert_eq!(
            *log.lock().unwrap(),
            vec![
                (0, 1200, StartWriteback),
                (0, 1200, Release),
                (1200, 1200, StartWriteback),
            ]
        );
    }

    #[test]
    fn read_windows_are_released_at_once() {
        let (hook, log) = recorder();
        let f = file();
        let mut w = Windows::new(hook, false);
        w.advance(&f, 600, 1000);
        assert!(log.lock().unwrap().is_empty());
        w.advance(&f, 600, 1000);
        assert_eq!(*log.lock().unwrap(), vec![(0, 1200, Release)]);
    }

    #[test]
    fn a_seek_starts_a_new_window_without_releasing_the_old_one() {
        let (hook, log) = recorder();
        let f = file();
        let mut w = Windows::new(hook, true);
        w.advance(&f, 900, 1000);
        w.seek(0);
        w.advance(&f, 900, 1000);
        assert!(log.lock().unwrap().is_empty());
        w.advance(&f, 200, 1000);
        assert_eq!(*log.lock().unwrap(), vec![(0, 1100, StartWriteback)]);
    }

    #[test]
    fn a_file_that_never_filled_a_window_costs_nothing_at_the_end() {
        for writes in [true, false] {
            let (hook, log) = recorder();
            let f = file();
            let mut w = Windows::new(hook, writes);
            w.advance(&f, 999, 1000);
            w.finish(&f);
            assert!(log.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn the_end_of_a_big_written_file_releases_the_last_window_and_starts_the_tail() {
        let (hook, log) = recorder();
        let f = file();
        let mut w = Windows::new(hook, true);
        w.advance(&f, 1000, 1000);
        w.advance(&f, 300, 1000);
        w.finish(&f);
        assert_eq!(
            *log.lock().unwrap(),
            vec![
                (0, 1000, StartWriteback),
                (0, 1000, Release),
                (1000, 300, StartWriteback),
            ]
        );
    }

    #[test]
    fn the_end_of_a_big_read_file_releases_the_tail() {
        let (hook, log) = recorder();
        let f = file();
        let mut w = Windows::new(hook, false);
        w.advance(&f, 1000, 1000);
        w.advance(&f, 300, 1000);
        w.finish(&f);
        assert_eq!(
            *log.lock().unwrap(),
            vec![(0, 1000, Release), (1000, 300, Release)]
        );
    }

    #[test]
    fn the_window_follows_io_depth_and_chunk_within_limits() {
        let c = Control::new();
        assert_eq!(window_bytes(&c, 128 << 10), MIN_WINDOW);
        assert_eq!(window_bytes(&c, 1 << 20), 4 << 20);
        c.set_io_depth(8);
        assert_eq!(window_bytes(&c, 1 << 20), 8 << 20);
        c.set_io_depth(1000);
        assert_eq!(window_bytes(&c, 4 << 20), MAX_WINDOW);
    }
}
