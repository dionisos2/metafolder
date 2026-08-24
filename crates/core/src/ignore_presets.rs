//! Named `mf_ignore` presets (spec-file-tracking "Ignore presets"): reusable,
//! named groups of ignore-pattern regexes living in the user configuration
//! (`$XDG_CONFIG_HOME/metafolder/core/ignore-presets.toml`, installed by
//! `metafolder-sync-config`). Expansion (preset name -> patterns) is a pure,
//! client-side transformation shared by the CLI (`mf ignore`, `mf repo init`)
//! and the GUI; the daemon never reads this file and carries no built-in ignore
//! policy. A missing or malformed file is an error — there is no embedded
//! runtime fallback (spec-config "No runtime fallback").

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::config;

/// One preset: a leaf (`patterns`), a group (`pattern-set` referencing other
/// presets), or both (their expansions are unioned).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preset {
    /// Human-readable description shown by `mf ignore list`.
    #[serde(default)]
    pub description: String,
    /// Literal regex patterns contributed by this preset.
    #[serde(default)]
    pub patterns: Vec<String>,
    /// Names of other presets whose patterns are included (expanded first).
    #[serde(default, rename = "pattern-set")]
    pub pattern_set: Vec<String>,
}

/// A parsed presets file: a map from preset name to its definition.
#[derive(Debug, Clone, Default)]
pub struct Presets(HashMap<String, Preset>);

impl Presets {
    /// Parses the TOML source of a presets file.
    pub fn parse(src: &str) -> Result<Self, String> {
        let map: HashMap<String, Preset> =
            toml::from_str(src).map_err(|e| format!("invalid ignore-presets.toml: {e}"))?;
        Ok(Presets(map))
    }

    /// The preset names with their descriptions, sorted by name.
    pub fn descriptions(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .0
            .iter()
            .map(|(name, p)| (name.clone(), p.description.clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Whether a preset by that name exists.
    pub fn contains(&self, name: &str) -> bool {
        self.0.contains_key(name)
    }

    /// Expands the given preset names into the ordered, de-duplicated list of
    /// regex patterns they contribute. An unknown preset name or a reference
    /// cycle is an error. An empty `names` yields an empty list (used by
    /// `mf ignore set` to clear the target's set).
    pub fn expand(&self, names: &[&str]) -> Result<Vec<String>, String> {
        let mut out: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for name in names {
            let mut stack: Vec<String> = Vec::new();
            self.expand_into(name, &mut out, &mut seen, &mut stack)?;
        }
        Ok(out)
    }

    /// Recursive helper: appends `name`'s patterns to `out` (own patterns first,
    /// then its group members), de-duplicating via `seen` and guarding against
    /// reference cycles via the active-`stack` path.
    fn expand_into(
        &self,
        name: &str,
        out: &mut Vec<String>,
        seen: &mut std::collections::HashSet<String>,
        stack: &mut Vec<String>,
    ) -> Result<(), String> {
        if stack.iter().any(|n| n == name) {
            stack.push(name.to_string());
            return Err(format!("ignore preset reference cycle: {}", stack.join(" -> ")));
        }
        let preset = self
            .0
            .get(name)
            .ok_or_else(|| format!("unknown ignore preset '{name}'"))?;
        stack.push(name.to_string());
        for pattern in &preset.patterns {
            if seen.insert(pattern.clone()) {
                out.push(pattern.clone());
            }
        }
        for member in &preset.pattern_set {
            self.expand_into(member, out, seen, stack)?;
        }
        stack.pop();
        Ok(())
    }
}

/// `$XDG_CONFIG_HOME/metafolder/core/ignore-presets.toml`.
pub fn presets_path() -> Option<PathBuf> {
    config::crate_config_dir("core").map(|dir| dir.join("ignore-presets.toml"))
}

/// Reads and parses the presets file at `path`. A missing or malformed file is
/// an error; there is no fall back to a shipped default (spec-config).
pub fn load_from(path: &Path) -> Result<Presets, String> {
    let src = config::read_required(path)?;
    Presets::parse(&src)
}

/// Resolves the configured path and loads the presets.
pub fn load() -> Result<Presets, String> {
    let path = presets_path().ok_or("cannot determine the configuration directory")?;
    load_from(&path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped presets file, embedded for tests only (never a runtime
    /// fallback).
    const DEFAULT_PRESETS: &str = include_str!("../default-config/ignore-presets.toml");

    fn sample() -> Presets {
        Presets::parse(
            r#"
[a]
patterns = ['pat-a']

[b]
patterns = ['pat-b', 'shared']

[c]
patterns = ['shared', 'pat-c']

[grp]
pattern-set = ["a", "b"]

[both]
patterns = ['pat-both']
pattern-set = ["a"]
"#,
        )
        .unwrap()
    }

    #[test]
    fn expand_leaf_returns_its_patterns() {
        assert_eq!(sample().expand(&["a"]).unwrap(), vec!["pat-a"]);
    }

    #[test]
    fn expand_group_unions_members_in_order() {
        assert_eq!(sample().expand(&["grp"]).unwrap(), vec!["pat-a", "pat-b", "shared"]);
    }

    #[test]
    fn expand_multiple_names_concatenates() {
        assert_eq!(sample().expand(&["a", "b"]).unwrap(), vec!["pat-a", "pat-b", "shared"]);
    }

    #[test]
    fn expand_deduplicates_shared_patterns() {
        // `b` and `c` both contribute 'shared'; it appears once, at first sight.
        assert_eq!(sample().expand(&["b", "c"]).unwrap(), vec!["pat-b", "shared", "pat-c"]);
    }

    #[test]
    fn expand_preset_with_own_patterns_and_a_group() {
        assert_eq!(sample().expand(&["both"]).unwrap(), vec!["pat-both", "pat-a"]);
    }

    #[test]
    fn expand_empty_names_is_empty() {
        assert!(sample().expand(&[]).unwrap().is_empty());
    }

    #[test]
    fn expand_unknown_preset_is_error() {
        let err = sample().expand(&["nope"]).unwrap_err();
        assert!(err.contains("unknown ignore preset 'nope'"), "{err}");
    }

    #[test]
    fn expand_detects_reference_cycle() {
        let p = Presets::parse(
            r#"
[x]
pattern-set = ["y"]
[y]
pattern-set = ["x"]
"#,
        )
        .unwrap();
        let err = p.expand(&["x"]).unwrap_err();
        assert!(err.contains("cycle"), "{err}");
    }

    #[test]
    fn parse_rejects_unknown_field() {
        let err = Presets::parse("[a]\nbogus = 1\n").unwrap_err();
        assert!(err.contains("invalid ignore-presets.toml"), "{err}");
    }

    #[test]
    fn shipped_default_parses_and_default_expands() {
        let p = Presets::parse(DEFAULT_PRESETS).expect("shipped presets parse");
        let patterns = p.expand(&["default"]).expect("default expands");
        assert!(!patterns.is_empty());
        // The rust-build pattern is part of the default set.
        assert!(
            patterns.iter().any(|p| p.contains("incremental")),
            "default should include the cargo build-intermediates pattern"
        );
        // git/metafolder/hidden are distinct leaves; all three land in default.
        assert!(patterns.iter().any(|p| p.contains(r"\.git")));
        assert!(patterns.iter().any(|p| p.contains(r"\.metafolder")));
        assert!(patterns.iter().any(|p| p == r"(^|/)\.[^/]+"));
    }

    #[test]
    fn descriptions_are_sorted_by_name() {
        let d = sample().descriptions();
        let names: Vec<&str> = d.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "both", "c", "grp"]);
    }
}
