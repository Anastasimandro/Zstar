//! The TOML configuration file: the settings that outlive one command line.
//!
//! Only the subset Zstar needs is parsed (comments, one `[limits]` table, integers with
//! optional `_` separators, and size strings such as `"64G"`), so no TOML dependency is
//! pulled in. Unknown keys are errors: a typo in a safety limit must not pass silently.
//!
//! ```toml
//! threads = 4              # 0 = one per CPU
//!
//! [limits]
//! max_total_bytes  = "64G"   # total uncompressed bytes
//! max_entries      = 10_000_000
//! max_ratio        = 500     # output:archive ratio, 0 = off
//! ratio_min_bytes  = "256M"  # the ratio is only checked past this much output
//! max_memory_bytes = "1G"    # decoder dictionary size
//! ```

use crate::error::{Error, Result};
use crate::options::Options;

/// Parses a size such as `1024`, `64K`, `10M`, `64G`, `2T` (binary multiples).
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let (digits, shift) = match s.chars().last()? {
        'k' | 'K' => (&s[..s.len() - 1], 10),
        'm' | 'M' => (&s[..s.len() - 1], 20),
        'g' | 'G' => (&s[..s.len() - 1], 30),
        't' | 'T' => (&s[..s.len() - 1], 40),
        _ => (s, 0),
    };
    digits
        .trim()
        .replace('_', "")
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(1 << shift))
}

fn bad(line: usize, msg: impl std::fmt::Display) -> Error {
    Error::InvalidOption(format!("config line {line}: {msg}"))
}

/// Applies the settings in `text` to `opts`. Command-line flags should be applied after it.
pub fn apply_toml(text: &str, opts: &mut Options) -> Result<()> {
    let mut table = String::new();
    for (i, raw) in text.lines().enumerate() {
        let n = i + 1;
        // A `#` inside a quoted size cannot occur, so a plain split is enough.
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            table = name.trim().to_owned();
            if table != "limits" {
                return Err(bad(n, format!("unknown table [{table}]")));
            }
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| bad(n, "expected `key = value`"))?;
        let (key, value) = (key.trim(), value.trim().trim_matches('"'));
        let number = || {
            parse_size(value).ok_or_else(|| bad(n, format!("`{key}`: invalid number {value:?}")))
        };
        let limits = &mut opts.limits;
        match (table.as_str(), key) {
            ("", "threads") => opts.threads = number()? as usize,
            ("limits", "max_total_bytes") => limits.max_total_bytes = number()?,
            ("limits", "max_entries") => limits.max_entries = number()?,
            ("limits", "max_ratio") => limits.max_ratio = number()?,
            ("limits", "ratio_min_bytes") => limits.ratio_min_bytes = number()?,
            ("limits", "max_memory_bytes") => limits.max_memory_bytes = number()?,
            _ => return Err(bad(n, format!("unknown key `{key}`"))),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("5"), Some(5));
        assert_eq!(parse_size("64G"), Some(64 << 30));
        assert_eq!(parse_size("1_000"), Some(1000));
        assert_eq!(parse_size("99999999999T"), None);
        assert_eq!(parse_size("x"), None);
    }

    #[test]
    fn applies_every_key_and_rejects_typos() {
        let mut o = Options::default();
        apply_toml(
            "threads = 3 # cpus\n[limits]\nmax_total_bytes = \"2G\"\nmax_entries = 1_000\n\
             max_ratio = 0\nratio_min_bytes = \"1M\"\nmax_memory_bytes = \"512M\"\n",
            &mut o,
        )
        .unwrap();
        assert_eq!(o.threads, 3);
        assert_eq!(o.limits.max_total_bytes, 2 << 30);
        assert_eq!(o.limits.max_entries, 1000);
        assert_eq!(o.limits.max_ratio, 0);
        assert_eq!(o.limits.ratio_min_bytes, 1 << 20);
        assert_eq!(o.limits.max_memory_bytes, 512 << 20);

        for bad in [
            "[limitz]\n",
            "[limits]\nmax_total = 1\n",
            "[limits]\nmax_ratio = abc\n",
            "oops\n",
        ] {
            assert!(apply_toml(bad, &mut Options::default()).is_err(), "{bad:?}");
        }
    }
}
