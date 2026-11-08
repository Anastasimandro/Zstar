//! Turning files and directories on disk into the entries of a new archive.
//!
//! The tree is walked lazily, in the same order the old whole-tree list had: each input in
//! turn, parents before children, siblings sorted by name. Memory is bounded by the directory
//! listings the walk is inside of, not by the size of the tree: see [`SourceWalker`]. A format
//! reads the entries through a [`SourceStream`]; when archiving files the walk runs on a helper
//! thread that feeds a bounded queue, so a slow disk walk overlaps with compression and the
//! first entry reaches the format at once.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{
    AtomicBool, AtomicU64, Ordering::Acquire, Ordering::Relaxed, Ordering::Release,
};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::SystemTime;

use crate::control::Control;
use crate::error::{Error, LimitKind, Result};
use crate::format::EntryKind;
use crate::options::{Limits, Options};
use crate::stats::{self, Class, FsOp, Stats};

/// Entries that may wait in the queue between the walk and the format. With the batch being
/// filled and the one being read, at most `QUEUE_ENTRIES + 2 * BATCH_ENTRIES` are in flight.
pub(crate) const QUEUE_ENTRIES: usize = 1024;

/// Entries handed over per queue message.
const BATCH_ENTRIES: usize = 128;

/// How often, in directory entries, a long directory listing looks at the cancel flag.
const CANCEL_EVERY: usize = 1024;

/// Bytes charged per entry on top of its name and link target, for what a container library
/// keeps per entry until it writes its index (central directory, header). An estimate.
#[cfg_attr(not(any(feature = "zip", feature = "sevenz")), allow(dead_code))]
pub(crate) const ENTRY_OVERHEAD: u64 = 128;

/// One file-system object to be stored in a new archive.
#[derive(Debug, Clone)]
pub struct SourceEntry {
    /// Where the data lives on disk.
    pub path: PathBuf,
    /// Name inside the archive: relative, `/`-separated, no trailing slash.
    pub name: String,
    /// File, directory or symlink.
    pub kind: EntryKind,
    /// Size in bytes (0 for directories and links).
    pub size: u64,
    /// Unix permission bits (`0o777` mask), where the platform has them.
    pub mode: Option<u32>,
    /// Modification time.
    pub mtime: Option<SystemTime>,
    /// Target of a symlink, as stored on disk.
    pub link_target: Option<String>,
}

#[cfg(unix)]
fn mode_of(meta: &fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(meta.permissions().mode() & 0o777)
}

#[cfg(not(unix))]
fn mode_of(_meta: &fs::Metadata) -> Option<u32> {
    None
}

fn utf8(name: &std::ffi::OsStr) -> Result<&str> {
    name.to_str().ok_or_else(|| {
        Error::InvalidOption(format!(
            "file name is not valid UTF-8: {}",
            name.to_string_lossy()
        ))
    })
}

fn describe(path: &Path, name: String, stats: Option<&Stats>) -> Result<Option<SourceEntry>> {
    let meta = stats::fs(stats, FsOp::StatSrc, || fs::symlink_metadata(path))?;
    let ft = meta.file_type();
    let mut entry = SourceEntry {
        path: path.to_owned(),
        name,
        kind: EntryKind::File,
        size: 0,
        mode: mode_of(&meta),
        mtime: meta.modified().ok(),
        link_target: None,
    };
    if ft.is_symlink() {
        entry.kind = EntryKind::Symlink;
        let target = stats::timed(stats, Class::Input, || fs::read_link(path))?;
        entry.link_target = Some(utf8(target.as_os_str())?.to_owned());
    } else if ft.is_dir() {
        entry.kind = EntryKind::Dir;
    } else if ft.is_file() {
        entry.size = meta.len();
    } else {
        return Ok(None); // sockets, FIFOs, devices
    }
    Ok(Some(entry))
}

/// The totals of a stream: file bytes and entries. Known when the whole tree has been walked,
/// which is later than the first entry; progress shows no total until then.
#[derive(Debug, Default)]
pub struct Totals {
    bytes: AtomicU64,
    entries: AtomicU64,
    complete: AtomicBool,
}

impl Totals {
    /// `(bytes, entries)`, or `None` while the walk is still going.
    pub fn known(&self) -> Option<(u64, u64)> {
        self.complete
            .load(Acquire)
            .then(|| (self.bytes.load(Relaxed), self.entries.load(Relaxed)))
    }

    fn set(&self, bytes: u64, entries: u64) {
        self.bytes.store(bytes, Relaxed);
        self.entries.store(entries, Relaxed);
        self.complete.store(true, Release);
    }
}

/// Where a format reads the entries of a new archive from: in order, parents before
/// children, siblings sorted. An error ends the stream.
pub trait SourceStream {
    /// The next entry, or `None` at the end.
    fn next_entry(&mut self) -> Result<Option<SourceEntry>>;

    /// The totals of the whole stream; see [`Totals`].
    fn totals(&self) -> Arc<Totals>;
}

/// A stream over entries that are already listed.
pub struct Listed<'a> {
    rest: std::slice::Iter<'a, SourceEntry>,
    totals: Arc<Totals>,
}

impl<'a> Listed<'a> {
    pub fn new(entries: &'a [SourceEntry]) -> Self {
        let totals = Arc::new(Totals::default());
        totals.set(entries.iter().map(|e| e.size).sum(), entries.len() as u64);
        Listed {
            rest: entries.iter(),
            totals,
        }
    }
}

impl SourceStream for Listed<'_> {
    fn next_entry(&mut self) -> Result<Option<SourceEntry>> {
        Ok(self.rest.next().cloned())
    }

    fn totals(&self) -> Arc<Totals> {
        self.totals.clone()
    }
}

/// Meters the metadata a format keeps in memory until it ends, against
/// [`Limits::metadata_limit`]. Exceeding it is an error, never a skipped entry.
#[cfg_attr(not(any(feature = "zip", feature = "sevenz")), allow(dead_code))]
pub(crate) struct MetaBudget {
    used: u64,
    max: u64,
}

#[cfg_attr(not(any(feature = "zip", feature = "sevenz")), allow(dead_code))]
impl MetaBudget {
    pub(crate) fn new(max: u64) -> Self {
        MetaBudget { used: 0, max }
    }

    pub(crate) fn charge_bytes(&mut self, n: u64) -> Result<()> {
        self.used = self.used.saturating_add(n);
        if self.used > self.max {
            return Err(Error::LimitExceeded(LimitKind::Metadata));
        }
        Ok(())
    }

    /// One entry that a container library will remember until it writes its index.
    pub(crate) fn charge(&mut self, entry: &SourceEntry) -> Result<()> {
        let target = entry.link_target.as_ref().map_or(0, String::len);
        self.charge_bytes((entry.name.len() + target) as u64 + ENTRY_OVERHEAD)
    }
}

/// The names inside one directory being walked, kept compactly: one byte buffer and a
/// `(offset, length)` pair per name, sorted by name.
struct Frame {
    dir: PathBuf,
    /// Archive name of `dir`.
    name: String,
    /// Components of `name`.
    depth: u64,
    names: Vec<u8>,
    spans: Vec<(u32, u32)>,
    next: usize,
}

impl Frame {
    /// What this listing holds in memory, for the metadata limit.
    fn bytes(&self) -> u64 {
        (self.names.capacity()
            + self.spans.capacity() * 8
            + self.dir.as_os_str().len()
            + self.name.len()) as u64
    }
}

/// A lazy walk over `inputs` and everything below them, in the order the archive will have:
/// each input in turn, parents before children, siblings sorted by name. Symlinks are
/// stored, not followed. Entries whose canonical path equals one of `exclude` are skipped.
///
/// Memory is the listings of the directories on the way down to the current entry (names
/// only, about the length of the name plus 8 bytes each), so a deep and narrow tree costs
/// almost nothing and one directory of a million files costs a few tens of megabytes. The
/// listings count against [`Limits::max_metadata_bytes`], and entries, name length and depth
/// against their own limits: exceeding one ends the walk with a
/// [`LimitExceeded`](Error::LimitExceeded) error.
pub(crate) struct SourceWalker<'a> {
    inputs: std::slice::Iter<'a, PathBuf>,
    exclude: &'a [PathBuf],
    limits: Limits,
    control: Option<&'a Control>,
    stats: Option<&'a Stats>,
    stack: Vec<Frame>,
    pending_bytes: u64,
    /// Entries returned so far and the bytes of the files among them.
    count: u64,
    bytes: u64,
    done: bool,
}

fn base_name(input: &Path) -> Result<String> {
    Ok(match input.file_name() {
        Some(n) => utf8(n)?.to_owned(),
        None => {
            utf8(fs::canonicalize(input)?.file_name().ok_or_else(|| {
                Error::InvalidOption(format!("cannot archive {}", input.display()))
            })?)?
            .to_owned()
        }
    })
}

impl<'a> SourceWalker<'a> {
    pub(crate) fn new(
        inputs: &'a [PathBuf],
        exclude: &'a [PathBuf],
        limits: Limits,
        control: Option<&'a Control>,
        stats: Option<&'a Stats>,
    ) -> Self {
        SourceWalker {
            inputs: inputs.iter(),
            exclude,
            limits,
            control,
            stats,
            stack: Vec::new(),
            pending_bytes: 0,
            count: 0,
            bytes: 0,
            done: false,
        }
    }

    /// The next entry, `None` at the end. After an error or the end it keeps returning `None`.
    pub(crate) fn next_entry(&mut self) -> Result<Option<SourceEntry>> {
        if self.done {
            return Ok(None);
        }
        let r = self.advance();
        if !matches!(r, Ok(Some(_))) {
            self.done = true;
            self.stack.clear();
        }
        r
    }

    fn check_cancel(&self) -> Result<()> {
        match self.control {
            Some(c) if c.is_cancelled() => Err(Error::Cancelled),
            _ => Ok(()),
        }
    }

    fn advance(&mut self) -> Result<Option<SourceEntry>> {
        loop {
            self.check_cancel()?;
            let (path, name, depth) = if let Some(frame) = self.stack.last_mut() {
                if frame.next == frame.spans.len() {
                    let gone = self.stack.pop().map_or(0, |f| f.bytes());
                    self.pending_bytes = self.pending_bytes.saturating_sub(gone);
                    continue;
                }
                let (off, len) = frame.spans[frame.next];
                frame.next += 1;
                let child =
                    std::str::from_utf8(&frame.names[off as usize..(off + len) as usize])
                        .map_err(|_| Error::InvalidOption("file name is not valid UTF-8".into()))?;
                (
                    frame.dir.join(child),
                    format!("{}/{child}", frame.name),
                    frame.depth + 1,
                )
            } else {
                let Some(input) = self.inputs.next() else {
                    return Ok(None);
                };
                (input.clone(), base_name(input)?, 1)
            };
            if !self.exclude.is_empty() && is_excluded(&path, self.exclude) {
                continue;
            }
            let Some(entry) = describe(&path, name, self.stats)? else {
                continue; // sockets, FIFOs, devices
            };
            self.admit(&entry, depth)?;
            if entry.kind == EntryKind::Dir {
                self.expand(&path, &entry.name, depth)?;
            }
            return Ok(Some(entry));
        }
    }

    /// Counts `entry` against the entry, name length and depth limits.
    fn admit(&mut self, entry: &SourceEntry, depth: u64) -> Result<()> {
        self.count += 1;
        if self.count > self.limits.max_entries {
            return Err(Error::LimitExceeded(LimitKind::Entries));
        }
        if entry.name.len() as u64 > self.limits.max_path_bytes {
            return Err(Error::LimitExceeded(LimitKind::PathLength));
        }
        if depth > self.limits.max_depth {
            return Err(Error::LimitExceeded(LimitKind::Depth));
        }
        self.bytes = self.bytes.saturating_add(entry.size);
        Ok(())
    }

    /// Reads the names of `dir` into a sorted [`Frame`] and goes inside it.
    fn expand(&mut self, dir: &Path, name: &str, depth: u64) -> Result<()> {
        let mut names = Vec::new();
        let mut spans: Vec<(u32, u32)> = Vec::new();
        let mut invalid = Vec::new();
        for (i, child) in fs::read_dir(dir)?.enumerate() {
            if i % CANCEL_EVERY == 0 {
                self.check_cancel()?;
            }
            let child = child?.file_name();
            match child.to_str() {
                Some(s) => {
                    let (Ok(off), Ok(len)) = (u32::try_from(names.len()), u32::try_from(s.len()))
                    else {
                        return Err(Error::LimitExceeded(LimitKind::Metadata));
                    };
                    spans.push((off, len));
                    names.extend_from_slice(s.as_bytes());
                }
                None => invalid.push(child),
            }
        }
        if !invalid.is_empty() {
            // The same error as before: the first bad name in sorted order.
            invalid.sort();
            return Err(Error::InvalidOption(format!(
                "file name is not valid UTF-8: {}",
                invalid[0].to_string_lossy()
            )));
        }
        // Names are unique inside a directory, so an unstable sort is deterministic; byte order
        // of valid UTF-8 is the order `OsString` has.
        spans.sort_unstable_by(|a, b| {
            names[a.0 as usize..(a.0 + a.1) as usize]
                .cmp(&names[b.0 as usize..(b.0 + b.1) as usize])
        });
        let frame = Frame {
            dir: dir.to_owned(),
            name: name.to_owned(),
            depth,
            names,
            spans,
            next: 0,
        };
        self.pending_bytes = self.pending_bytes.saturating_add(frame.bytes());
        if self.pending_bytes > self.limits.metadata_limit() {
            return Err(Error::LimitExceeded(LimitKind::Metadata));
        }
        self.stack.push(frame);
        Ok(())
    }
}

/// What the walk thread sends through the queue. Entries travel in batches: one hand-over per
/// entry costs a wake-up of the other thread each time (measured: +14% on 100,000 files).
enum Msg {
    Batch(Vec<SourceEntry>),
    Done,
    Failed(Error),
}

/// The format's end of the queue.
struct Queued<'a> {
    rx: Receiver<Msg>,
    batch: std::vec::IntoIter<SourceEntry>,
    totals: Arc<Totals>,
    stats: Option<&'a Stats>,
    ended: bool,
}

impl SourceStream for Queued<'_> {
    fn next_entry(&mut self) -> Result<Option<SourceEntry>> {
        loop {
            if let Some(entry) = self.batch.next() {
                return Ok(Some(entry));
            }
            if self.ended {
                return Ok(None);
            }
            // Waiting for the walk is waiting for input.
            match stats::timed(self.stats, Class::Input, || self.rx.recv()) {
                Ok(Msg::Batch(entries)) => self.batch = entries.into_iter(),
                Ok(Msg::Done) => self.ended = true,
                Ok(Msg::Failed(e)) => {
                    self.ended = true;
                    return Err(e);
                }
                // The walk thread is gone without saying it was done: never read that as the end.
                Err(_) => {
                    self.ended = true;
                    return Err(Error::Io(io::Error::other(
                        "the source walker stopped unexpectedly",
                    )));
                }
            }
        }
    }

    fn totals(&self) -> Arc<Totals> {
        self.totals.clone()
    }
}

/// The walk thread: walks and sends until the end, an error, or the format hanging up.
fn feed(mut walker: SourceWalker<'_>, tx: SyncSender<Msg>, totals: &Totals, stats: Option<&Stats>) {
    let _lane = stats::lane(stats);
    let mut batch = Vec::with_capacity(BATCH_ENTRIES);
    // A full queue means the format is the slow side: idle time, counted nowhere.
    let send = |msg| stats::timed(stats, Class::Idle, || tx.send(msg)).is_ok();
    loop {
        let end = match walker.next_entry() {
            Ok(Some(entry)) => {
                batch.push(entry);
                if batch.len() < BATCH_ENTRIES {
                    continue;
                }
                None
            }
            Ok(None) => {
                totals.set(walker.bytes, walker.count);
                Some(Msg::Done)
            }
            Err(e) => Some(Msg::Failed(e)),
        };
        // Entries found before an error or the end are delivered first, in order.
        if !batch.is_empty() {
            let full = std::mem::replace(&mut batch, Vec::with_capacity(BATCH_ENTRIES));
            if !send(Msg::Batch(full)) {
                return;
            }
        }
        if let Some(msg) = end {
            send(msg);
            return;
        }
    }
}

/// Walks `inputs` on a helper thread, hands the entries to `f` through a bounded queue and
/// returns what `f` returns. The helper stops when `f` returns, so a format that fails or is
/// cancelled does not leave a walk behind.
pub(crate) fn with_walk<T>(
    inputs: &[PathBuf],
    exclude: &[PathBuf],
    opts: &Options,
    f: impl FnOnce(&mut dyn SourceStream) -> T,
) -> T {
    let totals = Arc::new(Totals::default());
    let stats = opts.stats.as_ref();
    let walker = SourceWalker::new(inputs, exclude, opts.limits, Some(&opts.control), stats);
    let (tx, rx) = sync_channel(QUEUE_ENTRIES / BATCH_ENTRIES);
    std::thread::scope(|scope| {
        let shared = totals.clone();
        scope.spawn(move || feed(walker, tx, &shared, stats));
        let mut stream = Queued {
            rx,
            batch: Vec::new().into_iter(),
            totals,
            stats,
            ended: false,
        };
        let out = f(&mut stream);
        // Hang up before the scope joins, or a walk waiting on a full queue would never end.
        drop(stream);
        out
    })
}

/// Lists `inputs` and everything below them, parents before children, siblings sorted by
/// name so the output is deterministic. Symlinks are stored, not followed.
///
/// Each input appears in the archive under its own file name. Entries whose canonical path
/// equals one of `exclude` are skipped (use it to keep an output archive out of itself). The
/// default [`Limits`] apply; this holds the whole list in memory, so prefer
/// [`create_file`](crate::create_file), which streams.
pub fn collect_sources(inputs: &[PathBuf], exclude: &[PathBuf]) -> Result<Vec<SourceEntry>> {
    let mut walker = SourceWalker::new(inputs, exclude, Limits::default(), None, None);
    let mut out = Vec::new();
    while let Some(entry) = walker.next_entry()? {
        out.push(entry);
    }
    Ok(out)
}

fn is_excluded(path: &Path, exclude: &[PathBuf]) -> bool {
    // Cheap pre-check on the file name before touching the file system.
    if !exclude.iter().any(|e| e.file_name() == path.file_name()) {
        return false;
    }
    fs::canonicalize(path).is_ok_and(|p| exclude.contains(&p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walks_in_sorted_parent_first_order_and_honours_exclude() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        fs::create_dir_all(root.join("b")).unwrap();
        fs::create_dir_all(root.join("a")).unwrap();
        fs::write(root.join("b/z.txt"), b"zz").unwrap();
        fs::write(root.join("a/y.txt"), b"y").unwrap();
        fs::write(root.join("skip.bin"), b"s").unwrap();

        let excl = fs::canonicalize(root.join("skip.bin")).unwrap();
        let got = collect_sources(std::slice::from_ref(&root), &[excl]).unwrap();
        let names: Vec<_> = got.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            ["proj", "proj/a", "proj/a/y.txt", "proj/b", "proj/b/z.txt"]
        );
        assert_eq!(got[4].size, 2);
        assert_eq!(got[1].kind, EntryKind::Dir);
    }

    #[test]
    fn dot_input_uses_the_real_directory_name() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("f"), b"1").unwrap();
        let got = collect_sources(&[tmp.path().join(".")], &[]).unwrap();
        let top = tmp.path().file_name().unwrap().to_str().unwrap();
        assert_eq!(got[0].name, top);
        assert_eq!(got[1].name, format!("{top}/f"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_stored_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/definitely/elsewhere", tmp.path().join("l")).unwrap();
        let got = collect_sources(&[tmp.path().join("l")], &[]).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, EntryKind::Symlink);
        assert_eq!(got[0].link_target.as_deref(), Some("/definitely/elsewhere"));
    }

    fn tree_of(n: usize) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("t");
        for i in 0..n {
            let dir = root.join(format!("d{:02}", i % 7));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(format!("f{i:05}")), b"ab").unwrap();
        }
        (tmp, root)
    }

    #[test]
    fn streamed_order_and_totals_match_the_list() {
        // More entries than the queue holds, so the walker really has to wait for the reader.
        let (_tmp, root) = tree_of(QUEUE_ENTRIES * 3);
        let want = collect_sources(std::slice::from_ref(&root), &[]).unwrap();
        let opts = Options::default();
        let (names, totals) = with_walk(std::slice::from_ref(&root), &[], &opts, |stream| {
            let mut names = Vec::new();
            while let Some(e) = stream.next_entry().unwrap() {
                names.push(e.name);
            }
            (names, stream.totals())
        });
        let want_names: Vec<_> = want.iter().map(|e| e.name.clone()).collect();
        assert_eq!(names, want_names);
        let bytes: u64 = want.iter().map(|e| e.size).sum();
        assert_eq!(totals.known(), Some((bytes, want.len() as u64)));
    }

    #[test]
    fn totals_are_unknown_until_the_walk_is_complete() {
        let (_tmp, root) = tree_of(QUEUE_ENTRIES * 2);
        let opts = Options::default();
        with_walk(std::slice::from_ref(&root), &[], &opts, |stream| {
            let totals = stream.totals();
            stream.next_entry().unwrap().unwrap();
            // The helper cannot have finished: the queue holds far fewer entries than the tree.
            assert_eq!(totals.known(), None);
        });
    }

    #[test]
    fn dropping_the_stream_early_ends_the_walk() {
        let (_tmp, root) = tree_of(QUEUE_ENTRIES * 4);
        let opts = Options::default();
        // Returns instead of hanging on a full queue.
        with_walk(std::slice::from_ref(&root), &[], &opts, |stream| {
            stream.next_entry().unwrap().unwrap();
        });
    }

    #[test]
    fn limits_stop_the_walk_with_a_clear_error() {
        let (_tmp, root) = tree_of(40);
        let inputs = std::slice::from_ref(&root);
        let walk = |f: fn(&mut Limits)| {
            let mut limits = Limits::default();
            f(&mut limits);
            let mut w = SourceWalker::new(inputs, &[], limits, None, None);
            loop {
                match w.next_entry() {
                    Ok(Some(_)) => {}
                    Ok(None) => return Ok(()),
                    Err(e) => return Err(e),
                }
            }
        };
        assert!(matches!(
            walk(|l| l.max_entries = 10),
            Err(Error::LimitExceeded(LimitKind::Entries))
        ));
        assert!(matches!(
            walk(|l| l.max_depth = 1),
            Err(Error::LimitExceeded(LimitKind::Depth))
        ));
        assert!(matches!(
            walk(|l| l.max_path_bytes = 4),
            Err(Error::LimitExceeded(LimitKind::PathLength))
        ));
        assert!(matches!(
            walk(|l| l.max_metadata_bytes = Some(8)),
            Err(Error::LimitExceeded(LimitKind::Metadata))
        ));
        assert!(walk(|_| {}).is_ok());
    }

    #[test]
    fn a_cancelled_walk_stops() {
        let (_tmp, root) = tree_of(40);
        let control = Control::default();
        control.cancel();
        let mut w = SourceWalker::new(
            std::slice::from_ref(&root),
            &[],
            Limits::default(),
            Some(&control),
            None,
        );
        assert!(matches!(w.next_entry(), Err(Error::Cancelled)));
        // After an error the walker is finished, never half-alive.
        assert!(matches!(w.next_entry(), Ok(None)));
    }

    #[cfg(unix)]
    #[test]
    fn a_name_that_is_not_utf8_is_an_error_not_a_skip() {
        use std::os::unix::ffi::OsStrExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("t");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("fine"), b"1").unwrap();
        fs::write(root.join(std::ffi::OsStr::from_bytes(b"bad\xff")), b"2").unwrap();
        let r = collect_sources(std::slice::from_ref(&root), &[]);
        assert!(matches!(r, Err(Error::InvalidOption(m)) if m.contains("UTF-8")));
    }
}
