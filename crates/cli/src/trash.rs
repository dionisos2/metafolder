//! The repository trash-bin (spec-trash.org) — CLI surface.
//!
//! The filesystem layer (`TrashDir` and friends) lives in
//! [`metafolder_core::trash`], shared with the GUI. This module re-exports it
//! and keeps the CLI-only argument parsing (`parse_size` / `parse_duration`,
//! which report `CliError::Usage`).

use crate::client::CliError;

// The shared filesystem core: locating, moving, listing, restoring and pruning
// the trash blobs. Re-exported so `crate::trash::TrashDir` etc. keep resolving.
pub use metafolder_core::trash::{PruneMode, Reason, TrashDir, TrashEntry, TrashError, TrashedNode};

impl From<TrashError> for CliError {
    fn from(e: TrashError) -> Self {
        CliError::Op(e.0)
    }
}

/// Parses a human size into bytes: a bare integer (bytes) or a number with a
/// `k`/`m`/`g`/`t` suffix (base 1024, case-insensitive; an optional trailing
/// `b` is allowed — `100mb`, `1g`, `512k`, `2048`).
pub fn parse_size(s: &str) -> Result<u64, CliError> {
    let mut t = s.trim().to_ascii_lowercase();
    if t.is_empty() {
        return Err(CliError::Usage("empty size".into()));
    }
    // A trailing `b` is decorative (`100mb`, `1024b`): drop it, leaving either
    // a unit letter or bare digits.
    if t.ends_with('b') {
        t.pop();
    }
    let mult: u64 = match t.chars().last() {
        Some('k') => 1024,
        Some('m') => 1024u64.pow(2),
        Some('g') => 1024u64.pow(3),
        Some('t') => 1024u64.pow(4),
        _ => 1,
    };
    let digits = if mult == 1 { t.as_str() } else { &t[..t.len() - 1] };
    let n: u64 = digits
        .parse()
        .map_err(|_| CliError::Usage(format!("invalid size '{s}'")))?;
    n.checked_mul(mult)
        .ok_or_else(|| CliError::Usage(format!("size '{s}' overflows")))
}

/// Parses `<n><unit>` into milliseconds; unit is `y` (365 d), `w`, `d`, `h`,
/// `m` (minute), `s` (`1y`, `30d`, `12h`, `45m`).
pub fn parse_duration(s: &str) -> Result<i64, CliError> {
    let t = s.trim().to_ascii_lowercase();
    let unit = t
        .chars()
        .last()
        .filter(|c| c.is_ascii_alphabetic())
        .ok_or_else(|| CliError::Usage(format!("invalid duration '{s}' (expected e.g. 30d)")))?;
    let per: i64 = match unit {
        's' => 1_000,
        'm' => 60_000,
        'h' => 3_600_000,
        'd' => 86_400_000,
        'w' => 7 * 86_400_000,
        'y' => 365 * 86_400_000,
        _ => return Err(CliError::Usage(format!("unknown duration unit '{unit}'"))),
    };
    let n: i64 = t[..t.len() - 1]
        .parse()
        .map_err(|_| CliError::Usage(format!("invalid duration '{s}'")))?;
    n.checked_mul(per)
        .ok_or_else(|| CliError::Usage(format!("duration '{s}' overflows")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_size_units() {
        assert_eq!(parse_size("2048").unwrap(), 2048);
        assert_eq!(parse_size("1024b").unwrap(), 1024);
        assert_eq!(parse_size("512k").unwrap(), 512 * 1024);
        assert_eq!(parse_size("100mb").unwrap(), 100 * 1024 * 1024);
        assert_eq!(parse_size("5M").unwrap(), 5 * 1024 * 1024);
        assert_eq!(parse_size("1g").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_size("2T").unwrap(), 2 * 1024u64.pow(4));
    }

    #[test]
    fn parse_size_rejects_garbage() {
        assert!(parse_size("").is_err());
        assert!(parse_size("abc").is_err());
        assert!(parse_size("10x").is_err());
        assert!(parse_size("mb").is_err());
    }

    #[test]
    fn parse_duration_units() {
        assert_eq!(parse_duration("10s").unwrap(), 10_000);
        assert_eq!(parse_duration("45m").unwrap(), 45 * 60_000);
        assert_eq!(parse_duration("12h").unwrap(), 12 * 3_600_000);
        assert_eq!(parse_duration("30d").unwrap(), 30 * 86_400_000);
        assert_eq!(parse_duration("2w").unwrap(), 14 * 86_400_000);
        assert_eq!(parse_duration("1y").unwrap(), 365 * 86_400_000);
    }

    #[test]
    fn parse_duration_rejects_garbage() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("10").is_err());
        assert!(parse_duration("d").is_err());
        assert!(parse_duration("1x").is_err());
    }
}
