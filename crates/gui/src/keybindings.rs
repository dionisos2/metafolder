//! Keybinding engine (spec-gui "Keybinding"): combo parsing, TOML model,
//! defaults + user + panel-suggestion merging, and compilation into the
//! flat table pushed to the frontend matcher (`panel-shim/keymatch.js`).
//!
//! Merge semantics:
//! - The engine ships defaults (`default-config/keybindings.toml`); the
//!   user file contains only overrides and wins per key combo.
//! - Panel suggestions (`metafolder.addKeybinding`) are weakest: applied
//!   only when the merged table has no binding with the same combo and
//!   the same `when` scope.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// One entry of a keybindings TOML file.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct BindingSpec {
    pub command: String,
    #[serde(default)]
    pub when: Option<String>,
    #[serde(default, rename = "text-input")]
    pub text_input: bool,
}

/// One binding of the compiled table sent to the frontend.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CompiledBinding {
    /// Normalized combo sequence, e.g. `["g", "g"]` or `["ctrl+k"]`.
    pub keys: Vec<String>,
    /// Command invocation string, e.g. `"entry-list:set-mode grid"`.
    pub invocation: String,
    /// Panel type scope; `None` = global.
    pub when: Option<String>,
    pub text_input: bool,
}

/// Parses and normalizes a combo sequence string: lowercased keys,
/// modifiers sorted `ctrl+alt+shift+meta`, whitespace-separated sequence.
pub fn parse_combo(input: &str) -> Result<Vec<String>, String> {
    let mut keys = Vec::new();
    for part in input.split_whitespace() {
        keys.push(normalize_chord(part)?);
    }
    if keys.is_empty() {
        return Err("empty key combo".into());
    }
    Ok(keys)
}

fn normalize_chord(chord: &str) -> Result<String, String> {
    const MODIFIERS: [&str; 4] = ["ctrl", "alt", "shift", "meta"];
    let mut present = [false; 4];
    let mut key: Option<String> = None;

    for piece in chord.split('+') {
        let piece = piece.trim().to_lowercase();
        if piece.is_empty() {
            return Err(format!("malformed key combo: '{chord}'"));
        }
        if let Some(index) = MODIFIERS.iter().position(|m| *m == piece) {
            present[index] = true;
        } else if key.is_some() {
            return Err(format!("multiple keys in combo: '{chord}'"));
        } else {
            key = Some(piece);
        }
    }

    let key = key.ok_or_else(|| format!("no key in combo: '{chord}'"))?;
    let mut out = String::new();
    for (index, modifier) in MODIFIERS.iter().enumerate() {
        if present[index] {
            out.push_str(modifier);
            out.push('+');
        }
    }
    out.push_str(&key);
    Ok(out)
}

/// Parses a keybindings TOML file into (normalized combo, spec) pairs.
pub fn parse_toml(source: &str) -> Result<Vec<(Vec<String>, BindingSpec)>, String> {
    let table: HashMap<String, BindingSpec> =
        toml::from_str(source).map_err(|e| format!("invalid keybindings file: {e}"))?;
    let mut entries: Vec<(Vec<String>, BindingSpec)> = Vec::new();
    for (combo, spec) in table {
        entries.push((parse_combo(&combo)?, spec));
    }
    // HashMap order is arbitrary: sort for determinism.
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(entries)
}

/// The merged binding set.
pub struct KeybindingSet {
    /// Keyed by normalized combo sequence; defaults overridden by user.
    config: HashMap<Vec<String>, BindingSpec>,
    /// Panel suggestions that survived conflict checking.
    suggestions: Vec<CompiledBinding>,
}

impl KeybindingSet {
    /// Merges the engine defaults with the user file (user wins per combo).
    pub fn from_sources(defaults: &str, user: &str) -> Result<Self, String> {
        let mut config: HashMap<Vec<String>, BindingSpec> = HashMap::new();
        for (keys, spec) in parse_toml(defaults)? {
            config.insert(keys, spec);
        }
        for (keys, spec) in parse_toml(user)? {
            config.insert(keys, spec);
        }
        Ok(KeybindingSet { config, suggestions: Vec::new() })
    }

    /// `metafolder.addKeybinding` — applied only when no configured
    /// binding has the same combo and the same `when` scope.
    pub fn add_suggestion(
        &mut self,
        combo: &str,
        invocation: &str,
        when: Option<&str>,
        text_input: bool,
    ) -> Result<(), String> {
        let keys = parse_combo(combo)?;
        let conflicts = |w: &Option<String>| w.as_deref() == when;
        if self
            .config
            .get(&keys)
            .map(|spec| conflicts(&spec.when))
            .unwrap_or(false)
            || self
                .suggestions
                .iter()
                .any(|s| s.keys == keys && conflicts(&s.when))
        {
            return Ok(()); // user/config binding wins; suggestion dropped
        }
        self.suggestions.push(CompiledBinding {
            keys,
            invocation: invocation.to_string(),
            when: when.map(str::to_string),
            text_input,
        });
        Ok(())
    }

    /// Flat table for the frontend matcher, deterministically ordered.
    pub fn compiled(&self) -> Vec<CompiledBinding> {
        let mut table: Vec<CompiledBinding> = self
            .config
            .iter()
            .map(|(keys, spec)| CompiledBinding {
                keys: keys.clone(),
                invocation: spec.command.clone(),
                when: spec.when.clone(),
                text_input: spec.text_input,
            })
            .chain(self.suggestions.iter().cloned())
            .collect();
        table.sort_by(|a, b| (&a.keys, &a.when).cmp(&(&b.keys, &b.when)));
        table
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Combo parsing ────────────────────────────────────────────────────

    #[test]
    fn test_parse_combo_normalizes_case_and_modifier_order() {
        assert_eq!(parse_combo("ctrl+K").unwrap(), vec!["ctrl+k"]);
        assert_eq!(parse_combo("shift+ctrl+a").unwrap(), vec!["ctrl+shift+a"]);
        assert_eq!(parse_combo("Escape").unwrap(), vec!["escape"]);
        assert_eq!(parse_combo(":").unwrap(), vec![":"]);
    }

    #[test]
    fn test_parse_combo_sequences() {
        assert_eq!(parse_combo("g g").unwrap(), vec!["g", "g"]);
        assert_eq!(parse_combo("ctrl+x ctrl+s").unwrap(), vec!["ctrl+x", "ctrl+s"]);
    }

    #[test]
    fn test_parse_combo_rejects_malformed_input() {
        assert!(parse_combo("").is_err());
        assert!(parse_combo("ctrl+").is_err());
        assert!(parse_combo("ctrl").is_err()); // a modifier alone is not a key
        assert!(parse_combo("a+b").is_err()); // two non-modifier keys
    }

    // ── TOML model (spec examples verbatim) ──────────────────────────────

    const SPEC_EXAMPLE: &str = r#"
"escape"  = { command = "editing:unfocus",         text-input = true              }
"ctrl+a"  = { command = "editing:goto-line-start", text-input = true              }
"j"       = { command = "entry-list:next",         when = "entry-list"            }
"ctrl+e"  = { command = "my-panel:edit",           when = "my-panel", text-input = true }
"ctrl+t"  = { command = "tab:new"                                                  }
"#;

    #[test]
    fn test_parse_toml_spec_examples() {
        let entries = parse_toml(SPEC_EXAMPLE).unwrap();
        assert_eq!(entries.len(), 5);

        let find = |keys: &[&str]| {
            entries
                .iter()
                .find(|(k, _)| k.iter().map(String::as_str).collect::<Vec<_>>() == keys)
                .map(|(_, spec)| spec)
                .unwrap()
        };

        let escape = find(&["escape"]);
        assert_eq!(escape.command, "editing:unfocus");
        assert_eq!(escape.when, None);
        assert!(escape.text_input);

        let j = find(&["j"]);
        assert_eq!(j.command, "entry-list:next");
        assert_eq!(j.when.as_deref(), Some("entry-list"));
        assert!(!j.text_input);

        let ctrl_e = find(&["ctrl+e"]);
        assert_eq!(ctrl_e.when.as_deref(), Some("my-panel"));
        assert!(ctrl_e.text_input);

        let ctrl_t = find(&["ctrl+t"]);
        assert_eq!(ctrl_t.command, "tab:new");
        assert_eq!(ctrl_t.when, None);
        assert!(!ctrl_t.text_input);
    }

    #[test]
    fn test_parse_toml_rejects_garbage() {
        assert!(parse_toml("not toml at all [").is_err());
        assert!(parse_toml(r#""ctrl+" = { command = "x" }"#).is_err());
    }

    // ── Merge ────────────────────────────────────────────────────────────

    #[test]
    fn test_user_overrides_default_for_same_combo() {
        let defaults = r#""ctrl+t" = { command = "tab:new" }
"alt+w" = { command = "tab:close" }"#;
        let user = r#""ctrl+t" = { command = "panel:split" }"#;
        let set = KeybindingSet::from_sources(defaults, user).unwrap();
        let table = set.compiled();

        let ctrl_t = table.iter().find(|b| b.keys == ["ctrl+t"]).unwrap();
        assert_eq!(ctrl_t.invocation, "panel:split");
        // Untouched default survives.
        let alt_w = table.iter().find(|b| b.keys == ["alt+w"]).unwrap();
        assert_eq!(alt_w.invocation, "tab:close");
        assert_eq!(table.len(), 2);
    }

    #[test]
    fn test_combo_normalization_applies_to_merge() {
        // The same combo spelled differently still merges as one entry.
        let defaults = r#""shift+ctrl+a" = { command = "a" }"#;
        let user = r#""ctrl+shift+A" = { command = "b" }"#;
        let set = KeybindingSet::from_sources(defaults, user).unwrap();
        let table = set.compiled();
        assert_eq!(table.len(), 1);
        assert_eq!(table[0].invocation, "b");
    }

    // ── Panel suggestions ────────────────────────────────────────────────

    #[test]
    fn test_suggestion_applied_when_no_conflict() {
        let mut set = KeybindingSet::from_sources("", "").unwrap();
        set.add_suggestion("ctrl+l", "my-panel:change-mode list", Some("my-panel"), false)
            .unwrap();
        let table = set.compiled();
        assert_eq!(table.len(), 1);
        assert_eq!(table[0].invocation, "my-panel:change-mode list");
        assert_eq!(table[0].when.as_deref(), Some("my-panel"));
    }

    #[test]
    fn test_suggestion_dropped_on_same_combo_and_scope() {
        let user = r#""j" = { command = "user:thing", when = "entry-list" }"#;
        let mut set = KeybindingSet::from_sources("", user).unwrap();
        set.add_suggestion("j", "entry-list:next", Some("entry-list"), false)
            .unwrap();
        let table = set.compiled();
        assert_eq!(table.len(), 1);
        assert_eq!(table[0].invocation, "user:thing");
    }

    #[test]
    fn test_suggestion_kept_on_same_combo_different_scope() {
        // A global user "j" does not block a local suggestion: locality
        // already gives the local binding precedence at match time.
        let user = r#""j" = { command = "user:global-j" }"#;
        let mut set = KeybindingSet::from_sources("", user).unwrap();
        set.add_suggestion("j", "entry-list:next", Some("entry-list"), false)
            .unwrap();
        let table = set.compiled();
        assert_eq!(table.len(), 2);
    }

    #[test]
    fn test_duplicate_suggestions_keep_first() {
        let mut set = KeybindingSet::from_sources("", "").unwrap();
        set.add_suggestion("j", "panel:first", Some("p"), false).unwrap();
        set.add_suggestion("j", "panel:second", Some("p"), false).unwrap();
        let table = set.compiled();
        assert_eq!(table.len(), 1);
        assert_eq!(table[0].invocation, "panel:first");
    }

    #[test]
    fn test_shipped_defaults_parse() {
        let defaults = include_str!("../default-config/keybindings.toml");
        let set = KeybindingSet::from_sources(defaults, "").unwrap();
        assert!(set.compiled().iter().any(|b| b.invocation == "tab:new"));
        assert!(set.compiled().iter().any(|b| b.keys == [":"]));
    }

    // ── Compilation ──────────────────────────────────────────────────────

    #[test]
    fn test_compiled_table_carries_scope_dimensions_and_sequences() {
        let defaults = r#"
"g g" = { command = "entry-list:goto-top", when = "entry-list" }
"escape" = { command = "editing:unfocus", text-input = true }
"#;
        let set = KeybindingSet::from_sources(defaults, "").unwrap();
        let table = set.compiled();

        let gg = table.iter().find(|b| b.keys == ["g", "g"]).unwrap();
        assert_eq!(gg.invocation, "entry-list:goto-top");
        assert_eq!(gg.when.as_deref(), Some("entry-list"));
        assert!(!gg.text_input);

        let escape = table.iter().find(|b| b.keys == ["escape"]).unwrap();
        assert!(escape.text_input);
        assert_eq!(escape.when, None);
    }
}
