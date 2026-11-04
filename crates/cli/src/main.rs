//! `zstar`: command-line front end for zstar-core.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use lexopt::prelude::*;
use zstar_core::{
    DriveKind, EntryKind, Error, Format, LimitKind, Options, Password, Priority, Profile, Stats,
    StatsSnapshot, ZipMethod,
};

/// Release builds set `ZSTAR_VERSION` (see `.github/workflows/release.yml`) so the binary
/// reports the version of the release it was published in.
const VERSION: &str = match option_env!("ZSTAR_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

const USAGE: &str = "\
zstar - archive tool

USAGE:
    zstar compress [OPTIONS] <ARCHIVE> <INPUT>...
    zstar extract  [OPTIONS] <ARCHIVE>
    zstar list     [OPTIONS] <ARCHIVE>

COMMON OPTIONS:
        --config <FILE>    TOML settings (default: the user config file, if it exists)
    -j, --threads <N>      worker threads (default: from the profile)
        --profile <P>      fast, balanced, gentle or auto (default): how many CPUs to use
                           and at what priority. Auto picks from the drive kind.
        --drive <KIND>     hdd or ssd: the kind of drive the files are on. Without it the
                           OS is asked about every path involved; if it cannot tell (USB,
                           network, RAID, ...) the safe HDD rules apply (one file at a
                           time, 1 MiB buffers).
    -f, --force            overwrite existing files
        --password-file <FILE>  read the zip/7z password from the first line of FILE
                           (- = stdin). Without it the ZSTAR_PASSWORD variable is used.
                           There is no --password flag: it would show up in `ps`.
        --stats            after compress or extract, print a `key: value` block of timings
                           (reading, writing, waiting, MiB/s) to stderr. Off by default.
    -h, --help             show this help
    -V, --version          show the version

compress:
    -l, --level <N>        compression level (zip/7z/gz/xz: 0-9, bz2: 1-9, zst: 1-22,
                           br: 0-11; lz4 has none)
        --format <NAME>    zip, 7z, tar, tar.gz, tar.bz2, tar.xz, tar.zst, tar.lz4, or one of
                           gz, bz2, xz, zst, lz4, br (default: from the file name).
                           The single-file formats compress exactly one file.
        --zip-method <M>   zip entries: deflate (default), store, bzip2 or zstd (older
                           unzip tools cannot read zstd). Zip also extracts LZMA and
                           Deflate64, which Zstar cannot write.

extract:
    -o, --output <DIR>     destination directory (default: current directory)
        --max-size <SIZE>  stop after this much output, e.g. 10G (default 64G)
        --max-entries <N>  stop after this many entries (default 10000000)
        --max-ratio <N>    stop above this output:archive ratio, 0 = off (default 500)
        --max-memory <SIZE>  refuse decoders asking for more memory (xz/7z dictionary),
                           e.g. 512M (default 1G)
        --no-limits        disable all bomb protection (trusted archives only)

The archive format is detected from its contents, never from its name, except for
Brotli (.br), which has no signature and is recognised by its file name.";

enum Cmd {
    Compress {
        archive: PathBuf,
        inputs: Vec<PathBuf>,
        format: Option<Format>,
    },
    Extract {
        archive: PathBuf,
        output: PathBuf,
    },
    List {
        archive: PathBuf,
    },
    /// Hidden: shows how the drive holding a path was classified.
    DriveInfo {
        path: PathBuf,
    },
}

struct Cli {
    cmd: Cmd,
    opts: Options,
    /// `--drive` was given, so the OS is not asked.
    drive_given: bool,
    /// `--stats`: print the timing block after the operation.
    stats: bool,
}

fn parse_size(s: &str) -> Result<u64, String> {
    zstar_core::config::parse_size(s).ok_or_else(|| format!("invalid size '{s}'"))
}

/// The default config file: `$XDG_CONFIG_HOME/zstar/config.toml` (or `~/.config/...`) on
/// Unix, `%APPDATA%\\zstar\\config.toml` on Windows. It is optional.
fn default_config_path() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    };
    base.map(|b| b.join("zstar").join("config.toml"))
}

/// An explicit `--config` file must exist; the default one is used only when present.
fn load_config(explicit: Option<&std::path::Path>, opts: &mut Options) -> Result<(), String> {
    let (path, required) = match explicit {
        Some(p) => (p.to_path_buf(), true),
        None => match default_config_path() {
            Some(p) => (p, false),
            None => return Ok(()),
        },
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => zstar_core::config::apply_toml(&text, opts)
            .map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if !required && e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Option<Cli>, String> {
    let mut opts = Options::default();
    let mut command: Option<String> = None;
    let mut positional: Vec<PathBuf> = Vec::new();
    let mut output = PathBuf::from(".");
    let mut format = None;
    let mut config: Option<PathBuf> = None;
    let mut drive_given = false;
    let mut stats = false;
    let e = |e: lexopt::Error| e.to_string();
    let args: Vec<OsString> = args.into_iter().collect();
    // Flags override the file, so the file is read first: find --config before parsing.
    for (i, a) in args.iter().enumerate() {
        if a == "--config" {
            config = args.get(i + 1).map(PathBuf::from);
        }
    }
    load_config(config.as_deref(), &mut opts)?;
    opts.password = std::env::var("ZSTAR_PASSWORD")
        .ok()
        .filter(|v| !v.is_empty())
        .map(|v| Password::new(&v));
    let mut p = lexopt::Parser::from_args(args);

    while let Some(arg) = p.next().map_err(e)? {
        match arg {
            Short('h') | Long("help") => {
                println!("{USAGE}");
                return Ok(None);
            }
            Short('V') | Long("version") => {
                println!("zstar {VERSION}");
                return Ok(None);
            }
            Short('j') | Long("threads") => {
                opts.threads = p.value().map_err(e)?.parse().map_err(e)?
            }
            Long("config") => {
                p.value().map_err(e)?;
            }
            Long("profile") => {
                let v: String = p.value().map_err(e)?.parse().map_err(e)?;
                opts.profile = Profile::from_name(&v).ok_or(format!(
                    "unknown profile '{v}' (fast, balanced, gentle, auto)"
                ))?;
            }
            Long("drive") => {
                let v: String = p.value().map_err(e)?.parse().map_err(e)?;
                opts.drive = DriveKind::from_name(&v)
                    .filter(|d| *d != DriveKind::Unknown)
                    .ok_or(format!("unknown drive '{v}' (hdd, ssd)"))?;
                drive_given = true;
            }
            Long("stats") => stats = true,
            Short('f') | Long("force") => opts.overwrite = true,
            Short('l') | Long("level") => {
                opts.level = Some(p.value().map_err(e)?.parse().map_err(e)?)
            }
            Long("zip-method") => {
                let v: String = p.value().map_err(e)?.parse().map_err(e)?;
                opts.zip_method = Some(ZipMethod::from_name(&v).ok_or(format!(
                    "unknown zip method '{v}' (deflate, store, bzip2, zstd)"
                ))?);
            }
            Long("format") => {
                let v: String = p.value().map_err(e)?.parse().map_err(e)?;
                format = Some(Format::from_name(&v).ok_or(format!("unknown format '{v}'"))?);
            }
            Short('o') | Long("output") => output = p.value().map_err(e)?.into(),
            Long("max-size") => {
                let v: String = p.value().map_err(e)?.parse().map_err(e)?;
                opts.limits.max_total_bytes = parse_size(&v)?;
            }
            Long("max-entries") => {
                opts.limits.max_entries = p.value().map_err(e)?.parse().map_err(e)?
            }
            Long("max-ratio") => {
                opts.limits.max_ratio = p.value().map_err(e)?.parse().map_err(e)?
            }
            Long("max-memory") => {
                let v: String = p.value().map_err(e)?.parse().map_err(e)?;
                opts.limits.max_memory_bytes = parse_size(&v)?;
            }
            Long("password-file") => {
                let path: PathBuf = p.value().map_err(e)?.into();
                opts.password = Some(read_password_file(&path)?);
            }
            Long("no-limits") => opts.limits = zstar_core::Limits::unlimited(),
            Value(v) if command.is_none() => command = Some(v.string().map_err(e)?),
            Value(v) => positional.push(v.into()),
            other => return Err(other.unexpected().to_string()),
        }
    }

    let need = |n: usize, what: &str| {
        if positional.len() < n {
            Err(format!("missing argument: {what}"))
        } else {
            Ok(())
        }
    };
    let cmd = match command.as_deref() {
        Some("compress" | "c") => {
            need(2, "<ARCHIVE> <INPUT>...")?;
            let archive = positional.remove(0);
            Cmd::Compress {
                archive,
                inputs: positional,
                format,
            }
        }
        Some("extract" | "x") => {
            need(1, "<ARCHIVE>")?;
            if positional.len() > 1 {
                return Err("extract takes one archive".into());
            }
            Cmd::Extract {
                archive: positional.remove(0),
                output,
            }
        }
        Some("list" | "l") => {
            need(1, "<ARCHIVE>")?;
            if positional.len() > 1 {
                return Err("list takes one archive".into());
            }
            Cmd::List {
                archive: positional.remove(0),
            }
        }
        Some("drive-info") => {
            need(1, "<PATH>")?;
            if positional.len() > 1 {
                return Err("drive-info takes one path".into());
            }
            Cmd::DriveInfo {
                path: positional.remove(0),
            }
        }
        Some(other) => return Err(format!("unknown command '{other}'")),
        None => return Err("missing command".into()),
    };
    if stats && !matches!(cmd, Cmd::Compress { .. } | Cmd::Extract { .. }) {
        return Err("--stats applies to compress and extract".into());
    }
    Ok(Some(Cli {
        cmd,
        opts,
        drive_given,
        stats,
    }))
}

/// Prints `\r<done>/<total>` to stderr, at most every 100 ms, only when stderr is a terminal.
fn progress_printer(start: Instant) -> zstar_core::ProgressFn {
    let last_ms = AtomicU64::new(0);
    Arc::new(move |p| {
        let now = start.elapsed().as_millis() as u64;
        if now < last_ms.load(Ordering::Relaxed) + 100 {
            return;
        }
        last_ms.store(now, Ordering::Relaxed);
        let mb = |b: u64| b as f64 / (1 << 20) as f64;
        let line = match p.bytes_total {
            Some(t) if t > 0 => format!(
                "{:.1}/{:.1} MiB ({}%)",
                mb(p.bytes_done),
                mb(t),
                p.bytes_done * 100 / t
            ),
            _ => format!("{:.1} MiB", mb(p.bytes_done)),
        };
        let _ = write!(std::io::stderr(), "\r\x1b[K{line}");
    })
}

/// First line of `path` (or of stdin for `-`), without its line ending.
fn read_password_file(path: &Path) -> Result<Password, String> {
    let mut text = String::new();
    if path.as_os_str() == "-" {
        std::io::stdin().read_line(&mut text)
    } else {
        std::fs::File::open(path).and_then(|f| BufReader::new(f).read_line(&mut text))
    }
    .map_err(|err| format!("cannot read the password file {}: {err}", path.display()))?;
    let line = text.trim_end_matches(['\r', '\n']);
    if line.is_empty() {
        return Err(format!("the password file {} is empty", path.display()));
    }
    Ok(Password::new(line))
}

fn hint(e: &Error) -> &'static str {
    match e {
        Error::LimitExceeded(LimitKind::TotalSize) => {
            "raise it with --max-size or `max_total_bytes` under [limits] in the config file, or use --no-limits if you trust the archive"
        }
        Error::LimitExceeded(LimitKind::Entries) => {
            "raise it with --max-entries or `max_entries` under [limits] in the config file, or use --no-limits if you trust the archive"
        }
        Error::LimitExceeded(LimitKind::Memory) => {
            "raise it with --max-memory or `max_memory_bytes` under [limits] in the config file, or use --no-limits if you trust the archive"
        }
        Error::LimitExceeded(LimitKind::Ratio) => {
            "raise it with --max-ratio (0 = off) or `max_ratio` under [limits] in the config file, or use --no-limits if you trust the archive"
        }
        Error::AlreadyExists(_) => "use --force to overwrite",
        Error::PasswordRequired | Error::WrongPassword => {
            "give the password with --password-file <FILE> or the ZSTAR_PASSWORD variable"
        }
        _ => "",
    }
}

fn drive_from_sys(kind: zstar_sys::DriveKind) -> DriveKind {
    match kind {
        zstar_sys::DriveKind::Ssd => DriveKind::Ssd,
        zstar_sys::DriveKind::Hdd => DriveKind::Hdd,
        zstar_sys::DriveKind::Unknown => DriveKind::Unknown,
    }
}

/// The drive rule for one operation: the worst of all the paths it reads or writes.
fn detect_drive<'a>(paths: impl IntoIterator<Item = &'a Path>) -> DriveKind {
    drive_from_sys(zstar_sys::combine(
        paths.into_iter().map(zstar_sys::drive_kind),
    ))
}

/// What `--stats` prints besides the library's counters.
struct Report<'a> {
    operation: &'a str,
    format: Option<&'a str>,
    profile: Profile,
    drive: DriveKind,
    drive_given: bool,
    same_device: Option<bool>,
    wall: Duration,
}

/// `Some(true)` only when every input is on the same device as `output`; `Some(false)` as
/// soon as one is not; `None` when nothing is known to differ but something is not sure.
fn same_device_all<'a>(output: &Path, inputs: impl IntoIterator<Item = &'a Path>) -> Option<bool> {
    let mut all = Some(true);
    for input in inputs {
        match zstar_sys::same_device(input, output) {
            Some(false) => return Some(false),
            Some(true) => {}
            None => all = None,
        }
    }
    all
}

/// The `--stats` block: one `key: value` per line, keys never change, durations in
/// milliseconds with three decimals. `codec_cpu_ms` is `n/a` unless the multi-threaded gzip
/// path ran. See the docs of `zstar_core::StatsSnapshot` for what each number covers.
fn stats_block(r: &Report<'_>, s: &StatsSnapshot) -> String {
    let ms = |d: Duration| format!("{:.3}", d.as_secs_f64() * 1e3);
    let compress = r.operation == "compress";
    // The uncompressed side: what was read from the files, or what was written to them.
    let plain = if compress {
        s.read_bytes
    } else {
        s.write_bytes
    };
    let secs = r.wall.as_secs_f64();
    let rate = if secs > 0.0 {
        plain as f64 / (1 << 20) as f64 / secs
    } else {
        0.0
    };
    let yes_no = match r.same_device {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unknown",
    };
    let codec = if s.codec_cpu.is_zero() {
        "n/a".to_owned()
    } else {
        ms(s.codec_cpu)
    };
    let mut out = String::from("stats_version: 1\n");
    let mut line = |k: &str, v: String| {
        out.push_str(k);
        out.push_str(": ");
        out.push_str(&v);
        out.push('\n');
    };
    line("operation", r.operation.into());
    line("format", r.format.unwrap_or("detected").into());
    line("profile", r.profile.name().into());
    line("drive_kind", r.drive.name().into());
    line(
        "drive_source",
        if r.drive_given { "given" } else { "detected" }.into(),
    );
    line("same_device", yes_no.into());
    line("threads_pool", s.threads_pool.max(1).to_string());
    line("threads_start", s.threads_start.max(1).to_string());
    line("wall_ms", ms(r.wall));
    line("read_ms", ms(s.read));
    line("write_ms", ms(s.write));
    line("codec_cpu_ms", codec);
    line("other_ms", ms(s.other()));
    line("parked_ms", ms(s.parked));
    line("bytes_in", s.read_bytes.to_string());
    line("bytes_out", s.write_bytes.to_string());
    line("throughput_mib_s", format!("{rate:.1}"));
    line("throughput_basis", "uncompressed".into());
    out
}

fn run(mut cli: Cli) -> Result<(), Error> {
    if let Cmd::DriveInfo { path } = &cli.cmd {
        let start = Instant::now();
        let d = zstar_sys::detect(path);
        let took = start.elapsed();
        println!("path: {}", path.display());
        println!("{d}");
        println!("time: {:.3} ms (cold)", took.as_secs_f64() * 1e3);
        return Ok(());
    }
    if !cli.drive_given {
        cli.opts.drive = match &cli.cmd {
            Cmd::Compress {
                archive, inputs, ..
            } => detect_drive(
                inputs
                    .iter()
                    .map(PathBuf::as_path)
                    .chain([archive.as_path()]),
            ),
            Cmd::Extract { archive, output } => detect_drive([archive.as_path(), output.as_path()]),
            Cmd::List { .. } | Cmd::DriveInfo { .. } => cli.opts.drive,
        };
    }
    cli.opts.worker_hook = Some(Arc::new(|p| {
        let p = match p {
            Priority::Normal => zstar_sys::Priority::Normal,
            Priority::Low => zstar_sys::Priority::Low,
        };
        // Failing only means the thread keeps its priority.
        let _ = zstar_sys::set_current_thread_priority(p);
    }));
    // A hint that cannot be given only means a plain open.
    cli.opts.open_hook = Some(Arc::new(zstar_sys::open_sequential));
    if cli.stats {
        cli.opts.stats = Some(Stats::new());
    }
    let ctrlc_token = cli.opts.control.clone();
    // Failing to install the handler only costs graceful Ctrl-C, so it is not an error.
    let _ = ctrlc::set_handler(move || ctrlc_token.cancel());
    let start = Instant::now();
    let show_progress = std::io::stderr().is_terminal();
    if show_progress {
        cli.opts.progress = Some(progress_printer(start));
    }
    let clear = || {
        if show_progress {
            let _ = write!(std::io::stderr(), "\r\x1b[K");
        }
    };

    match cli.cmd {
        Cmd::Compress {
            archive,
            inputs,
            format,
        } => {
            let format = format
                .or_else(|| Format::from_path(&archive))
                .ok_or_else(|| {
                    Error::InvalidOption(
                        "cannot tell the format from the file name; use --format".into(),
                    )
                })?;
            let same = cli
                .stats
                .then(|| same_device_all(&archive, inputs.iter().map(PathBuf::as_path)));
            let began = Instant::now();
            let r = zstar_core::create_file(format, &inputs, &archive, &cli.opts);
            let wall = began.elapsed();
            clear();
            r?;
            if let (Some(same_device), Some(snap)) = (same, cli.opts.stats_snapshot()) {
                let report = Report {
                    operation: "compress",
                    format: Some(format.name()),
                    profile: cli.opts.profile,
                    drive: cli.opts.drive,
                    drive_given: cli.drive_given,
                    same_device,
                    wall,
                };
                eprint!("{}", stats_block(&report, &snap));
            }
            eprintln!(
                "created {} ({}) in {:.2?}",
                archive.display(),
                format.name(),
                start.elapsed()
            );
        }
        Cmd::Extract { archive, output } => {
            let same = cli
                .stats
                .then(|| same_device_all(&output, [archive.as_path()]));
            let began = Instant::now();
            let r = zstar_core::extract_file(&archive, &output, &cli.opts);
            let wall = began.elapsed();
            clear();
            let s = r?;
            if let (Some(same_device), Some(snap)) = (same, cli.opts.stats_snapshot()) {
                let report = Report {
                    operation: "extract",
                    format: None,
                    profile: cli.opts.profile,
                    drive: cli.opts.drive,
                    drive_given: cli.drive_given,
                    same_device,
                    wall,
                };
                eprint!("{}", stats_block(&report, &snap));
            }
            eprintln!(
                "extracted {} files, {} dirs, {} symlinks, {:.1} MiB in {:.2?}",
                s.files,
                s.dirs,
                s.symlinks,
                s.bytes as f64 / (1 << 20) as f64,
                start.elapsed()
            );
            if s.skipped > 0 {
                eprintln!(
                    "warning: skipped {} unsupported entries (devices, FIFOs, or symlinks on this platform)",
                    s.skipped
                );
            }
        }
        Cmd::List { archive } => {
            let mut out = std::io::BufWriter::new(std::io::stdout().lock());
            zstar_core::list_file(&archive, &cli.opts, &mut |e| {
                let kind = match e.kind {
                    EntryKind::Dir => 'd',
                    EntryKind::Symlink => 'l',
                    EntryKind::Hardlink => 'h',
                    EntryKind::Other => '?',
                    EntryKind::File => '-',
                };
                let target = e
                    .link_target
                    .map(|t| format!(" -> {t}"))
                    .unwrap_or_default();
                // A closed pipe (e.g. `| head`) is not worth an error message.
                let _ = writeln!(out, "{kind} {:>12} {}{target}", e.size, e.name);
            })?;
            let _ = out.flush();
        }
        Cmd::DriveInfo { .. } => {}
    }
    Ok(())
}

fn main() -> ExitCode {
    let cli = match parse(std::env::args_os().skip(1)) {
        Ok(Some(cli)) => cli,
        Ok(None) => return ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("zstar: {msg}\nTry 'zstar --help'.");
            return ExitCode::from(2);
        }
    };
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Error::Cancelled) => {
            eprintln!("zstar: cancelled");
            ExitCode::from(130)
        }
        Err(e) => {
            eprintln!("zstar: error: {e}");
            let h = hint(&e);
            if !h.is_empty() {
                eprintln!("zstar: {h}");
            }
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<OsString> {
        s.split_whitespace().map(OsString::from).collect()
    }

    #[test]
    fn sizes_accept_binary_suffixes() {
        assert_eq!(parse_size("5").unwrap(), 5);
        assert_eq!(parse_size("2K").unwrap(), 2048);
        assert_eq!(parse_size("10g").unwrap(), 10 << 30);
        assert!(parse_size("abc").is_err());
        assert!(parse_size("99999999999T").is_err());
    }

    #[test]
    fn parses_each_command() {
        let c = parse(args("compress -l 3 -j 2 out.zip a b"))
            .unwrap()
            .unwrap();
        assert_eq!((c.opts.level, c.opts.threads), (Some(3), 2));
        assert!(matches!(c.cmd, Cmd::Compress { ref inputs, .. } if inputs.len() == 2));
        assert_eq!(
            (c.opts.profile, c.opts.drive),
            (Profile::Auto, DriveKind::Unknown)
        );

        let c = parse(args("extract --profile gentle --drive hdd a.zip"))
            .unwrap()
            .unwrap();
        assert_eq!(
            (c.opts.profile, c.opts.drive),
            (Profile::Gentle, DriveKind::Hdd)
        );

        let c = parse(args("extract -o dest --max-size 1M --max-ratio 0 a.zip"))
            .unwrap()
            .unwrap();
        assert_eq!(
            (c.opts.limits.max_total_bytes, c.opts.limits.max_ratio),
            (1 << 20, 0)
        );
        assert!(
            matches!(c.cmd, Cmd::Extract { ref output, .. } if output == &PathBuf::from("dest"))
        );

        assert!(matches!(
            parse(args("list a.zip")).unwrap().unwrap().cmd,
            Cmd::List { .. }
        ));
        assert!(matches!(
            parse(args("drive-info /tmp")).unwrap().unwrap().cmd,
            Cmd::DriveInfo { .. }
        ));
    }

    #[test]
    fn drive_flag_is_remembered() {
        assert!(!parse(args("extract a.zip")).unwrap().unwrap().drive_given);
        assert!(
            parse(args("extract --drive ssd a.zip"))
                .unwrap()
                .unwrap()
                .drive_given
        );
    }

    #[test]
    fn config_file_sets_limits_and_flags_override_it() {
        let dir = std::env::temp_dir().join(format!("zstar-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("c.toml");
        std::fs::write(&file, "[limits]\nmax_total_bytes = \"1M\"\nmax_ratio = 7\n").unwrap();
        let cfg = file.display();

        let c = parse(args(&format!("extract --config {cfg} a.zip")))
            .unwrap()
            .unwrap();
        assert_eq!(c.opts.limits.max_total_bytes, 1 << 20);
        assert_eq!(c.opts.limits.max_ratio, 7);

        let c = parse(args(&format!("extract --config {cfg} --max-size 2M a.zip")))
            .unwrap()
            .unwrap();
        assert_eq!(c.opts.limits.max_total_bytes, 2 << 20);
        assert!(parse(args("extract --config /no/such/file.toml a.zip")).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_bad_input() {
        for bad in [
            "",
            "frobnicate x",
            "compress only-one",
            "extract",
            "extract --bogus a",
            "list a b",
            "drive-info",
            "drive-info a b",
            "compress --format rar a b",
            "compress --profile turbo a b",
            "compress --drive nvme a b",
            "compress --drive unknown a b",
        ] {
            assert!(parse(args(bad)).is_err(), "{bad:?} should fail");
        }
    }
}
