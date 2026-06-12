//! Stat-derived `mfr_*` field values (spec-data-model "Reserved fields",
//! spec-platform "File metadata fields").

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use metafolder_core::metarecord::{Field, Value};

/// The stat-derived fields of a file or directory: `mfr_type`, `mfr_size`,
/// `mfr_mtime`, and on Unix `mfr_permissions`, `mfr_uid`, `mfr_gid`.
/// (`mfr_btime` and `mfr_mime` are v2.)
pub fn stat_fields(path: &Path) -> Result<Vec<Field>> {
    let meta = std::fs::metadata(path).with_context(|| format!("Failed to stat {path:?}"))?;
    let kind = if meta.is_dir() { "dir" } else { "file" };
    let mut fields = vec![
        Field::new("mfr_type", Value::String(kind.to_string())),
        Field::new("mfr_size", Value::Int(meta.len() as i64)),
    ];
    if let Ok(mtime) = meta.modified() {
        fields.push(Field::new("mfr_mtime", Value::DateTime(iso8601(mtime))));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        fields.push(Field::new(
            "mfr_permissions",
            Value::String(format!("{:04o}", meta.mode() & 0o7777)),
        ));
        fields.push(Field::new("mfr_uid", Value::Int(meta.uid() as i64)));
        fields.push(Field::new("mfr_gid", Value::Int(meta.gid() as i64)));
    }
    Ok(fields)
}

/// Formats a timestamp as an ISO-8601 UTC datetime (`YYYY-MM-DDTHH:MM:SSZ`).
/// Times before the Unix epoch are clamped to the epoch.
pub fn iso8601(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Days-since-epoch → (year, month, day) in the proleptic Gregorian calendar
/// (Howard Hinnant's `civil_from_days` algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}
