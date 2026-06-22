//! Stat-derived `mfr_*` field values (spec-data-model "Reserved fields",
//! spec-platform "File metadata fields").

use std::path::Path;

use anyhow::{Context, Result};
use metafolder_core::date::ms_from_systemtime;
use metafolder_core::metarecord::{Field, Value};

/// The stat-derived fields of a file, directory or symlink: `mfr_type`
/// (`file`/`dir`/`symlink`), `mfr_size`, `mfr_mtime`, `mfr_btime` (when the
/// platform/filesystem records it), and on Unix `mfr_permissions`, `mfr_uid`,
/// `mfr_gid`. For a symlink, `mfr_symlink_target` records where it points.
/// (`mfr_mime` is v2.)
///
/// Symlinks are **never dereferenced** (`lstat`, not `stat`): a symlink is a
/// first-class entity described by its own metadata and its target *path*, so
/// the daemon never reads the target's content or stats a location outside the
/// repository through a link (spec-platform "Symbolic links").
pub fn stat_fields(path: &Path) -> Result<Vec<Field>> {
    let meta =
        std::fs::symlink_metadata(path).with_context(|| format!("Failed to stat {path:?}"))?;
    let file_type = meta.file_type();
    let kind = if file_type.is_symlink() {
        "symlink"
    } else if file_type.is_dir() {
        "dir"
    } else {
        "file"
    };
    let mut fields = vec![
        Field::new("mfr_type", Value::String(kind.to_string())),
        Field::new("mfr_size", Value::Int(meta.len() as i64)),
    ];
    if file_type.is_symlink() {
        // The link's "content" is where it points — recorded without following.
        if let Ok(target) = std::fs::read_link(path) {
            fields.push(Field::new(
                "mfr_symlink_target",
                Value::String(target.to_string_lossy().into_owned()),
            ));
        }
    }
    if let Ok(mtime) = meta.modified() {
        fields.push(Field::new("mfr_mtime", Value::DateTime(ms_from_systemtime(mtime))));
    }
    // `created()` abstracts the per-OS source (statx/st_birthtime/ftCreationTime);
    // it errors when the filesystem does not record a birth time, in which case
    // `mfr_btime` is simply absent (spec-platform "File metadata fields").
    if let Ok(btime) = meta.created() {
        fields.push(Field::new("mfr_btime", Value::DateTime(ms_from_systemtime(btime))));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn field<'a>(fields: &'a [Field], name: &str) -> Option<&'a Value> {
        fields.iter().find(|f| f.name == name).map(|f| &f.value)
    }

    #[cfg(unix)]
    #[test]
    fn stat_fields_describes_a_symlink_without_following_it() {
        let dir = std::env::temp_dir().join(format!("mf-statlink-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let link = dir.join("link");
        // Target does not exist (and is outside any repo): following it would
        // fail to stat; lstat must succeed and describe the link itself.
        let target = "/nonexistent/secret/target";
        std::os::unix::fs::symlink(target, &link).unwrap();

        let fields = stat_fields(&link).unwrap();
        assert_eq!(field(&fields, "mfr_type"), Some(&Value::String("symlink".into())));
        assert_eq!(field(&fields, "mfr_symlink_target"), Some(&Value::String(target.into())));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stat_fields_types_regular_files_and_dirs() {
        let dir = std::env::temp_dir().join(format!("mf-stattype-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.txt"), b"hi").unwrap();

        let file = stat_fields(&dir.join("f.txt")).unwrap();
        assert_eq!(field(&file, "mfr_type"), Some(&Value::String("file".into())));
        assert_eq!(field(&file, "mfr_symlink_target"), None);

        let d = stat_fields(&dir).unwrap();
        assert_eq!(field(&d, "mfr_type"), Some(&Value::String("dir".into())));

        std::fs::remove_dir_all(&dir).ok();
    }
}
