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
    /// Named focus scope (spec-gui "Keybinding"): fires only while the widget
    /// tagged `data-mf-focus="<focus>"` is focused, even inside a text input.
    /// `None` = not focus-scoped. The most specific scope dimension.
    #[serde(default)]
    pub focus: Option<String>,
}

/// One binding of the compiled table sent to the frontend.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CompiledBinding {
    /// Normalized combo sequence, e.g. `["g", "g"]` or `["ctrl+k"]`.
    pub keys: Vec<String>,
    /// Command invocation string, e.g. `"metarecord-list:set-mode grid"`.
    pub invocation: String,
    /// Panel type scope; `None` = global.
    pub when: Option<String>,
    pub text_input: bool,
    /// Named focus scope; `None` = not focus-scoped.
    pub focus: Option<String>,
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

/// A combo's TOML value: either a single binding or, when the same combo needs
/// several `when`-scoped bindings (e.g. `down` in every list panel), an array
/// of them.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum SpecOrList {
    One(BindingSpec),
    Many(Vec<BindingSpec>),
}

impl SpecOrList {
    fn into_vec(self) -> Vec<BindingSpec> {
        match self {
            SpecOrList::One(spec) => vec![spec],
            SpecOrList::Many(specs) => specs,
        }
    }
}

/// Parses a keybindings TOML file into (normalized combo, spec) pairs. A combo
/// key may map to one binding or an array of them; each element becomes its own
/// pair, so the same combo can appear more than once (different `when` scopes).
pub fn parse_toml(source: &str) -> Result<Vec<(Vec<String>, BindingSpec)>, String> {
    let table: HashMap<String, SpecOrList> =
        toml::from_str(source).map_err(|e| format!("invalid keybindings file: {e}"))?;
    let mut entries: Vec<(Vec<String>, BindingSpec)> = Vec::new();
    for (combo, value) in table {
        let keys = parse_combo(&combo)?;
        for spec in value.into_vec() {
            entries.push((keys.clone(), spec));
        }
    }
    // HashMap order is arbitrary: sort for determinism (by combo, then scope).
    entries.sort_by(|a, b| {
        (&a.0, &a.1.when, &a.1.focus).cmp(&(&b.0, &b.1.when, &b.1.focus))
    });
    Ok(entries)
}

/// A binding's identity for override purposes: the same key sequence in the
/// same `when` and `focus` scope. So `down` in `metarecord-list`, `down` in
/// `file-manager` and `down` focus-scoped to the finder are all distinct, and a
/// user file overrides each independently.
type BindingKey = (Vec<String>, Option<String>, Option<String>);

/// The merged binding set.
pub struct KeybindingSet {
    /// Keyed by (combo, scope); defaults overridden by the user per (combo,
    /// scope), so rebinding one panel's `down` leaves the others intact.
    config: HashMap<BindingKey, BindingSpec>,
    /// Panel suggestions that survived conflict checking.
    suggestions: Vec<CompiledBinding>,
}

impl KeybindingSet {
    /// Merges the engine defaults with the user file (user wins per (combo,
    /// scope)).
    pub fn from_sources(defaults: &str, user: &str) -> Result<Self, String> {
        let mut config: HashMap<BindingKey, BindingSpec> = HashMap::new();
        for (keys, spec) in parse_toml(defaults)? {
            config.insert((keys, spec.when.clone(), spec.focus.clone()), spec);
        }
        for (keys, spec) in parse_toml(user)? {
            config.insert((keys, spec.when.clone(), spec.focus.clone()), spec);
        }
        Ok(KeybindingSet { config, suggestions: Vec::new() })
    }

    /// The complete set from a single source. In the git-backed config model
    /// (spec-config) the user's `keybindings.toml` already is the shipped
    /// defaults merged with their edits, so there is no separate defaults layer.
    pub fn from_source(source: &str) -> Result<Self, String> {
        Self::from_sources(source, "")
    }

    /// `metafolder.addKeybinding` — applied only when no configured
    /// binding has the same combo and the same `when` scope.
    pub fn add_suggestion(
        &mut self,
        combo: &str,
        invocation: &str,
        when: Option<&str>,
        text_input: bool,
        focus: Option<&str>,
    ) -> Result<(), String> {
        let keys = parse_combo(combo)?;
        if self
            .config
            .contains_key(&(keys.clone(), when.map(str::to_string), focus.map(str::to_string)))
            || self.suggestions.iter().any(|s| {
                s.keys == keys && s.when.as_deref() == when && s.focus.as_deref() == focus
            })
        {
            return Ok(()); // user/config binding wins; suggestion dropped
        }
        self.suggestions.push(CompiledBinding {
            keys,
            invocation: invocation.to_string(),
            when: when.map(str::to_string),
            text_input,
            focus: focus.map(str::to_string),
        });
        Ok(())
    }

    /// Flat table for the frontend matcher, deterministically ordered.
    pub fn compiled(&self) -> Vec<CompiledBinding> {
        let mut table: Vec<CompiledBinding> = self
            .config
            .iter()
            .map(|((keys, _when, _focus), spec)| CompiledBinding {
                keys: keys.clone(),
                invocation: spec.command.clone(),
                when: spec.when.clone(),
                text_input: spec.text_input,
                focus: spec.focus.clone(),
            })
            .chain(self.suggestions.iter().cloned())
            .collect();
        table.sort_by(|a, b| (&a.keys, &a.when, &a.focus).cmp(&(&b.keys, &b.when, &b.focus)));
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
        // The literal "+" is the chord separator, so the "+" key is spelled
        // "plus" (the JS matcher maps the event the same way).
        assert_eq!(parse_combo("plus").unwrap(), vec!["plus"]);
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
"j"       = { command = "metarecord-list:next",         when = "metarecord-list"            }
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
        assert_eq!(j.command, "metarecord-list:next");
        assert_eq!(j.when.as_deref(), Some("metarecord-list"));
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

    #[test]
    fn test_parse_toml_accepts_an_array_of_bindings_for_one_combo() {
        // The same combo can carry several scoped bindings (different `when`),
        // which a single table cannot express.
        let source = r#"
"j" = [
  { command = "metarecord-list:next", when = "metarecord-list" },
  { command = "file-manager:next",    when = "file-manager" },
]
"enter" = [
  { command = "editing:confirm", text-input = true },
  { command = "metarecord-list:open", when = "metarecord-list" },
]
"t" = { command = "tab:new" }
"#;
        let entries = parse_toml(source).unwrap();
        // 2 (j) + 2 (enter) + 1 (t)
        assert_eq!(entries.len(), 5);

        let js: Vec<&BindingSpec> = entries
            .iter()
            .filter(|(k, _)| k == &vec!["j".to_string()])
            .map(|(_, s)| s)
            .collect();
        assert_eq!(js.len(), 2);
        assert!(js.iter().any(|s| s.command == "metarecord-list:next"
            && s.when.as_deref() == Some("metarecord-list")));
        assert!(js.iter().any(|s| s.command == "file-manager:next"
            && s.when.as_deref() == Some("file-manager")));

        // The single-table form still works alongside arrays.
        assert!(entries
            .iter()
            .any(|(k, s)| k == &vec!["t".to_string()] && s.command == "tab:new"));
    }

    #[test]
    fn test_array_bindings_survive_into_the_compiled_table() {
        let source = r#"
"j" = [
  { command = "metarecord-list:next", when = "metarecord-list" },
  { command = "file-manager:next",    when = "file-manager" },
]
"#;
        let set = KeybindingSet::from_source(source).unwrap();
        let table = set.compiled();
        let js: Vec<_> = table.iter().filter(|b| b.keys == ["j"]).collect();
        assert_eq!(js.len(), 2);
    }

    #[test]
    fn test_focus_scope_parses_and_is_a_distinct_binding() {
        // A focus-scoped binding coexists with a when-scoped one on the same
        // combo (different identity), and the user overrides each independently.
        let defaults = r#"
"down" = [
  { command = "metarecord-list:next", when = "metarecord-list" },
  { command = "metarecord-list:next", focus = "finder" },
]
"#;
        let set = KeybindingSet::from_source(defaults).unwrap();
        let downs: Vec<_> = set.compiled().into_iter().filter(|b| b.keys == ["down"]).collect();
        assert_eq!(downs.len(), 2);
        let finder = downs.iter().find(|b| b.focus.as_deref() == Some("finder")).unwrap();
        assert_eq!(finder.when, None);
        assert_eq!(finder.invocation, "metarecord-list:next");

        // Overriding the focus-scoped one leaves the when-scoped one intact.
        let user = r#""down" = { command = "metarecord-list:apply-finder", focus = "finder" }"#;
        let set = KeybindingSet::from_sources(defaults, user).unwrap();
        let downs: Vec<_> = set.compiled().into_iter().filter(|b| b.keys == ["down"]).collect();
        assert_eq!(downs.len(), 2);
        assert_eq!(
            downs.iter().find(|b| b.focus.as_deref() == Some("finder")).unwrap().invocation,
            "metarecord-list:apply-finder"
        );
        assert_eq!(
            downs.iter().find(|b| b.when.as_deref() == Some("metarecord-list")).unwrap().invocation,
            "metarecord-list:next"
        );
    }

    #[test]
    fn test_suggestion_dropped_only_on_same_combo_when_and_focus() {
        // A configured focus-scoped binding blocks a same-(combo,focus)
        // suggestion but not one with a different focus.
        let user = r#""down" = { command = "user:finder", focus = "finder" }"#;
        let mut set = KeybindingSet::from_sources("", user).unwrap();
        set.add_suggestion("down", "panel:finder", None, false, Some("finder")).unwrap();
        set.add_suggestion("down", "panel:other", None, false, Some("other")).unwrap();
        let downs: Vec<_> = set.compiled().into_iter().filter(|b| b.keys == ["down"]).collect();
        assert_eq!(downs.len(), 2); // the "finder" suggestion was dropped
        assert!(downs.iter().any(|b| b.invocation == "user:finder"));
        assert!(downs.iter().any(|b| b.invocation == "panel:other"));
    }

    #[test]
    fn test_user_override_targets_one_scope_of_a_shared_combo() {
        // A user rebinds `j` only for metarecord-list; the file-manager `j`
        // default is untouched (override is keyed by combo AND scope).
        let defaults = r#"
"j" = [
  { command = "metarecord-list:next", when = "metarecord-list" },
  { command = "file-manager:next",    when = "file-manager" },
]
"#;
        let user = r#""j" = { command = "metarecord-list:custom", when = "metarecord-list" }"#;
        let set = KeybindingSet::from_sources(defaults, user).unwrap();
        let table = set.compiled();
        let js: Vec<_> = table.iter().filter(|b| b.keys == ["j"]).collect();
        assert_eq!(js.len(), 2);
        let mlist = js.iter().find(|b| b.when.as_deref() == Some("metarecord-list")).unwrap();
        assert_eq!(mlist.invocation, "metarecord-list:custom");
        let fm = js.iter().find(|b| b.when.as_deref() == Some("file-manager")).unwrap();
        assert_eq!(fm.invocation, "file-manager:next");
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
        // The same combo spelled differently still merges as one metarecord.
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
        set.add_suggestion("ctrl+l", "my-panel:change-mode list", Some("my-panel"), false, None)
            .unwrap();
        let table = set.compiled();
        assert_eq!(table.len(), 1);
        assert_eq!(table[0].invocation, "my-panel:change-mode list");
        assert_eq!(table[0].when.as_deref(), Some("my-panel"));
    }

    #[test]
    fn test_suggestion_dropped_on_same_combo_and_scope() {
        let user = r#""j" = { command = "user:thing", when = "metarecord-list" }"#;
        let mut set = KeybindingSet::from_sources("", user).unwrap();
        set.add_suggestion("j", "metarecord-list:next", Some("metarecord-list"), false, None)
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
        set.add_suggestion("j", "metarecord-list:next", Some("metarecord-list"), false, None)
            .unwrap();
        let table = set.compiled();
        assert_eq!(table.len(), 2);
    }

    #[test]
    fn test_duplicate_suggestions_keep_first() {
        let mut set = KeybindingSet::from_sources("", "").unwrap();
        set.add_suggestion("j", "panel:first", Some("p"), false, None).unwrap();
        set.add_suggestion("j", "panel:second", Some("p"), false, None).unwrap();
        let table = set.compiled();
        assert_eq!(table.len(), 1);
        assert_eq!(table[0].invocation, "panel:first");
    }

    #[test]
    fn test_shipped_defaults_parse() {
        let defaults = include_str!("../default-config/keybindings.toml");
        let set = KeybindingSet::from_sources(defaults, "").unwrap();
        let table = set.compiled();
        assert!(table.iter().any(|b| b.invocation == "tab:new"));
        assert!(table.iter().any(|b| b.keys == [":"]));
        assert!(table.iter().any(|b| b.keys == ["x"] && b.invocation == "panel:swap"));

        // The panel bindings now live in the shipped file (not addKeybinding):
        // the same combo carries several scoped bindings.
        let down: Vec<_> = table.iter().filter(|b| b.keys == ["down"]).collect();
        assert!(down.len() >= 6, "expected per-panel `down` bindings, got {}", down.len());
        assert!(down
            .iter()
            .any(|b| b.when.as_deref() == Some("metarecord-list") && b.invocation == "metarecord-list:next"));
        assert!(down
            .iter()
            .any(|b| b.when.as_deref() == Some("file-manager") && b.invocation == "file-manager:next"));
        // `enter` mixes a global text-input binding with per-panel actions.
        let enter: Vec<_> = table.iter().filter(|b| b.keys == ["enter"]).collect();
        assert!(enter.iter().any(|b| b.when.is_none() && b.text_input));
        assert!(enter
            .iter()
            .any(|b| b.when.as_deref() == Some("treeref") && b.invocation == "treeref:descend"));

        // The finder's in-input shortcuts are focus-scoped (spec-gui "focus").
        assert!(down.iter().any(|b| b.focus.as_deref() == Some("finder")));
        assert!(table
            .iter()
            .any(|b| b.keys == ["ctrl+enter"] && b.focus.as_deref() == Some("finder")
                && b.invocation == "pick:confirm"));
    }

    // ── Compilation ──────────────────────────────────────────────────────

    #[test]
    fn test_compiled_table_carries_scope_dimensions_and_sequences() {
        let defaults = r#"
"g g" = { command = "metarecord-list:goto-top", when = "metarecord-list" }
"escape" = { command = "editing:unfocus", text-input = true }
"#;
        let set = KeybindingSet::from_sources(defaults, "").unwrap();
        let table = set.compiled();

        let gg = table.iter().find(|b| b.keys == ["g", "g"]).unwrap();
        assert_eq!(gg.invocation, "metarecord-list:goto-top");
        assert_eq!(gg.when.as_deref(), Some("metarecord-list"));
        assert!(!gg.text_input);

        let escape = table.iter().find(|b| b.keys == ["escape"]).unwrap();
        assert!(escape.text_input);
        assert_eq!(escape.when, None);
    }
}
