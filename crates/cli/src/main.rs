//! `zstar`: command-line front end for zstar-core.

use std::ffi::OsString;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use lexopt::prelude::*;
use zstar_core::{EntryKind, Error, Format, LimitKind, Options};

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
    -j, --threads <N>      worker threads (default: one per CPU)
    -f, --force            overwrite existing files
    -h, --help             show this help
    -V, --version          show the version

compress:
    -l, --level <N>        compression level (zip/gz: 0-9, zst: 1-22)
        --format <NAME>    zip, tar, tar.gz or tar.zst (default: from the file name)

extract:
    -o, --output <DIR>     destination directory (default: current directory)
        --max-size <SIZE>  stop after this much output, e.g. 10G (default 64G)
        --max-entries <N>  stop after this many entries (default 10000000)
        --max-ratio <N>    stop above this output:archive ratio, 0 = off (default 500)
        --no-limits        disable all bomb protection (trusted archives only)

The archive format is detected from its contents, never from its name.";

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
}

struct Cli {
    cmd: Cmd,
    opts: Options,
}

fn parse_size(s: &str) -> Result<u64, String> {
    let (digits, shift) = match s.chars().last() {
        Some('k' | 'K') => (&s[..s.len() - 1], 10),
        Some('m' | 'M') => (&s[..s.len() - 1], 20),
        Some('g' | 'G') => (&s[..s.len() - 1], 30),
        Some('t' | 'T') => (&s[..s.len() - 1], 40),
        _ => (s, 0),
    };
    digits
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(1 << shift))
        .ok_or_else(|| format!("invalid size '{s}'"))
}

fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Option<Cli>, String> {
    let mut p = lexopt::Parser::from_args(args);
    let mut opts = Options::default();
    let mut command: Option<String> = None;
    let mut positional: Vec<PathBuf> = Vec::new();
    let mut output = PathBuf::from(".");
    let mut format = None;
    let e = |e: lexopt::Error| e.to_string();

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
            Short('f') | Long("force") => opts.overwrite = true,
            Short('l') | Long("level") => {
                opts.level = Some(p.value().map_err(e)?.parse().map_err(e)?)
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
        Some(other) => return Err(format!("unknown command '{other}'")),
        None => return Err("missing command".into()),
    };
    Ok(Some(Cli { cmd, opts }))
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

fn hint(e: &Error) -> &'static str {
    match e {
        Error::LimitExceeded(LimitKind::TotalSize) => {
            "raise it with --max-size, or use --no-limits if you trust the archive"
        }
        Error::LimitExceeded(LimitKind::Entries) => {
            "raise it with --max-entries, or use --no-limits if you trust the archive"
        }
        Error::LimitExceeded(LimitKind::Ratio) => {
            "raise it with --max-ratio (0 = off), or use --no-limits if you trust the archive"
        }
        Error::AlreadyExists(_) => "use --force to overwrite",
        _ => "",
    }
}

fn run(mut cli: Cli) -> Result<(), Error> {
    let ctrlc_token = cli.opts.cancel.clone();
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
            let r = zstar_core::create_file(format, &inputs, &archive, &cli.opts);
            clear();
            r?;
            eprintln!(
                "created {} ({}) in {:.2?}",
                archive.display(),
                format.name(),
                start.elapsed()
            );
        }
        Cmd::Extract { archive, output } => {
            let r = zstar_core::extract_file(&archive, &output, &cli.opts);
            clear();
            let s = r?;
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
            "compress --format rar a b",
        ] {
            assert!(parse(args(bad)).is_err(), "{bad:?} should fail");
        }
    }
}
