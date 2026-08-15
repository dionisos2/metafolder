//! The hierarchical-tag model behind `mf tag` (spec-data-model "* CLI"):
//! the field-name convention plus the pure subsumption/exclusivity helpers that
//! the `gui-tag-*` scripts used to re-implement in bash. Everything here is
//! pure and unit-tested; the HTTP orchestration lives in `commands::tag_*`.
//!
//! Convention (all names configurable via the `[tag]` config table): a *tag
//! entry* is a metarecord `type = <entry_type>` (`"tag"`) whose `<name_field>`
//! (`"name"`) is a `"/"`-separated path; records reference entries through the
//! multi-map Ref fields `<positive>` / `<negative>` / `<mixed>`
//! (`tags`/`negative_tags`/`mixed_tags`). Bool flags `<partition>` on a parent
//! and `<exclusive>` on a child make a parent's direct children mutually
//! exclusive.

use std::collections::HashSet;

use serde::Deserialize;

/// The tag-model field names (the `[tag]` config table). Defaults reproduce the
/// convention the scripts already use, so an empty config changes nothing.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct TagConfig {
    /// Field name holding the entry-type marker.
    pub type_field: String,
    /// `type` value marking a metarecord as a tag entry.
    pub entry_type: String,
    /// The tag entry's path field.
    pub name_field: String,
    /// Multi-map Ref field: the record *has* the tag.
    pub positive: String,
    /// Multi-map Ref field: the record does *not* have the tag.
    pub negative: String,
    /// Multi-map Ref field: a folder is *mixed* w.r.t. the tag.
    pub mixed: String,
    /// Bool flag on a parent entry: its direct children are mutually exclusive.
    pub partition: String,
    /// Bool flag on a child entry: it forbids its siblings.
    pub exclusive: String,
}

impl Default for TagConfig {
    fn default() -> Self {
        TagConfig {
            type_field: "type".into(),
            entry_type: "tag".into(),
            name_field: "name".into(),
            positive: "tags".into(),
            negative: "negative_tags".into(),
            mixed: "mixed_tags".into(),
            partition: "partition".into(),
            exclusive: "exclusive".into(),
        }
    }
}

/// The parent path of a tag (`"a/b/c"` → `"a/b"`), or `None` for a top-level tag.
pub fn parent(path: &str) -> Option<&str> {
    path.rfind('/').map(|i| &path[..i])
}

/// Whether `a` is a strict ancestor path of `b` (`"a"` of `"a/b"`).
pub fn is_ancestor(a: &str, b: &str) -> bool {
    a != b && b.starts_with(a) && b.as_bytes().get(a.len()) == Some(&b'/')
}

/// The proper ancestor paths of `path`, deepest first (`"a/b/c"` → `["a/b","a"]`).
pub fn ancestors(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut p = path;
    while let Some(par) = parent(p) {
        out.push(par.to_string());
        p = par;
    }
    out
}

/// The vocabulary names strictly under `path`.
pub fn descendants(path: &str, vocab: &[String]) -> Vec<String> {
    vocab.iter().filter(|n| is_ancestor(path, n)).cloned().collect()
}

/// The vocabulary names sharing `path`'s parent, excluding `path` itself.
pub fn siblings(path: &str, vocab: &[String]) -> Vec<String> {
    let par = parent(path);
    vocab.iter().filter(|n| n.as_str() != path && parent(n) == par).cloned().collect()
}

/// Whether `path` forbids its siblings — it is itself `exclusive`, or its parent
/// is a `partition`.
pub fn is_exclusive(
    path: &str,
    partitions: &HashSet<String>,
    exclusives: &HashSet<String>,
) -> bool {
    exclusives.contains(path) || parent(path).is_some_and(|p| partitions.contains(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }
    fn vocab() -> Vec<String> {
        ["musique", "musique/jazz", "musique/jazz/bebop", "musique/rock", "administratif"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn test_parent_and_ancestors() {
        assert_eq!(parent("musique/jazz/bebop"), Some("musique/jazz"));
        assert_eq!(parent("musique"), None);
        assert_eq!(ancestors("musique/jazz/bebop"), vec!["musique/jazz", "musique"]);
        assert!(ancestors("musique").is_empty());
    }

    #[test]
    fn test_is_ancestor() {
        assert!(is_ancestor("musique", "musique/jazz"));
        assert!(is_ancestor("musique", "musique/jazz/bebop"));
        assert!(!is_ancestor("musique", "musique"));
        assert!(!is_ancestor("musique/jazz", "musique"));
        // Prefix that is not a path boundary must not match.
        assert!(!is_ancestor("mus", "musique"));
    }

    #[test]
    fn test_descendants() {
        assert_eq!(
            descendants("musique", &vocab()),
            vec!["musique/jazz", "musique/jazz/bebop", "musique/rock"]
        );
        assert!(descendants("administratif", &vocab()).is_empty());
    }

    #[test]
    fn test_siblings() {
        assert_eq!(siblings("musique/jazz", &vocab()), vec!["musique/rock"]);
        // Top-level siblings share the (None) parent.
        assert_eq!(siblings("musique", &vocab()), vec!["administratif"]);
    }

    #[test]
    fn test_is_exclusive() {
        // Own exclusive flag.
        assert!(is_exclusive("musique/jazz", &set(&[]), &set(&["musique/jazz"])));
        // Parent is a partition.
        assert!(is_exclusive("musique/jazz", &set(&["musique"]), &set(&[])));
        // Neither.
        assert!(!is_exclusive("musique/jazz", &set(&["administratif"]), &set(&["musique/rock"])));
        // A top-level tag with no exclusive flag is not exclusive.
        assert!(!is_exclusive("musique", &set(&["musique"]), &set(&[])));
    }
}
