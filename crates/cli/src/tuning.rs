//! `--tune`: regulate a running operation from what it measures (ADR 0012, step 4), and
//! remember per volume where the last run ended up.
//!
//! The remembered knobs are only where the next run starts exploring from: volume keys are
//! built from device names that the OS may reuse, so a wrong hit costs a few ticks, never a
//! wrong result.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use zstar_core::{DriveKind, Knobs, Log, Mode, Options, Profile, Running, TuneBounds, Tuner};

/// Volumes kept in the memory file, newest first.
const KEEP: usize = 64;
const MIN_CHUNK: usize = 64 << 10;
const MAX_CHUNK: usize = 4 << 20;
/// With less RAM than this the biggest request stays at 1 MiB.
const SMALL_RAM: u64 = 1 << 30;

/// Ceilings from the profile settings, the drive and the memory budget.
pub fn bounds(opts: &Options, ram_total: Option<u64>) -> TuneBounds {
    let max_chunk = match ram_total {
        Some(total) if total < SMALL_RAM => 1 << 20,
        _ => MAX_CHUNK,
    };
    TuneBounds {
        max_workers: opts.effective_threads(),
        // A spinning disk gains nothing from many requests; unknown counts as spinning.
        max_io_depth: if opts.drive == DriveKind::Ssd { 32 } else { 4 },
        min_chunk: MIN_CHUNK,
        max_chunk,
    }
}

/// A gentle profile is a background operation; the rest wait for the result.
pub fn mode(profile: Profile) -> Mode {
    if profile == Profile::Gentle {
        Mode::BackgroundActive
    } else {
        Mode::Foreground
    }
}

/// One line per volume involved, sorted so the order of the arguments does not matter.
pub fn volume_key<'a>(paths: impl IntoIterator<Item = &'a Path>) -> String {
    let mut parts: Vec<String> = paths
        .into_iter()
        .map(|p| {
            let d = zstar_sys::detect(p);
            format!(
                "{}:{}:{}",
                d.kind.name(),
                d.bus.as_deref().unwrap_or("-"),
                d.chain.first().map_or("-", String::as_str)
            )
        })
        .collect();
    parts.sort();
    parts.dedup();
    parts
        .join("+")
        .chars()
        .map(|c| if c.is_control() { '_' } else { c })
        .collect()
}

/// Where the memory file lives: `$XDG_STATE_HOME/zstar` (or `~/.local/state/zstar`) on Unix,
/// `%LOCALAPPDATA%\zstar` on Windows.
fn state_file() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
    };
    base.map(|b| b.join("zstar").join("volumes.tsv"))
}

fn parse(text: &str) -> Vec<(String, Knobs)> {
    text.lines()
        .filter_map(|line| {
            let (key, rest) = line.split_once('\t')?;
            let mut n = rest.split_whitespace().map(|f| f.parse::<usize>().ok());
            let (workers, io_depth, chunk) = (n.next()??, n.next()??, n.next()??);
            (n.next().is_none() && workers > 0 && io_depth > 0 && chunk > 0).then(|| {
                (
                    key.to_owned(),
                    Knobs {
                        workers,
                        io_depth,
                        chunk,
                    },
                )
            })
        })
        .collect()
}

fn render(entries: &[(String, Knobs)]) -> String {
    entries
        .iter()
        .map(|(k, v)| format!("{k}\t{} {} {}\n", v.workers, v.io_depth, v.chunk))
        .collect()
}

/// `key` first, the others after it, at most [`KEEP`] in all.
fn remember(mut entries: Vec<(String, Knobs)>, key: &str, knobs: Knobs) -> Vec<(String, Knobs)> {
    entries.retain(|(k, _)| k != key);
    entries.insert(0, (key.to_owned(), knobs));
    entries.truncate(KEEP);
    entries
}

fn load(file: &Path) -> Vec<(String, Knobs)> {
    fs::read_to_string(file)
        .map(|t| parse(&t))
        .unwrap_or_default()
}

/// Written next to the file and renamed over it, so a crash never leaves half a file.
fn store(file: &Path, entries: &[(String, Knobs)]) -> io::Result<()> {
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = file.with_extension("tmp");
    fs::write(&tmp, render(entries))?;
    fs::rename(&tmp, file).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// A running governor and where to save what it learned.
pub struct Tuning {
    running: Running,
    key: String,
    file: Option<PathBuf>,
}

/// Starts regulating `opts`' operation over `paths`. `None` when it cannot be done (no stats
/// to read, no thread): the operation then simply runs untuned.
pub fn begin<'a>(opts: &Options, paths: impl IntoIterator<Item = &'a Path>) -> Option<Tuning> {
    let bounds = bounds(opts, zstar_sys::ram().map(|r| r.total));
    let mode = mode(opts.profile);
    let key = volume_key(paths);
    let file = state_file();
    let remembered = file
        .as_deref()
        .and_then(|f| load(f).into_iter().find(|(k, _)| *k == key))
        .map(|(_, v)| v);
    let tuner = match remembered {
        Some(start) => Tuner::with_start(mode, bounds, start),
        None => Tuner::new(mode, bounds, None),
    };
    let signals = Box::new(|| zstar_core::Signals {
        pressure: zstar_sys::memory_pressure(),
        user_input: false,
        user_idle: zstar_sys::user_idle(),
        on_battery: zstar_sys::on_battery(),
        process_user_cpu: zstar_sys::process_user_cpu(),
    });
    let running = zstar_core::start_governor(opts, tuner, Some(signals))
        .ok()
        .flatten()?;
    Some(Tuning { running, key, file })
}

impl Tuning {
    /// Stops the governor; after a successful operation keeps the best knobs it found.
    /// Returns what the governor changed, for `--stats`.
    pub fn finish(self, succeeded: bool) -> Option<Log> {
        let Tuning { running, key, file } = self;
        let (tuner, log) = running.stop_with_log()?;
        if let (true, Some(best), Some(file)) = (succeeded, tuner.best(), file) {
            // Not being able to save only costs the head start of the next run.
            let _ = store(&file, &remember(load(&file), &key, best));
        }
        Some(log)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(workers: usize, io_depth: usize, chunk: usize) -> Knobs {
        Knobs {
            workers,
            io_depth,
            chunk,
        }
    }

    #[test]
    fn memory_round_trips_and_skips_bad_lines() {
        let entries = vec![("ssd:nvme:nvme0n1".to_owned(), k(8, 32, 1 << 20))];
        assert_eq!(parse(&render(&entries)), entries);
        let text = "a\t1 2 3\nno tab\nb\t1 2\nc\t1 2 3 4\nd\t0 1 1\ne\tx 1 1\nf\t4 4 4096\n";
        let got = parse(text);
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].0, "f");
    }

    #[test]
    fn remember_moves_to_front_replaces_and_caps() {
        let mut e = Vec::new();
        for i in 0..(KEEP + 5) {
            e = remember(e, &format!("v{i}"), k(1, 1, 4096));
        }
        assert_eq!(e.len(), KEEP);
        assert_eq!(e[0].0, format!("v{}", KEEP + 4));
        let e = remember(e, "v10", k(2, 2, 8192));
        assert_eq!(e[0], ("v10".to_owned(), k(2, 2, 8192)));
        assert_eq!(e.iter().filter(|(key, _)| key == "v10").count(), 1);
        assert_eq!(e.len(), KEEP);
    }

    #[test]
    fn store_replaces_atomically_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sub/volumes.tsv");
        store(&file, &[("a".into(), k(1, 1, 4096))]).unwrap();
        store(&file, &[("b".into(), k(2, 2, 8192))]).unwrap();
        assert_eq!(load(&file), vec![("b".to_owned(), k(2, 2, 8192))]);
        assert!(!file.with_extension("tmp").exists());
        assert!(load(&dir.path().join("missing")).is_empty());
    }

    #[test]
    fn bounds_follow_drive_and_memory_and_mode_follows_profile() {
        let mut opts = Options {
            drive: DriveKind::Hdd,
            ..Options::default()
        };
        assert_eq!(bounds(&opts, None).max_io_depth, 4);
        opts.drive = DriveKind::Ssd;
        let b = bounds(&opts, Some(16 << 30));
        assert_eq!((b.max_io_depth, b.max_chunk), (32, MAX_CHUNK));
        assert_eq!(bounds(&opts, Some(512 << 20)).max_chunk, 1 << 20);
        assert_eq!(mode(Profile::Gentle), Mode::BackgroundActive);
        assert_eq!(mode(Profile::Auto), Mode::Foreground);
    }

    #[test]
    fn key_is_stable_ordered_and_one_line() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path();
        let b = Path::new(".");
        let key = volume_key([a, b]);
        assert_eq!(key, volume_key([b, a]));
        assert!(!key.contains(['\n', '\t']));
        assert!(!key.is_empty());
    }
}
