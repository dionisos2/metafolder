//! Stat-derived `mfr_*` field values (spec-data-model "Reserved fields",
//! spec-platform "File metadata fields").

use std::path::Path;

use anyhow::{Context, Result};
use metafolder_core::date::iso8601;
use metafolder_core::metarecord::{Field, Value};

/// The stat-derived fields of a file or directory: `mfr_type`, `mfr_size`,
/// `mfr_mtime`, `mfr_btime` (when the platform/filesystem records it), and on
/// Unix `mfr_permissions`, `mfr_uid`, `mfr_gid`. (`mfr_mime` is v2.)
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
    // `created()` abstracts the per-OS source (statx/st_birthtime/ftCreationTime);
    // it errors when the filesystem does not record a birth time, in which case
    // `mfr_btime` is simply absent (spec-platform "File metadata fields").
    if let Ok(btime) = meta.created() {
        fields.push(Field::new("mfr_btime", Value::DateTime(iso8601(btime))));
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
