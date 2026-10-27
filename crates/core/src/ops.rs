//! Format-independent extract and compress operations. Everything that
//! touches the file system lives here: safe paths, `.part` files, progress
//! and cancellation. Format backends only move bytes.

use std::fs::{self, File};
use std::io::{self, BufReader, Read, Seek, Write};
use std::path::{Path, PathBuf};

use crate::archive::{ArchiveReader, ArchiveWriter, Entry, create_writer, open_reader};
use crate::error::{Error, Result};
use crate::format::Format;
use crate::path::safe_join;
use crate::progress::{Observer, Progress};

/// Progress is reported once per entry and after this many bytes of data.
const REPORT_EVERY: u64 = 256 * 1024;

/// Buffer between the archive file and the format backend.
const FILE_BUF: usize = 64 * 1024;

#[derive(Clone, Debug, Default)]
pub struct ExtractOptions {
    /// Used for encrypted entries; ignored for the others.
    pub password: Option<Vec<u8>>,
    /// Replace files that already exist instead of failing.
    pub overwrite: bool,
}

/// What an operation did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub entries: usize,
    /// Uncompressed bytes processed.
    pub bytes: u64,
}

struct Tracker<'a> {
    obs: &'a mut dyn Observer,
    bytes_done: u64,
    bytes_total: u64,
    entries_done: usize,
    entries_total: usize,
    unreported: u64,
}

impl<'a> Tracker<'a> {
    fn new(obs: &'a mut dyn Observer, bytes_total: u64, entries_total: usize) -> Self {
        Self {
            obs,
            bytes_done: 0,
            bytes_total,
            entries_done: 0,
            entries_total,
            unreported: 0,
        }
    }

    fn report(&mut self, current: &str) {
        self.unreported = 0;
        self.obs.on_progress(&Progress {
            bytes_done: self.bytes_done,
            bytes_total: self.bytes_total,
            entries_done: self.entries_done,
            entries_total: self.entries_total,
            current,
        });
    }

    fn advance(&mut self, bytes: u64, current: &str) {
        self.bytes_done += bytes;
        self.unreported += bytes;
        if self.unreported >= REPORT_EVERY {
            self.report(current);
        }
    }

    fn cancelled(&self) -> bool {
        self.obs.is_cancelled()
    }
}

/// Counts what is written, reports progress and stops when cancelled.
struct TrackedWriter<'t, 'a> {
    inner: &'t mut dyn Write,
    tracker: &'t mut Tracker<'a>,
    current: &'t str,
}

impl Write for TrackedWriter<'_, '_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.tracker.cancelled() {
            return Err(io::Error::other("operation cancelled"));
        }
        let written = self.inner.write(buf)?;
        self.tracker.advance(written as u64, self.current);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Counts what is read, reports progress and stops when cancelled.
struct TrackedReader<'t, 'a> {
    inner: &'t mut dyn Read,
    tracker: &'t mut Tracker<'a>,
    current: &'t str,
}

impl Read for TrackedReader<'_, '_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.tracker.cancelled() {
            return Err(io::Error::other("operation cancelled"));
        }
        let read = self.inner.read(buf)?;
        self.tracker.advance(read as u64, self.current);
        Ok(read)
    }
}

fn part_path(target: &Path) -> PathBuf {
    let mut name = target.as_os_str().to_owned();
    name.push(".part");
    PathBuf::from(name)
}

#[cfg(unix)]
fn apply_mode(file: &File, mode: Option<u32>) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(mode) = mode {
        // Keep the owner able to read and write, and drop setuid/setgid.
        file.set_permissions(fs::Permissions::from_mode((mode & 0o777) | 0o600))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn apply_mode(_file: &File, _mode: Option<u32>) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn mode_of(meta: &fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(meta.permissions().mode())
}

#[cfg(not(unix))]
fn mode_of(_meta: &fs::Metadata) -> Option<u32> {
    None
}

// ---------------------------------------------------------------- extract

/// Extracts every entry into `dest`.
///
/// All entry names are checked first, so a malicious archive fails before
/// anything is written. Each file is written to `<name>.part` and renamed
/// when complete; a failed or cancelled file leaves no `.part` behind.
/// Existing files are an error unless `opts.overwrite` is set. Modification
/// times are not restored.
pub fn extract_all(
    reader: &mut dyn ArchiveReader,
    dest: &Path,
    opts: &ExtractOptions,
    obs: &mut dyn Observer,
) -> Result<Summary> {
    let entries = reader.entries()?;
    let targets = entries
        .iter()
        .map(|entry| safe_join(dest, &entry.name))
        .collect::<Result<Vec<_>>>()?;
    let bytes_total = entries
        .iter()
        .filter(|entry| !entry.is_dir)
        .map(|entry| entry.size)
        .sum::<u64>();

    fs::create_dir_all(dest)?;
    let mut tracker = Tracker::new(obs, bytes_total, entries.len());
    for (index, (entry, target)) in entries.iter().zip(&targets).enumerate() {
        if tracker.cancelled() {
            return Err(Error::Cancelled);
        }
        tracker.report(&entry.name);
        if entry.is_dir {
            fs::create_dir_all(target)?;
        } else {
            extract_entry(reader, index, entry, target, opts, &mut tracker)?;
        }
        tracker.entries_done += 1;
    }
    tracker.report("");
    Ok(Summary {
        entries: entries.len(),
        bytes: tracker.bytes_done,
    })
}

fn extract_entry(
    reader: &mut dyn ArchiveReader,
    index: usize,
    entry: &Entry,
    target: &Path,
    opts: &ExtractOptions,
    tracker: &mut Tracker<'_>,
) -> Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    if !opts.overwrite && target.exists() {
        return Err(Error::AlreadyExists(target.to_path_buf()));
    }

    let part = part_path(target);
    let written = write_part(reader, index, entry, &part, opts, tracker);
    let result = written.and_then(|()| fs::rename(&part, target).map_err(Error::Io));
    if let Err(error) = result {
        let _ = fs::remove_file(&part);
        return Err(if tracker.cancelled() {
            Error::Cancelled
        } else {
            error
        });
    }
    Ok(())
}

fn write_part(
    reader: &mut dyn ArchiveReader,
    index: usize,
    entry: &Entry,
    part: &Path,
    opts: &ExtractOptions,
    tracker: &mut Tracker<'_>,
) -> Result<()> {
    let mut file = File::create(part)?;
    {
        let mut out = TrackedWriter {
            inner: &mut file,
            tracker: &mut *tracker,
            current: &entry.name,
        };
        reader.read_entry(index, opts.password.as_deref(), &mut out)?;
    }
    apply_mode(&file, entry.unix_mode)?;
    Ok(())
}

/// Opens the archive at `archive` (format detected by magic bytes) and
/// extracts it into `dest`. See `extract_all`.
pub fn extract_file(
    archive: &Path,
    dest: &Path,
    opts: &ExtractOptions,
    obs: &mut dyn Observer,
) -> Result<Summary> {
    let file = File::open(archive)?;
    let mut reader = open_reader(BufReader::with_capacity(FILE_BUF, file))?;
    extract_all(&mut *reader, dest, opts, obs)
}

// --------------------------------------------------------------- compress

struct Item {
    path: PathBuf,
    name: String,
    is_dir: bool,
    size: u64,
    mode: Option<u32>,
}

/// Lists what to add: each input by its own name, directories recursively
/// in name order. Symbolic links found inside directories are skipped.
fn collect_items(inputs: &[PathBuf]) -> Result<Vec<Item>> {
    let mut items = Vec::new();
    for input in inputs {
        let meta = fs::metadata(input)?;
        // `.` and `..` have no file name; resolve them to the real directory.
        let resolved;
        let named = match input.file_name() {
            Some(_) => input.as_path(),
            None => {
                resolved = fs::canonicalize(input)?;
                resolved.as_path()
            }
        };
        let name = named
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| Error::InvalidName(input.clone()))?;
        if meta.is_dir() {
            walk(input, name, &mut items)?;
        } else {
            items.push(Item {
                path: input.clone(),
                name: name.to_owned(),
                is_dir: false,
                size: meta.len(),
                mode: mode_of(&meta),
            });
        }
    }
    Ok(items)
}

fn walk(dir: &Path, name: &str, items: &mut Vec<Item>) -> Result<()> {
    items.push(Item {
        path: dir.to_path_buf(),
        name: name.to_owned(),
        is_dir: true,
        size: 0,
        mode: None,
    });

    let mut children = fs::read_dir(dir)?.collect::<io::Result<Vec<_>>>()?;
    children.sort_by_cached_key(|child| child.file_name());
    for child in children {
        let file_type = child.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        let file_name = child.file_name();
        let file_name = file_name
            .to_str()
            .ok_or_else(|| Error::InvalidName(child.path()))?;
        let child_name = format!("{name}/{file_name}");
        if file_type.is_dir() {
            walk(&child.path(), &child_name, items)?;
        } else if file_type.is_file() {
            let meta = child.metadata()?;
            items.push(Item {
                path: child.path(),
                name: child_name,
                is_dir: false,
                size: meta.len(),
                mode: mode_of(&meta),
            });
        }
    }
    Ok(())
}

fn compress_items(
    writer: &mut dyn ArchiveWriter,
    items: &[Item],
    obs: &mut dyn Observer,
) -> Result<Summary> {
    let bytes_total = items
        .iter()
        .filter(|item| !item.is_dir)
        .map(|item| item.size)
        .sum::<u64>();
    let mut tracker = Tracker::new(obs, bytes_total, items.len());
    for item in items {
        if tracker.cancelled() {
            return Err(Error::Cancelled);
        }
        tracker.report(&item.name);
        if item.is_dir {
            writer.add_dir(&item.name)?;
        } else {
            let mut file = File::open(&item.path)?;
            let mut data = TrackedReader {
                inner: &mut file,
                tracker: &mut tracker,
                current: &item.name,
            };
            let added = writer.add_file(&item.name, item.mode, item.size, &mut data);
            if let Err(error) = added {
                return Err(if tracker.cancelled() {
                    Error::Cancelled
                } else {
                    error
                });
            }
        }
        tracker.entries_done += 1;
    }
    tracker.report("");
    Ok(Summary {
        entries: items.len(),
        bytes: tracker.bytes_done,
    })
}

/// Compresses `inputs` into an archive of `format` written to `output`.
pub fn compress_to_writer<W: Write + Seek>(
    format: Format,
    output: W,
    inputs: &[PathBuf],
    obs: &mut dyn Observer,
) -> Result<Summary> {
    let items = collect_items(inputs)?;
    let mut writer = create_writer(format, output)?;
    let summary = compress_items(&mut *writer, &items, obs)?;
    writer.finish()?;
    Ok(summary)
}

/// Compresses `inputs` into the file `output`. The archive is written to
/// `<output>.part` and renamed when complete, so a failed or cancelled run
/// leaves nothing behind. An existing `output` is an error unless
/// `overwrite` is set.
pub fn compress_to_file(
    format: Format,
    output: &Path,
    inputs: &[PathBuf],
    overwrite: bool,
    obs: &mut dyn Observer,
) -> Result<Summary> {
    if !overwrite && output.exists() {
        return Err(Error::AlreadyExists(output.to_path_buf()));
    }
    // Collect before creating the `.part` file so it can never be an input.
    let items = collect_items(inputs)?;
    let part = part_path(output);

    let result = (|| -> Result<Summary> {
        let file = File::create(&part)?;
        let mut writer = create_writer(format, file)?;
        let summary = compress_items(&mut *writer, &items, obs)?;
        writer.finish()?;
        Ok(summary)
    })();

    match result {
        Ok(summary) => match fs::rename(&part, output) {
            Ok(()) => Ok(summary),
            Err(error) => {
                let _ = fs::remove_file(&part);
                Err(Error::Io(error))
            }
        },
        Err(error) => {
            let _ = fs::remove_file(&part);
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::NoObserver;
    use crate::testutil::TempDir;

    struct MockReader {
        items: Vec<(Entry, Vec<u8>)>,
    }

    impl MockReader {
        /// A name ending in `/` is a directory.
        fn new(items: &[(&str, &[u8])]) -> Self {
            let items = items
                .iter()
                .map(|(name, data)| {
                    let entry = Entry {
                        name: (*name).to_owned(),
                        is_dir: name.ends_with('/'),
                        size: data.len() as u64,
                        compressed_size: data.len() as u64,
                        unix_mode: None,
                    };
                    (entry, data.to_vec())
                })
                .collect();
            Self { items }
        }
    }

    impl ArchiveReader for MockReader {
        fn entries(&mut self) -> Result<Vec<Entry>> {
            Ok(self.items.iter().map(|(entry, _)| entry.clone()).collect())
        }

        fn read_entry(
            &mut self,
            index: usize,
            _password: Option<&[u8]>,
            out: &mut dyn Write,
        ) -> Result<u64> {
            let data = &self.items[index].1;
            for chunk in data.chunks(64 * 1024) {
                out.write_all(chunk)?;
            }
            Ok(data.len() as u64)
        }
    }

    #[derive(Default)]
    struct MockWriter {
        /// (name, is_dir, content, mode) in the order added.
        added: Vec<(String, bool, Vec<u8>, Option<u32>)>,
    }

    impl ArchiveWriter for MockWriter {
        fn add_dir(&mut self, name: &str) -> Result<()> {
            self.added.push((name.to_owned(), true, Vec::new(), None));
            Ok(())
        }

        fn add_file(
            &mut self,
            name: &str,
            unix_mode: Option<u32>,
            _size: u64,
            data: &mut dyn Read,
        ) -> Result<u64> {
            let mut content = Vec::new();
            data.read_to_end(&mut content)?;
            let len = content.len() as u64;
            self.added
                .push((name.to_owned(), false, content, unix_mode));
            Ok(len)
        }

        fn finish(self: Box<Self>) -> Result<()> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct Recorder {
        bytes: Vec<u64>,
        last_entries: usize,
    }

    impl Observer for Recorder {
        fn on_progress(&mut self, progress: &Progress<'_>) {
            self.bytes.push(progress.bytes_done);
            self.last_entries = progress.entries_done;
        }
    }

    struct CancelAfter {
        seen: u64,
        limit: u64,
    }

    impl Observer for CancelAfter {
        fn on_progress(&mut self, progress: &Progress<'_>) {
            self.seen = progress.bytes_done;
        }

        fn is_cancelled(&self) -> bool {
            self.seen >= self.limit
        }
    }

    fn leftover_parts(dir: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let mut pending = vec![dir.to_path_buf()];
        while let Some(current) = pending.pop() {
            for child in fs::read_dir(current).unwrap() {
                let path = child.unwrap().path();
                if path.is_dir() {
                    pending.push(path);
                } else if path.extension().is_some_and(|ext| ext == "part") {
                    found.push(path);
                }
            }
        }
        found
    }

    #[test]
    fn extracts_files_and_directories() {
        let tmp = TempDir::new();
        let dest = tmp.path().join("out");
        let mut reader = MockReader::new(&[
            ("top.txt", b"top"),
            ("dir/", b""),
            ("dir/nested/deep.txt", b"deep"),
            ("empty/", b""),
        ]);

        let summary = extract_all(
            &mut reader,
            &dest,
            &ExtractOptions::default(),
            &mut NoObserver,
        )
        .unwrap();

        assert_eq!(
            summary,
            Summary {
                entries: 4,
                bytes: 7
            }
        );
        assert_eq!(fs::read(dest.join("top.txt")).unwrap(), b"top");
        assert_eq!(fs::read(dest.join("dir/nested/deep.txt")).unwrap(), b"deep");
        assert!(dest.join("empty").is_dir());
        assert!(leftover_parts(&dest).is_empty());
    }

    #[test]
    fn unsafe_names_fail_before_anything_is_written() {
        let tmp = TempDir::new();
        let dest = tmp.path().join("out");
        let mut reader = MockReader::new(&[("fine.txt", b"ok"), ("../evil.txt", b"no")]);

        let result = extract_all(
            &mut reader,
            &dest,
            &ExtractOptions::default(),
            &mut NoObserver,
        );

        assert!(matches!(result, Err(Error::UnsafePath(_))));
        assert!(!dest.exists());
        assert!(!tmp.path().join("evil.txt").exists());
    }

    #[test]
    fn existing_files_are_kept_unless_overwrite_is_set() {
        let tmp = TempDir::new();
        let dest = tmp.path().join("out");
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("a.txt"), b"old").unwrap();

        let mut reader = MockReader::new(&[("a.txt", b"new")]);
        let result = extract_all(
            &mut reader,
            &dest,
            &ExtractOptions::default(),
            &mut NoObserver,
        );
        assert!(matches!(result, Err(Error::AlreadyExists(_))));
        assert_eq!(fs::read(dest.join("a.txt")).unwrap(), b"old");

        let opts = ExtractOptions {
            overwrite: true,
            ..ExtractOptions::default()
        };
        extract_all(&mut reader, &dest, &opts, &mut NoObserver).unwrap();
        assert_eq!(fs::read(dest.join("a.txt")).unwrap(), b"new");
        assert!(leftover_parts(&dest).is_empty());
    }

    #[test]
    fn cancelling_stops_and_cleans_up() {
        let tmp = TempDir::new();
        let dest = tmp.path().join("out");
        let big = vec![7u8; 1024 * 1024];
        let mut reader = MockReader::new(&[("big.bin", &big), ("later.txt", b"x")]);
        let mut observer = CancelAfter {
            seen: 0,
            limit: 256 * 1024,
        };

        let result = extract_all(
            &mut reader,
            &dest,
            &ExtractOptions::default(),
            &mut observer,
        );

        assert!(matches!(result, Err(Error::Cancelled)));
        assert!(!dest.join("big.bin").exists());
        assert!(!dest.join("later.txt").exists());
        assert!(leftover_parts(&dest).is_empty());
    }

    #[test]
    fn progress_is_monotonic_and_ends_complete() {
        let tmp = TempDir::new();
        let dest = tmp.path().join("out");
        let big = vec![1u8; 700 * 1024];
        let mut reader = MockReader::new(&[("a.bin", &big), ("b.bin", b"tail")]);
        let mut recorder = Recorder::default();

        let summary = extract_all(
            &mut reader,
            &dest,
            &ExtractOptions::default(),
            &mut recorder,
        )
        .unwrap();

        let total = 700 * 1024 + 4;
        assert_eq!(summary.bytes, total);
        assert!(recorder.bytes.windows(2).all(|pair| pair[0] <= pair[1]));
        assert_eq!(recorder.bytes.last().copied(), Some(total));
        assert_eq!(recorder.last_entries, 2);
        assert!(recorder.bytes.len() > 3, "large files report several times");
    }

    #[cfg(unix)]
    #[test]
    fn unix_mode_is_applied() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new();
        let dest = tmp.path().join("out");
        let mut reader = MockReader::new(&[("run.sh", b"#!/bin/sh")]);
        reader.items[0].0.unix_mode = Some(0o100755);

        extract_all(
            &mut reader,
            &dest,
            &ExtractOptions::default(),
            &mut NoObserver,
        )
        .unwrap();

        let mode = fs::metadata(dest.join("run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
    }

    #[test]
    fn collects_items_in_name_order_and_adds_empty_dirs() {
        let tmp = TempDir::new();
        let root = tmp.path().join("in");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();
        fs::write(root.join("b.txt"), b"bb").unwrap();
        fs::write(root.join("a.txt"), b"a").unwrap();
        fs::write(root.join("sub").join("c.txt"), b"ccc").unwrap();

        let items = collect_items(&[root]).unwrap();
        let mut writer = MockWriter::default();
        let summary = compress_items(&mut writer, &items, &mut NoObserver).unwrap();

        let names: Vec<_> = writer.added.iter().map(|added| added.0.as_str()).collect();
        assert_eq!(
            names,
            [
                "in",
                "in/a.txt",
                "in/b.txt",
                "in/empty",
                "in/sub",
                "in/sub/c.txt"
            ]
        );
        assert_eq!(writer.added[1].2, b"a");
        assert_eq!(writer.added[5].2, b"ccc");
        assert_eq!(
            summary,
            Summary {
                entries: 6,
                bytes: 6
            }
        );
    }

    #[test]
    fn a_single_file_input_keeps_its_own_name() {
        let tmp = TempDir::new();
        let file = tmp.path().join("note.txt");
        fs::write(&file, b"hi").unwrap();

        let items = collect_items(&[file]).unwrap();

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "note.txt");
        assert!(!items[0].is_dir);
    }

    #[test]
    fn missing_input_is_an_io_error() {
        let tmp = TempDir::new();
        let result = collect_items(&[tmp.path().join("nope")]);
        assert!(matches!(result, Err(Error::Io(_))));
    }

    #[test]
    fn cancelling_compression_reports_cancelled() {
        let tmp = TempDir::new();
        let file = tmp.path().join("big.bin");
        fs::write(&file, vec![3u8; 1024 * 1024]).unwrap();
        let items = collect_items(&[file]).unwrap();
        let mut writer = MockWriter::default();
        let mut observer = CancelAfter {
            seen: 0,
            limit: 256 * 1024,
        };

        let result = compress_items(&mut writer, &items, &mut observer);

        assert!(matches!(result, Err(Error::Cancelled)));
    }

    #[test]
    fn compress_to_file_with_unsupported_format_leaves_nothing() {
        let tmp = TempDir::new();
        let input = tmp.path().join("a.txt");
        fs::write(&input, b"a").unwrap();
        let output = tmp.path().join("out.rar");

        let result = compress_to_file(Format::Rar, &output, &[input], false, &mut NoObserver);

        assert!(matches!(result, Err(Error::Unsupported(Format::Rar))));
        assert!(!output.exists());
        assert!(!tmp.path().join("out.rar.part").exists());
    }

    #[test]
    fn compress_to_file_refuses_to_replace_unless_asked() {
        let tmp = TempDir::new();
        let input = tmp.path().join("a.txt");
        fs::write(&input, b"a").unwrap();
        let output = tmp.path().join("out.zip");
        fs::write(&output, b"existing").unwrap();

        let result = compress_to_file(
            Format::Zip,
            &output,
            std::slice::from_ref(&input),
            false,
            &mut NoObserver,
        );

        assert!(matches!(result, Err(Error::AlreadyExists(_))));
        assert_eq!(fs::read(&output).unwrap(), b"existing");
    }
}
