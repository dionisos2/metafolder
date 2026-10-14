//! The hierarchical-tag model behind `mf tag` (doc "Hierarchical tags"):
//! the field-name convention plus the pure subsumption helpers that
//! the `gui-tag-*` scripts used to re-implement in bash. Everything here is
//! pure and unit-tested; the HTTP orchestration lives in `commands::tag_*`.
//!
//! Convention (all names configurable via the `[tag]` config table): a *tag
//! entry* is a metarecord `mf_schema = <entry_type>` (`"tag"`) whose position in the
//! tag hierarchy is a `TreeRef` field `<path_field>` (`"path"`) — its resolved
//! path (`"musique/jazz"`) is the tag's identity, and an optional `label` may
//! name it. Records reference entries through the multi-map Ref fields
//! `<positive>` / `<negative>` / `<mixed>` (`tag`/`negative_tag`/`mixed_tag`).
//! Siblings never exclude each other. The pure helpers below operate
//! on the resolved path strings, so the hierarchy logic is unchanged whether the
//! path comes from a TreeRef (now) or a string field (before).

use serde::Deserialize;

/// The tag-model field names (the `[tag]` config table). Defaults reproduce the
/// convention the scripts already use, so an empty config changes nothing.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct TagConfig {
    /// Field name holding the entry-type marker (the schema field by default).
    pub type_field: String,
    /// `mf_schema` value marking a metarecord as a tag entry.
    pub entry_type: String,
    /// The tag entry's hierarchy field — a `TreeRef` whose resolved path is the
    /// tag's identity.
    pub path_field: String,
    /// Multi-map Ref field: the record *has* the tag.
    pub positive: String,
    /// Multi-map Ref field: the record does *not* have the tag.
    pub negative: String,
    /// Multi-map Ref field: a folder is *mixed* w.r.t. the tag.
    pub mixed: String,
}

impl Default for TagConfig {
    fn default() -> Self {
        TagConfig {
            type_field: "mf_schema".into(),
            entry_type: "tag".into(),
            path_field: "path".into(),
            positive: "tag".into(),
            negative: "negative_tag".into(),
            mixed: "mixed_tag".into(),
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
