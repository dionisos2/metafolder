//! Reserved field names (doc "Reserved fields"):
//! - `mfr_*` are written by the daemon; user writes require `force`.
//! - `mf_*` are read by the daemon; written freely, but unknown names are
//!   rejected to prevent typos from silently having no effect.
//!
//! [`RESERVED_FIELDS`] is the registry of every one of them: the `mf_*` names a
//! write may use, and the `mfr_*` names the daemon writes — a test holds every
//! `mfr_*` name of the daemon's source to it, and the wiki's `Reserved field`
//! catalog is generated from it.

/// A reserved field: its name (a family of names when it ends with `*`), the
/// type of its values and what it holds.
pub struct ReservedField {
    pub name: &'static str,
    pub value_type: &'static str,
    pub summary: &'static str,
}

const fn field(
    name: &'static str,
    value_type: &'static str,
    summary: &'static str,
) -> ReservedField {
    ReservedField { name, value_type, summary }
}

/// Every reserved field.
pub const RESERVED_FIELDS: &[ReservedField] = &[
    // The fields that steer the daemon.
    field("mf_watch", "bool", "Whether a metarecord and what lies under it are tracked."),
    field("mf_ignore", "string", "An ignore pattern; one row per pattern."),
    field(
        "mf_schema",
        "string",
        "A type of the metarecord, checked by the schema; one row per type.",
    ),
    field("mf_sync", "string", "Whether metafolder or another tool owns this content during sync."),
    // The fields of a file.
    field("mfr_path", "tree_ref", "A file's position in the filesystem's tree."),
    field("mfr_type", "string", "What the path is: file, dir or symlink."),
    field("mfr_size", "int", "The file's size in bytes at its last update."),
    field("mfr_mtime", "datetime", "The file's last content modification."),
    field("mfr_btime", "datetime", "The file's creation time, when the filesystem records one."),
    field("mfr_symlink_target", "string", "A symlink's target path."),
    field("mfr_permissions", "string", "The Unix permission bits, in octal."),
    field("mfr_uid", "int", "The id of the file's owner."),
    field("mfr_gid", "int", "The id of the file's group."),
    field("mfr_inode", "string", "The device and inode of a file with more than one hard link."),
    field("mfr_mime", "string", "The MIME type, from the content."),
    field("mfr_partial_hash", "string", "The hash of a file's first and last 4 KiB."),
    field("mfr_full_hash", "string", "The hash of a file's whole content."),
    field("mfr_hash_mtime", "datetime", "The file's mtime when its hashes were computed."),
    field("mfr_hash_size", "int", "The file's size when its hashes were computed."),
    field("mfr_mount", "string", "On a mount point: the mounted volume's identity."),
    field("mfr_path_old", "string", "Where an orphaned metarecord last lived."),
    field("mfr_watch_exceeded", "bool", "On a directory the watch budget could not cover."),
    field("mfr_meta_extracted", "bool", "That the file's embedded metadata has been read."),
    field("mfr_meta_*", "any", "A piece of embedded metadata, named by the metadata map."),
    // Duplicates.
    field("mfr_duplicate_group", "ref", "The duplicate group a file belongs to."),
    field("mfr_content_hash", "string", "A duplicate group's content hash."),
    field("mfr_content_size", "int", "The size of each file of a duplicate group."),
    field("mfr_duplicate_count", "int", "The number of files in a duplicate group."),
    field(
        "mfr_duplicate_reclaimable",
        "int",
        "The bytes freed by reducing a duplicate group to one file.",
    ),
];

/// Checks whether a user write to `field_name` is allowed.
pub fn check_writable(field_name: &str, force: bool) -> Result<(), String> {
    if field_name.starts_with("mfr_") {
        if force {
            return Ok(());
        }
        return Err(format!(
            "field '{field_name}' is reserved (mfr_*); pass \"force\": true to override"
        ));
    }
    if field_name.starts_with("mf_") && !RESERVED_FIELDS.iter().any(|f| f.name == field_name) {
        return Err(format!("unknown reserved field '{field_name}' (mf_* names are restricted)"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether `name` is a registered field, or one of a registered family.
    fn registered(name: &str) -> bool {
        RESERVED_FIELDS.iter().any(|f| match f.name.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None => f.name == name,
        })
    }

    fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    #[test]
    fn every_mfr_name_of_the_daemon_is_registered() {
        let names = regex::Regex::new(r#""(mfr_[a-z_]+)""#).unwrap();
        let mut files = Vec::new();
        rust_files(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files);
        let mut unknown = Vec::new();
        for file in files {
            let src = std::fs::read_to_string(&file).unwrap();
            for m in names.captures_iter(&src) {
                if !registered(&m[1]) {
                    unknown.push(format!("{}: {}", file.display(), &m[1]));
                }
            }
        }
        assert!(unknown.is_empty(), "not in RESERVED_FIELDS: {unknown:#?}");
    }

    #[test]
    fn the_registry_names_reserved_fields_once() {
        let mut seen = std::collections::HashSet::new();
        for f in RESERVED_FIELDS {
            assert!(f.name.starts_with("mf_") || f.name.starts_with("mfr_"), "{}", f.name);
            assert!(seen.insert(f.name), "{} twice", f.name);
            assert!(!f.summary.is_empty(), "{} has no summary", f.name);
        }
    }

    #[test]
    fn the_known_mf_fields_are_writable_and_others_refused() {
        for name in ["mf_watch", "mf_ignore", "mf_schema", "mf_sync"] {
            assert!(check_writable(name, false).is_ok(), "{name}");
        }
        assert!(check_writable("mf_wacth", false).is_err());
        assert!(check_writable("mfr_path", false).is_err());
        assert!(check_writable("mfr_path", true).is_ok());
        assert!(check_writable("rating", false).is_ok());
    }

    #[test]
    fn the_wiki_catalog_matches_the_code() {
        use metafolder_core::doc_gen::{generated_dir, slug, sync_generated, tid};
        let notes: std::collections::BTreeMap<String, String> = RESERVED_FIELDS
            .iter()
            .map(|f| {
                let title = format!("$:/mf/gen/Reserved field/{}", f.name);
                let writer = if f.name.starts_with("mfr_") {
                    "the daemon (a write of your own needs `force`)"
                } else {
                    "you; the daemon reads it"
                };
                let text = format!(
                    "!! Reference\n\n|!Type |`{}` |\n|!Written by |{writer} |\n",
                    f.value_type
                );
                let fields = [
                    ("title", title.as_str()),
                    ("catalog", "Reserved field"),
                    ("target", f.name),
                    ("summary", f.summary),
                ];
                (format!("{}.tid", slug(f.name)), tid(&fields, &text))
            })
            .collect();
        assert_eq!(notes.len(), RESERVED_FIELDS.len(), "two fields share a file name");
        sync_generated(&generated_dir(env!("CARGO_MANIFEST_DIR"), "Reserved field"), &notes)
            .unwrap();
    }
}
