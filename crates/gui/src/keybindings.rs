//! Keybinding engine (doc "Keybindings"): combo parsing, TOML model,
//! panel-suggestion merging, and compilation into the flat table pushed to
//! the frontend matcher (`panel-shim/keymatch.js`).
//!
//! - `keybindings.toml` is the complete set (doc "The keybindings file"): the
//!   shipped bindings and the user's edits are one file, merged by git, and
//!   the engine has no defaults layer of its own.
//! - Panel suggestions (`metafolder.addKeybinding`) are weakest: applied
//!   only when the file has no binding with the same combo and the same
//!   `when` and `focus` scope.

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
    /// Named focus scope (doc "Keybindings"): fires only while the widget
    /// tagged `data-mf-focus="<focus>"` is focused, even inside a text input.
    /// `None` = not focus-scoped. The most specific scope dimension.
    #[serde(default)]
    pub focus: Option<String>,
    /// The default target of a `{target}` template (doc "Keybinding
    /// variables"): `here`, `other` or `new`. Gone once expanded.
    #[serde(default)]
    pub target: Option<String>,
}

/// One binding of the compiled table sent to the frontend.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CompiledBinding {
    /// Normalized combo sequence, e.g. `["g", "g"]` or `["ctrl+k"]`.
    pub keys: Vec<String>,
    /// Command invocation string, e.g. `"metarecord-list:set mode grid"`.
    pub invocation: String,
    /// Panel type scope; `None` = global.
    pub when: Option<String>,
    pub text_input: bool,
    /// Named focus scope; `None` = not focus-scoped.
    pub focus: Option<String>,
    /// Suggested by a panel or by `commands.js` (`addKeybinding`): in effect,
    /// but not in `keybindings.toml`, so not something its editor can remove.
    pub suggested: bool,
}

/// Where a binding applies: its combo and its scope — what the settings panel
/// names when it writes or replaces one (doc "Changing keybindings").
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct BindingPlace {
    pub combo: String,
    pub when: Option<String>,
    pub focus: Option<String>,
}

impl BindingPlace {
    /// The same key in the same scope, however the combo is spelled.
    pub fn is(&self, other: &BindingPlace) -> bool {
        self.when == other.when
            && self.focus == other.focus
            && parse_combo(&self.combo).ok() == parse_combo(&other.combo).ok()
    }
}

/// How a scope reads in a sentence: "globally", "in log", "at @find".
fn describe_scope(when: Option<&str>, focus: Option<&str>) -> String {
    match (when, focus) {
        (None, None) => "globally".into(),
        (Some(when), None) => format!("in {when}"),
        (None, Some(focus)) => format!("at @{focus}"),
        (Some(when), Some(focus)) => format!("at @{focus} in {when}"),
    }
}

/// What stands in the way of binding a key at `new`, given the bindings in
/// effect (`table`), the ones it takes the place of left out.
///
/// Refused: a key already bound in the same place (to change it, edit that
/// binding), and a key that starts — or is started by — a sequence bound in the
/// same place: a key alone runs before any sequence it starts, so one of the two
/// could never run there (`keymatch.js`).
pub fn check_binding(table: &[CompiledBinding], new: &BindingPlace) -> Result<(), String> {
    binding_in_the_way(table, new).map_err(|(error, _)| error)
}

/// `check_binding`, with the binding that refuses the key.
fn binding_in_the_way(
    table: &[CompiledBinding],
    new: &BindingPlace,
) -> Result<(), (String, Option<KeyPlace>)> {
    let keys = parse_combo(&new.combo).map_err(|e| (e, None))?;
    for binding in table {
        if binding.when != new.when || binding.focus != new.focus {
            continue;
        }
        let scope = describe_scope(new.when.as_deref(), new.focus.as_deref());
        if let Some(clash) = clash(&keys, binding, &scope) {
            let place = KeyPlace {
                keys: binding.keys.clone(),
                when: binding.when.clone(),
                focus: binding.focus.clone(),
            };
            return Err((clash, Some(place)));
        }
    }
    Ok(())
}

/// Why `keys` cannot be bound beside `binding`, in the same place (`scope`).
fn clash(keys: &[String], binding: &CompiledBinding, scope: &str) -> Option<String> {
    let starts = |long: &[String], short: &[String]| {
        long.len() > short.len() && long[..short.len()] == *short
    };
    let (combo, other) = (keys.join(" "), binding.keys.join(" "));
    let invocation = &binding.invocation;
    if binding.keys == keys {
        return Some(format!("`{combo}` is already bound {scope} to `{invocation}`"));
    }
    if starts(&binding.keys, keys) {
        return Some(format!(
            "`{combo}` would hide `{other}` (`{invocation}`) {scope}: \
             a key alone runs before any sequence it starts"
        ));
    }
    if starts(keys, &binding.keys) {
        return Some(format!(
            "`{combo}` could never run: `{other}` alone is bound {scope} to `{invocation}`"
        ));
    }
    None
}

/// Every pair of bindings in one place where a key alone hides a sequence it
/// starts — what a change of variable may bring about (doc "keybindings
/// panel"). The shipped table has none.
pub fn sequence_clashes(table: &[CompiledBinding]) -> Vec<String> {
    let mut out = Vec::new();
    for short in table {
        for long in table {
            if short.when == long.when
                && short.focus == long.focus
                && long.keys.len() > short.keys.len()
                && long.keys[..short.keys.len()] == short.keys[..]
            {
                let scope = describe_scope(short.when.as_deref(), short.focus.as_deref());
                out.push(format!(
                    "`{}` (`{}`) would hide `{}` (`{}`) {scope}",
                    short.keys.join(" "),
                    short.invocation,
                    long.keys.join(" "),
                    long.invocation
                ));
            }
        }
    }
    out
}

/// Whether `invocation` runs a command that exists (`known`). Only its first
/// word is a name — a binding may pre-fill arguments — and a shell line (`!…`)
/// names none.
pub fn check_command(invocation: &str, known: impl Fn(&str) -> bool) -> Result<(), String> {
    let invocation = invocation.trim();
    if invocation.starts_with('!') {
        return Ok(());
    }
    let name = invocation.split_whitespace().next().ok_or("no command given")?;
    if known(name) {
        Ok(())
    } else {
        Err(format!("unknown command `{name}`"))
    }
}

/// What would refuse a binding, field by field: the command, and the key in
/// its place — `None` where nothing does. The settings panel shows it while the
/// form is typed; its save refuses on the first one.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct BindingCheck {
    pub command: Option<String>,
    pub key: Option<String>,
    /// What saving would do besides binding the key, when it is worth saying:
    /// a key another line binds, which this one overrides.
    pub note: Option<String>,
    /// The binding that refuses the key, when one does.
    pub in_the_way: Option<KeyPlace>,
}

/// A key in its place: what the keybindings panel marks.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct KeyPlace {
    pub keys: Vec<String>,
    pub when: Option<String>,
    pub focus: Option<String>,
}

impl BindingCheck {
    pub fn into_result(self) -> Result<(), String> {
        match self.command.or(self.key) {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
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

/// The named keys — typed by their name, alike on every layout.
pub const NAMED_KEYS: [&str; 17] = [
    "enter",
    "escape",
    "backspace",
    "delete",
    "tab",
    "space",
    "up",
    "down",
    "left",
    "right",
    "home",
    "end",
    "pageup",
    "pagedown",
    "plus",
    "insert",
    "menu",
];

/// Typed without a modifier on bépo, AZERTY and QWERTY alike.
pub const DIRECT_KEYS: [&str; 3] = [",", "-", "="];

/// Typed with shift at worst on one of the three layouts: fine alone, never
/// under ctrl or alt, where the shift a layout needs changes the character.
pub const SHIFTED_KEYS: [&str; 22] = [
    "0", "1", "2", "3", "4", "5", "6", "7", "8", "9", ".", "/", ";", ":", "!", "*", "(", ")", "\"",
    "$", "%", "?",
];

/// Whether `key` (a chord's key, modifiers off) is one of the named keys.
pub fn is_named_key(key: &str) -> bool {
    NAMED_KEYS.contains(&key)
        || key
            .strip_prefix('f')
            .and_then(|n| n.parse::<u8>().ok())
            .is_some_and(|n| (1..=12).contains(&n))
}

/// The level of a chord (doc "Key levels"), by what it costs to type, typed
/// alike on bépo, AZERTY and QWERTY: 0 a letter alone, 1 another key typed
/// without a modifier (`, - =`, a named key), 2 shift alone,
/// a character that needs it on one of the layouts, or ctrl or alt without
/// shift, 3 ctrl or alt with shift. `None` for a chord the three layouts do
/// not type alike (`ctrl+.`, `é`, `[`).
pub fn key_level(chord: &str) -> Option<u8> {
    let (modifiers, key) = chord.rsplit_once('+').unwrap_or(("", chord));
    // `ctrl++`: the key is the `+` after the last separator.
    let (modifiers, key) =
        if key.is_empty() { (modifiers.trim_end_matches('+'), "+") } else { (modifiers, key) };
    let letter = key.len() == 1 && key.chars().all(|c| c.is_ascii_lowercase());
    let plain = letter || is_named_key(key) || DIRECT_KEYS.contains(&key);
    let control =
        modifiers.contains("ctrl") || modifiers.contains("alt") || modifiers.contains("meta");
    let shift = modifiers.contains("shift");
    match (control, shift) {
        (true, true) => plain.then_some(3),
        (true, false) | (false, true) => plain.then_some(2),
        (false, false) if letter => Some(0),
        (false, false) if plain => Some(1),
        (false, false) => SHIFTED_KEYS.contains(&key).then_some(2),
    }
}

/// Commands whose keybinding a script may never take over: they are the user's
/// only ways out of a script's question (doc "Script sessions").
// `command-input:focus` is listed bare so both of its modes are covered: the
// match below also accepts an invocation that pre-fills arguments, and a script
// must be unable to take over `!` any more than `:`.
// `panel:focus cycle` (F6) walks to the question and back from it.
const RESERVED_COMMANDS: [&str; 3] =
    ["command-input:focus", "script-keys:toggle", "panel:focus cycle"];

/// The combos a script's `mf gui input` may not await: whatever opens the
/// command input, whatever toggles the script keys, `escape` (which always
/// stops the script), `ctrl+escape` (which always kills it) and `backspace` (which always sends "back" — the way out
/// of an answer already given, doc "Script sessions"). Single chords only —
/// a script awaits one key press, so a multi-chord sequence can never collide
/// with it. Sorted and deduplicated.
pub fn reserved_combos(compiled: &[CompiledBinding]) -> Vec<String> {
    let mut combos: Vec<String> = compiled
        .iter()
        .filter(|b| {
            b.keys.len() == 1
                && RESERVED_COMMANDS
                    .iter()
                    .any(|c| b.invocation == *c || b.invocation.starts_with(&format!("{c} ")))
        })
        .map(|b| b.keys[0].clone())
        .chain(["escape", "ctrl+escape", "backspace"].map(str::to_string))
        .collect();
    combos.sort();
    combos.dedup();
    combos
}

/// Whether `key` (a script's key name, in any case) is one of `reserved`.
/// A malformed key is *not* reserved: the caller rejects it on its own terms.
pub fn is_reserved(reserved: &[String], key: &str) -> bool {
    match parse_combo(key) {
        Ok(keys) if keys.len() == 1 => reserved.contains(&keys[0]),
        _ => false,
    }
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

/// The targets a `{target}` template expands to, each under the prefix held
/// by the variable `target-<name>` (doc "Keybinding variables").
const TARGETS: [&str; 3] = ["here", "other", "new"];

/// One binding of the file, once its variables and template are expanded.
struct Entry {
    keys: Vec<String>,
    spec: BindingSpec,
    /// The file's key it comes from, as written.
    source: String,
    /// The file's key it was derived from, when a variable or a target
    /// template produced it — a literal key of the file overrides it.
    derived_from: Option<String>,
}

fn is_var_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// `combo` with each `{name}` replaced by the variable's keys. A brace that
/// names no variable (`{`, `{ }`) is a key like any other; an unknown name is
/// an error.
fn substitute(combo: &str, vars: &HashMap<String, String>) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = combo;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('}') {
            Some(end) if is_var_name(&after[..end]) => {
                let name = &after[..end];
                let keys = vars
                    .get(name)
                    .ok_or_else(|| format!("unknown variable {{{name}}} in \"{combo}\""))?;
                out.push_str(keys);
                rest = &after[end + 1..];
            }
            _ => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// The `vars` table as written: variable name → its value, which may name
/// other variables.
fn read_raw_vars(value: Option<toml::Value>) -> Result<HashMap<String, String>, String> {
    let Some(value) = value else { return Ok(HashMap::new()) };
    let toml::Value::Table(table) = value else {
        return Err("invalid keybindings file: `vars` must be a table".into());
    };
    table
        .into_iter()
        .map(|(name, value)| match value {
            toml::Value::String(keys) if is_var_name(&name) => Ok((name, keys)),
            toml::Value::String(_) => Err(format!(
                "invalid keybindings file: variable name \"{name}\" (lowercase letters, digits, -)"
            )),
            _ => Err(format!("invalid keybindings file: the variable \"{name}\" must be a string")),
        })
        .collect()
}

/// The variables with their keys: a value naming another variable
/// (`"{tags} -"`) holds that variable's keys, so that moving a domain moves
/// the variables built on it too.
fn resolve_vars(raw: &HashMap<String, String>) -> Result<HashMap<String, String>, String> {
    fn resolve(
        name: &str,
        raw: &HashMap<String, String>,
        done: &mut HashMap<String, String>,
        stack: &mut Vec<String>,
    ) -> Result<String, String> {
        if let Some(keys) = done.get(name) {
            return Ok(keys.clone());
        }
        if stack.iter().any(|n| n == name) {
            stack.push(name.to_string());
            return Err(format!(
                "invalid keybindings file: the variables refer to themselves ({})",
                stack.join(" → ")
            ));
        }
        let value = raw.get(name).ok_or_else(|| format!("unknown variable {{{name}}}"))?;
        stack.push(name.to_string());
        let mut known = HashMap::new();
        for other in referenced(value) {
            if raw.contains_key(&other) {
                known.insert(other.clone(), resolve(&other, raw, done, stack)?);
            }
        }
        stack.pop();
        let keys = substitute(value, &known)
            .map_err(|e| format!("invalid keybindings file: the variable \"{name}\": {e}"))?;
        done.insert(name.to_string(), keys.clone());
        Ok(keys)
    }
    let mut done = HashMap::new();
    for name in raw.keys() {
        resolve(name, raw, &mut done, &mut Vec::new())?;
    }
    Ok(done)
}

/// The variables a key or a value names (`{name}`), in order.
fn referenced(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        match after.find('}') {
            Some(end) if is_var_name(&after[..end]) => {
                out.push(after[..end].to_string());
                rest = &after[end + 1..];
            }
            _ => rest = after,
        }
    }
    out
}

/// The bindings one key of the file stands for: itself, or — for a
/// `{target}` template — its default target on the key, and every target
/// under its prefix.
fn expand(
    combo: &str,
    spec: BindingSpec,
    vars: &HashMap<String, String>,
) -> Result<Vec<Entry>, String> {
    let expanded = substitute(combo, vars)?;
    let derived = (expanded != combo).then(|| combo.to_string());
    let entry = |keys: &str, spec: BindingSpec, derived_from: Option<String>| {
        Ok::<_, String>(Entry {
            keys: parse_combo(keys)?,
            spec,
            source: combo.to_string(),
            derived_from,
        })
    };
    if !spec.command.contains("{target}") {
        if spec.target.is_some() {
            return Err(format!(
                "\"{combo}\": `target` is set but the command has no {{target}} to fill"
            ));
        }
        return Ok(vec![entry(&expanded, spec, derived)?]);
    }
    let default = spec.target.clone().ok_or_else(|| {
        format!("\"{combo}\": a {{target}} command needs a default `target` (here, other, new)")
    })?;
    if !TARGETS.contains(&default.as_str()) {
        return Err(format!("\"{combo}\": unknown target \"{default}\" (here, other, new)"));
    }
    let aimed = |target: &str| BindingSpec {
        command: spec.command.replace("{target}", target),
        target: None,
        ..spec.clone()
    };
    let mut entries = vec![entry(&expanded, aimed(&default), derived)?];
    for target in TARGETS {
        let var = format!("target-{target}");
        let prefix = vars
            .get(&var)
            .ok_or_else(|| format!("\"{combo}\": a {{target}} command needs the variable {var}"))?;
        entries.push(entry(&format!("{prefix} {expanded}"), aimed(target), Some(combo.into()))?);
    }
    Ok(entries)
}

/// A keybindings file, read.
struct Parsed {
    /// Its bindings, variables and templates expanded.
    entries: Vec<Entry>,
    /// Its lines as written: the key, and the binding before expansion.
    lines: Vec<(String, BindingSpec)>,
    /// The variables as written, and with their keys.
    raw_vars: HashMap<String, String>,
    vars: HashMap<String, String>,
    /// The variables' names, in the order of the file.
    var_order: Vec<String>,
}

/// The keys of the file and the names of its `vars` table, in the order the
/// file writes them (`toml` sorts them; `toml_edit` keeps the file's order).
fn in_file_order(source: &str) -> (Vec<String>, Vec<String>) {
    let Ok(doc) = source.parse::<toml_edit::DocumentMut>() else { return Default::default() };
    let keys = doc.iter().map(|(key, _)| key.to_string()).filter(|key| key != "vars").collect();
    let vars = doc
        .get("vars")
        .and_then(toml_edit::Item::as_table_like)
        .map(|vars| vars.iter().map(|(name, _)| name.to_string()).collect())
        .unwrap_or_default();
    (keys, vars)
}

/// Parses a keybindings TOML file into its bindings, variables and target
/// templates expanded (doc "Keybinding variables"). A combo key may map to
/// one binding or an array of them; each element becomes its own entry, so
/// the same combo can appear more than once (different `when` scopes).
fn parse_entries(source: &str) -> Result<Parsed, String> {
    let mut table: toml::Table =
        toml::from_str(source).map_err(|e| format!("invalid keybindings file: {e}"))?;
    let raw_vars = read_raw_vars(table.remove("vars"))?;
    let vars = resolve_vars(&raw_vars)?;
    let (key_order, mut var_order) = in_file_order(source);
    var_order.retain(|name| vars.contains_key(name));
    let mut lines: Vec<(String, BindingSpec)> = Vec::new();
    let mut entries: Vec<Entry> = Vec::new();
    // One combo has one binding per (when, focus) scope. TOML only refuses a
    // key written twice the same way: two spellings of one combo
    // ("shift+ctrl+a", "ctrl+shift+a") are caught here, like a scope repeated
    // in a combo's array. A literal key overrides one a variable or a template
    // produced — what the settings panel writes to rebind one of them.
    let mut seen: HashMap<BindingKey, (String, usize)> = HashMap::new();
    for (combo, value) in table {
        let specs: SpecOrList =
            value.try_into().map_err(|e| format!("invalid keybindings file: \"{combo}\": {e}"))?;
        for spec in specs.into_vec() {
            lines.push((combo.clone(), spec.clone()));
            for entry in expand(&combo, spec, &vars)? {
                let key = (entry.keys.clone(), entry.spec.when.clone(), entry.spec.focus.clone());
                let Some((other, index)) = seen.get(&key).cloned() else {
                    seen.insert(key, (combo.clone(), entries.len()));
                    entries.push(entry);
                    continue;
                };
                match (entries[index].derived_from.is_some(), entry.derived_from.is_some()) {
                    (true, false) => {
                        seen.insert(key, (combo.clone(), index));
                        entries[index] = entry;
                        continue;
                    }
                    (false, true) => continue,
                    _ => {}
                }
                let (first, second) =
                    if other <= combo { (&other, &combo) } else { (&combo, &other) };
                let scope = match (&entry.spec.focus, &entry.spec.when) {
                    (Some(focus), _) => format!("in the {focus} widget"),
                    (None, Some(when)) => format!("in {when}"),
                    (None, None) => "globally".to_string(),
                };
                return Err(if first == second {
                    format!("\"{first}\" is bound twice {scope}")
                } else {
                    format!("\"{first}\" and \"{second}\" are the same key, bound twice {scope}")
                });
            }
        }
    }
    // Table order is the file's or arbitrary: sort for determinism (by combo,
    // then scope).
    entries.sort_by(|a, b| {
        (&a.keys, &a.spec.when, &a.spec.focus).cmp(&(&b.keys, &b.spec.when, &b.spec.focus))
    });
    // The lines as the file has them, a key's array in its order.
    let position = |key: &str| key_order.iter().position(|k| k == key).unwrap_or(usize::MAX);
    lines.sort_by_key(|(key, _)| position(key));
    Ok(Parsed { entries, lines, raw_vars, vars, var_order })
}

/// Parses a keybindings TOML file into (normalized combo, spec) pairs,
/// variables and templates expanded (`parse_entries`).
pub fn parse_toml(source: &str) -> Result<Vec<(Vec<String>, BindingSpec)>, String> {
    Ok(parse_entries(source)?.entries.into_iter().map(|e| (e.keys, e.spec)).collect())
}

/// Whether a key of the file names a variable (`{name}`).
fn has_variables(source: &str) -> bool {
    !referenced(source).is_empty()
}

/// A key of the file as it is written back: a combo normalized
/// (`shift+ctrl+a` → `ctrl+shift+a`), one naming a variable with its words
/// separated by one space, the variables left as they are.
pub fn normalize_source(source: &str) -> Result<String, String> {
    if has_variables(source) {
        let words: Vec<&str> = source.split_whitespace().collect();
        Ok(words.join(" "))
    } else {
        Ok(parse_combo(source)?.join(" "))
    }
}

/// Whether two keys of the file are one, however each is spelled.
pub fn same_source(a: &str, b: &str) -> bool {
    match (normalize_source(a), normalize_source(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// One binding of a line of `keybindings.toml`: the keys, and what they run.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EntryBinding {
    pub keys: Vec<String>,
    pub invocation: String,
}

/// One line of `keybindings.toml` — one binding of a key's array — as it is
/// written, with the keys it binds (doc "keybindings panel"). A suggestion
/// (`addKeybinding`) is listed too, as a line the file does not have.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FileEntry {
    /// The key as the file writes it, variables left in: `{go} {list}`.
    pub source: String,
    /// The command as written, `{target}` left in.
    pub command: String,
    pub when: Option<String>,
    pub focus: Option<String>,
    pub text_input: bool,
    /// The default target of a `{target}` template.
    pub target: Option<String>,
    /// The keys it binds that are in effect — a literal line overrides one a
    /// template binds —, each with what it runs.
    pub bindings: Vec<EntryBinding>,
    pub suggested: bool,
}

/// Where a line of the file is: its key as written, and its scope.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct EntryPlace {
    pub source: String,
    pub when: Option<String>,
    pub focus: Option<String>,
}

/// A line as the keybindings panel's form holds it.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryDraft {
    pub source: String,
    pub command: String,
    pub when: Option<String>,
    pub focus: Option<String>,
    #[serde(default)]
    pub text_input: bool,
    pub target: Option<String>,
}

impl EntryDraft {
    fn spec(&self) -> BindingSpec {
        BindingSpec {
            command: self.command.trim().to_string(),
            when: self.when.clone(),
            text_input: self.text_input,
            focus: self.focus.clone(),
            target: self.target.clone(),
        }
    }
}

/// A variable of the file (doc "Keybinding variables").
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Variable {
    pub name: String,
    /// As written: it may name other variables (`{tags} -`).
    pub value: String,
    /// Its keys, the variables it names replaced.
    pub keys: String,
    /// How many lines of the file use it, directly or through another
    /// variable.
    pub uses: usize,
}

/// A binding's identity for override purposes: the same key sequence in the
/// same `when` and `focus` scope. So `down` in `metarecord-list`, `down` in
/// `file-manager` and `down` focus-scoped to the finder are all distinct, and a
/// user file overrides each independently.
type BindingKey = (Vec<String>, Option<String>, Option<String>);

/// The merged binding set.
#[derive(Debug)]
pub struct KeybindingSet {
    /// Keyed by (combo, scope); defaults overridden by the user per (combo,
    /// scope), so rebinding one panel's `down` leaves the others intact.
    config: HashMap<BindingKey, BindingSpec>,
    /// The bindings a variable or a target template produced, with the
    /// file's key they come from (`derived_from`).
    derived: HashMap<BindingKey, String>,
    /// The file's key every binding of `config` comes from, as written.
    origin: HashMap<BindingKey, String>,
    /// The lines of the file, as written.
    lines: Vec<(String, BindingSpec)>,
    raw_vars: HashMap<String, String>,
    vars: HashMap<String, String>,
    var_order: Vec<String>,
    /// Panel suggestions that survived conflict checking.
    suggestions: Vec<CompiledBinding>,
}

impl KeybindingSet {
    /// Merges the engine defaults with the user file (user wins per (combo,
    /// scope)).
    pub fn from_sources(defaults: &str, user: &str) -> Result<Self, String> {
        let mut config: HashMap<BindingKey, BindingSpec> = HashMap::new();
        let mut derived: HashMap<BindingKey, String> = HashMap::new();
        let mut origin: HashMap<BindingKey, String> = HashMap::new();
        let (mut lines, mut var_order) = (Vec::new(), Vec::new());
        let (mut raw_vars, mut vars) = (HashMap::new(), HashMap::new());
        for parsed in [parse_entries(defaults)?, parse_entries(user)?] {
            for entry in parsed.entries {
                let key = (entry.keys, entry.spec.when.clone(), entry.spec.focus.clone());
                match entry.derived_from {
                    Some(line) => derived.insert(key.clone(), line),
                    None => derived.remove(&key),
                };
                origin.insert(key.clone(), entry.source);
                config.insert(key, entry.spec);
            }
            lines.extend(parsed.lines);
            for name in parsed.var_order {
                if !var_order.contains(&name) {
                    var_order.push(name);
                }
            }
            raw_vars.extend(parsed.raw_vars);
            vars.extend(parsed.vars);
        }
        Ok(KeybindingSet {
            config,
            derived,
            origin,
            lines,
            raw_vars,
            vars,
            var_order,
            suggestions: Vec::new(),
        })
    }

    /// The complete set from a single source. In the git-backed config model
    /// (doc "Configuration") the user's `keybindings.toml` already is the shipped
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
        if self.config.contains_key(&(
            keys.clone(),
            when.map(str::to_string),
            focus.map(str::to_string),
        )) || self
            .suggestions
            .iter()
            .any(|s| s.keys == keys && s.when.as_deref() == when && s.focus.as_deref() == focus)
        {
            return Ok(()); // user/config binding wins; suggestion dropped
        }
        self.suggestions.push(CompiledBinding {
            keys,
            invocation: invocation.to_string(),
            when: when.map(str::to_string),
            text_input,
            focus: focus.map(str::to_string),
            suggested: true,
        });
        Ok(())
    }

    /// This set with the suggestions `previous` held, each applied under the
    /// usual rule (a configured binding of the same combo and scope wins). The
    /// file is read again when a binding is edited or the configuration
    /// reloaded; the suggestions are not in it, and the panels that made them
    /// are not mounted a second time.
    pub fn with_suggestions_of(mut self, previous: &KeybindingSet) -> Self {
        for suggestion in &previous.suggestions {
            let key = (suggestion.keys.clone(), suggestion.when.clone(), suggestion.focus.clone());
            if !self.config.contains_key(&key) {
                self.suggestions.push(suggestion.clone());
            }
        }
        self
    }

    /// Whether the binding of this combo and scope is a suggestion — in effect,
    /// but not in the file, so not something the file's editor can remove.
    pub fn is_suggestion(&self, combo: &str, when: Option<&str>, focus: Option<&str>) -> bool {
        parse_combo(combo).is_ok_and(|keys| {
            self.suggestions
                .iter()
                .any(|s| s.keys == keys && s.when.as_deref() == when && s.focus.as_deref() == focus)
        })
    }

    /// The lines of the file, in its order, then the suggestions — each with
    /// the keys it binds that are in effect (doc "keybindings panel").
    pub fn entries(&self) -> Vec<FileEntry> {
        let mut out: Vec<FileEntry> = self
            .lines
            .iter()
            .map(|(source, spec)| {
                let bindings = expand(source, spec.clone(), &self.vars)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|e| {
                        let key = (e.keys.clone(), e.spec.when.clone(), e.spec.focus.clone());
                        self.origin.get(&key) == Some(source)
                    })
                    .map(|e| EntryBinding { keys: e.keys, invocation: e.spec.command })
                    .collect();
                FileEntry {
                    source: source.clone(),
                    command: spec.command.clone(),
                    when: spec.when.clone(),
                    focus: spec.focus.clone(),
                    text_input: spec.text_input,
                    target: spec.target.clone(),
                    bindings,
                    suggested: false,
                }
            })
            .collect();
        out.extend(self.suggestions.iter().map(|s| FileEntry {
            source: s.keys.join(" "),
            command: s.invocation.clone(),
            when: s.when.clone(),
            focus: s.focus.clone(),
            text_input: s.text_input,
            target: None,
            bindings: vec![EntryBinding { keys: s.keys.clone(), invocation: s.invocation.clone() }],
            suggested: true,
        }));
        out
    }

    /// The variables, in the order of the file, with the number of lines
    /// using each: `{name}` in their key, directly or through another
    /// variable, or — for a target prefix — a `{target}` template.
    pub fn variables(&self) -> Vec<Variable> {
        // Every variable a name stands for, itself included.
        let reaches = |start: &str| {
            let mut seen = vec![start.to_string()];
            let mut i = 0;
            while i < seen.len() {
                let value = self.raw_vars.get(&seen[i]).cloned().unwrap_or_default();
                for other in referenced(&value) {
                    if !seen.contains(&other) {
                        seen.push(other);
                    }
                }
                i += 1;
            }
            seen
        };
        self.var_order
            .iter()
            .map(|name| {
                let prefix = name.starts_with("target-");
                let uses = self
                    .lines
                    .iter()
                    .filter(|(source, spec)| {
                        referenced(source).iter().any(|v| reaches(v).contains(name))
                            || (prefix && spec.command.contains("{target}"))
                    })
                    .count();
                Variable {
                    name: name.clone(),
                    value: self.raw_vars[name].clone(),
                    keys: self.vars[name].clone(),
                    uses,
                }
            })
            .collect()
    }

    /// Whether this binding comes from the line at `place`.
    fn comes_from(&self, binding: &CompiledBinding, place: &EntryPlace) -> bool {
        binding.when == place.when
            && binding.focus == place.focus
            && self
                .origin
                .get(&(binding.keys.clone(), binding.when.clone(), binding.focus.clone()))
                .is_some_and(|source| same_source(source, &place.source))
    }

    /// What would refuse saving the line `draft` (`check_command`, and
    /// `check_binding` for every key it binds), `replacing` the line being
    /// edited, whose own keys are no clash. A literal key over one a template
    /// binds overrides it, and the other way round — the file's rule —, which
    /// the check's note says. A suggestion can be overridden in its own place
    /// but not moved: it is not in the file, and its panel would suggest it
    /// again.
    pub fn check_entry(
        &self,
        known: impl Fn(&str) -> bool,
        draft: &EntryDraft,
        replacing: Option<&EntryPlace>,
    ) -> BindingCheck {
        let command = check_command(&draft.command, known).err();
        match self.check_entry_keys(draft, replacing) {
            Ok(note) => BindingCheck { command, note, ..Default::default() },
            Err((key, in_the_way)) => {
                BindingCheck { command, key: Some(key), in_the_way, note: None }
            }
        }
    }

    fn check_entry_keys(
        &self,
        draft: &EntryDraft,
        replacing: Option<&EntryPlace>,
    ) -> Result<Option<String>, (String, Option<KeyPlace>)> {
        let source = normalize_source(&draft.source).map_err(|e| (e, None))?;
        if let Some(old) = replacing {
            let moved = !same_source(&old.source, &source)
                || old.when != draft.when
                || old.focus != draft.focus;
            let suggested =
                self.is_suggestion(&old.source, old.when.as_deref(), old.focus.as_deref())
                    && !self.lines.iter().any(|(s, spec)| {
                        same_source(s, &old.source)
                            && spec.when == old.when
                            && spec.focus == old.focus
                    });
            if moved && suggested {
                return Err((
                    format!(
                        "{} is suggested by a panel or by commands.js, not set in \
                         keybindings.toml: it cannot be moved, only overridden in its own place",
                        old.source
                    ),
                    None,
                ));
            }
        }
        let table: Vec<CompiledBinding> = self
            .compiled()
            .into_iter()
            .filter(|b| !replacing.is_some_and(|old| self.comes_from(b, old)))
            .collect();
        let mut notes = Vec::new();
        for entry in expand(&source, draft.spec(), &self.vars).map_err(|e| (e, None))? {
            let (when, focus) = (entry.spec.when.clone(), entry.spec.focus.clone());
            let key = (entry.keys.clone(), when.clone(), focus.clone());
            let combo = entry.keys.join(" ");
            // The same key in the same place, written once literally and once
            // by a template (or suggested): the literal line wins, no clash.
            let same =
                table.iter().find(|b| b.keys == entry.keys && b.when == when && b.focus == focus);
            let overridden = same.filter(|b| {
                b.suggested || self.derived.contains_key(&key) != entry.derived_from.is_some()
            });
            if let Some(other) = overridden {
                let line = self.origin.get(&key).cloned().unwrap_or_default();
                notes.push(match (other.suggested, entry.derived_from.is_some()) {
                    (true, _) => format!("Overrides the suggested `{combo}`."),
                    (false, false) => {
                        format!("Overrides `{combo}`, which the line \"{line}\" binds.")
                    }
                    (false, true) => format!("`{combo}` stays the line \"{line}\"'s."),
                });
            }
            let others: Vec<CompiledBinding> = table
                .iter()
                .filter(|b| !overridden.is_some_and(|o| std::ptr::eq(*b, o)))
                .cloned()
                .collect();
            binding_in_the_way(&others, &BindingPlace { combo, when, focus })?;
        }
        Ok((!notes.is_empty()).then(|| notes.join(" ")))
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
                suggested: false,
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
"ctrl+t"  = { command = "workspace:new"                                                  }
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
        assert_eq!(ctrl_t.command, "workspace:new");
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
"t" = { command = "workspace:new" }
"#;
        let entries = parse_toml(source).unwrap();
        // 2 (j) + 2 (enter) + 1 (t)
        assert_eq!(entries.len(), 5);

        let js: Vec<&BindingSpec> =
            entries.iter().filter(|(k, _)| k == &vec!["j".to_string()]).map(|(_, s)| s).collect();
        assert_eq!(js.len(), 2);
        assert!(js
            .iter()
            .any(|s| s.command == "metarecord-list:next"
                && s.when.as_deref() == Some("metarecord-list")));
        assert!(
            js.iter()
                .any(|s| s.command == "file-manager:next"
                    && s.when.as_deref() == Some("file-manager"))
        );

        // The single-table form still works alongside arrays.
        assert!(entries
            .iter()
            .any(|(k, s)| k == &vec!["t".to_string()] && s.command == "workspace:new"));
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
        let user =
            r#""down" = { command = "metarecord-list:apply finder stay", focus = "finder" }"#;
        let set = KeybindingSet::from_sources(defaults, user).unwrap();
        let downs: Vec<_> = set.compiled().into_iter().filter(|b| b.keys == ["down"]).collect();
        assert_eq!(downs.len(), 2);
        assert_eq!(
            downs.iter().find(|b| b.focus.as_deref() == Some("finder")).unwrap().invocation,
            "metarecord-list:apply finder stay"
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
        let defaults = r#""ctrl+t" = { command = "workspace:new" }
"alt+w" = { command = "workspace:close" }"#;
        let user = r#""ctrl+t" = { command = "panel:split" }"#;
        let set = KeybindingSet::from_sources(defaults, user).unwrap();
        let table = set.compiled();

        let ctrl_t = table.iter().find(|b| b.keys == ["ctrl+t"]).unwrap();
        assert_eq!(ctrl_t.invocation, "panel:split");
        // Untouched default survives.
        let alt_w = table.iter().find(|b| b.keys == ["alt+w"]).unwrap();
        assert_eq!(alt_w.invocation, "workspace:close");
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
    fn test_one_combo_spelled_twice_in_the_same_scope_is_refused() {
        // TOML sees two keys; they are one combo, and which binding won used
        // to depend on the order of a hash table.
        let source = r#"
"shift+ctrl+a" = { command = "first" }
"ctrl+shift+a" = { command = "second" }
"#;
        let error = parse_toml(source).unwrap_err();
        assert!(error.contains("shift+ctrl+a") && error.contains("ctrl+shift+a"), "{error}");
        assert!(KeybindingSet::from_source(source).is_err());

        // The same scope twice under one key is the same mistake.
        let twice = r#""j" = [
  { command = "a", when = "log" },
  { command = "b", when = "log" },
]"#;
        assert!(parse_toml(twice).is_err());

        // Two spellings in different scopes are two bindings.
        let scoped = r#"
"shift+ctrl+a" = { command = "first", when = "log" }
"ctrl+shift+a" = { command = "second" }
"#;
        assert_eq!(parse_toml(scoped).unwrap().len(), 2);
    }

    // ── Variables and target templates (doc "Keybinding variables") ──────

    fn keys_of(entries: &[(Vec<String>, BindingSpec)]) -> Vec<(String, String)> {
        entries.iter().map(|(k, spec)| (k.join(" "), spec.command.clone())).collect()
    }

    #[test]
    fn test_a_variable_names_a_key_once() {
        let source = r#"
vars.tags = ","
vars.add = "a"
"{tags} {add}" = { command = "mf:tag add" }
"shift+{add}" = { command = "other" }
"#;
        let entries = parse_toml(source).unwrap();
        assert_eq!(
            keys_of(&entries),
            [
                (", a".to_string(), "mf:tag add".to_string()),
                ("shift+a".to_string(), "other".to_string())
            ]
        );
    }

    #[test]
    fn test_a_variable_may_hold_a_sequence_and_vars_may_be_a_table() {
        let source = r#"
"{bulk-query} d" = { command = "metarecord:bulk query delete" }

[vars]
bulk-query = "m q"
"#;
        let entries = parse_toml(source).unwrap();
        assert_eq!(entries[0].0, ["m", "q", "d"]);
    }

    #[test]
    fn test_a_variable_may_be_built_on_another() {
        let source = r#"
vars.mixed = "{tags} ="
vars.tags = ","
"{mixed} a" = { command = "x" }
"#;
        assert_eq!(keys_of(&parse_toml(source).unwrap()), [(", = a".into(), "x".into())]);
        let set = KeybindingSet::from_source(source).unwrap();
        let tags = set.variables().into_iter().find(|v| v.name == "tags").unwrap();
        // Used through `mixed`.
        assert_eq!(tags.uses, 1);
        let mixed = set.variables().into_iter().find(|v| v.name == "mixed").unwrap();
        assert_eq!((mixed.value.as_str(), mixed.keys.as_str()), ("{tags} =", ", ="));

        let cycle = "vars.a = \"{b} x\"\nvars.b = \"{a}\"\n";
        assert!(parse_toml(cycle).unwrap_err().contains("themselves"));
        assert!(parse_toml("vars.a = \"{far}\"\n").unwrap_err().contains("far"));
    }

    #[test]
    fn test_an_unknown_or_malformed_variable_is_refused() {
        let error = parse_toml(r#""{nope} a" = { command = "x" }"#).unwrap_err();
        assert!(error.contains("{nope}"), "{error}");
        assert!(parse_toml("vars.a = 1\n").is_err());
        assert!(parse_toml("vars = \"a\"\n").is_err());
        // A brace that names no variable is a key like any other.
        assert_eq!(parse_toml(r#""{" = { command = "x" }"#).unwrap()[0].0, ["{"]);
    }

    const TARGET_VARS: &str = r#"
vars.target-here = "i"
vars.target-other = "b"
vars.target-new = "n"
"#;

    #[test]
    fn test_a_target_template_expands_to_its_default_and_three_prefixed_variants() {
        let source = format!(
            "{TARGET_VARS}\"t l\" = {{ command = \"panel:open {{target}} metarecord-list\", \
             target = \"other\", when = \"log\" }}\n"
        );
        let entries = parse_toml(&source).unwrap();
        assert_eq!(
            keys_of(&entries),
            [
                ("b t l".to_string(), "panel:open other metarecord-list".to_string()),
                ("i t l".to_string(), "panel:open here metarecord-list".to_string()),
                ("n t l".to_string(), "panel:open new metarecord-list".to_string()),
                ("t l".to_string(), "panel:open other metarecord-list".to_string()),
            ]
        );
        assert!(entries.iter().all(|(_, spec)| spec.when.as_deref() == Some("log")));
    }

    #[test]
    fn test_a_target_template_must_be_complete() {
        let missing_default = format!("{TARGET_VARS}\"o\" = {{ command = \"x {{target}}\" }}\n");
        assert!(parse_toml(&missing_default).unwrap_err().contains("target"));
        let no_placeholder =
            format!("{TARGET_VARS}\"o\" = {{ command = \"x\", target = \"here\" }}\n");
        assert!(parse_toml(&no_placeholder).is_err());
        let unknown =
            format!("{TARGET_VARS}\"o\" = {{ command = \"x {{target}}\", target = \"far\" }}\n");
        assert!(parse_toml(&unknown).unwrap_err().contains("far"));
        let no_prefix = r#""o" = { command = "x {target}", target = "here" }"#;
        assert!(parse_toml(no_prefix).unwrap_err().contains("target-here"));
    }

    #[test]
    fn test_a_literal_key_overrides_one_derived_from_a_variable() {
        // What the settings panel writes when one key of a template is
        // rebound: a literal line, which wins over the template's.
        let source = r#"
vars.tags = ","
"{tags} a" = { command = "template" }
", a" = { command = "literal" }
"#;
        assert_eq!(keys_of(&parse_toml(source).unwrap()), [(", a".into(), "literal".into())]);
        // Two templates landing on one key are a mistake, like two literals.
        let clash = r#"
vars.x = ","
vars.y = ","
"{x} a" = { command = "first" }
"{y} a" = { command = "second" }
"#;
        assert!(parse_toml(clash).is_err());
    }

    /// The lines of the file, for the keybindings panel (doc "keybindings
    /// panel"): each as written, with the keys it binds that are in effect.
    fn lines_source() -> String {
        format!(
            "{TARGET_VARS}vars.tags = \",\"\n\"{{tags}} a\" = {{ command = \"x\" }}\n\
             \"o\" = {{ command = \"y {{target}}\", target = \"here\" }}\n\
             \"b o\" = {{ command = \"mine\" }}\n\"z\" = {{ command = \"z\", when = \"log\" }}\n"
        )
    }

    fn draft(source: &str, command: &str) -> EntryDraft {
        EntryDraft {
            source: source.into(),
            command: command.into(),
            when: None,
            focus: None,
            text_input: false,
            target: None,
        }
    }

    fn line(source: &str) -> EntryPlace {
        EntryPlace { source: source.into(), when: None, focus: None }
    }

    #[test]
    fn test_entries_are_the_lines_of_the_file_with_the_keys_they_bind() {
        let mut set = KeybindingSet::from_source(&lines_source()).unwrap();
        set.add_suggestion("ctrl+y", "my-panel:go", Some("my-panel"), false, None).unwrap();
        let entries = set.entries();
        let of = |source: &str| entries.iter().find(|e| e.source == source).unwrap();
        let keys = |e: &FileEntry| {
            e.bindings
                .iter()
                .map(|b| format!("{} → {}", b.keys.join(" "), b.invocation))
                .collect::<Vec<_>>()
        };

        // In the order of the file, then the suggestions.
        let sources: Vec<&str> = entries.iter().map(|e| e.source.as_str()).collect();
        assert_eq!(sources, ["{tags} a", "o", "b o", "z", "ctrl+y"]);
        assert_eq!(keys(of("{tags} a")), [", a → x"]);
        // A template binds four keys, but `b o` is the literal line's.
        let template = of("o");
        assert_eq!(template.command, "y {target}");
        assert_eq!(template.target.as_deref(), Some("here"));
        assert_eq!(keys(template), ["o → y here", "i o → y here", "n o → y new"]);
        assert_eq!(keys(of("b o")), ["b o → mine"]);
        assert_eq!(of("z").when.as_deref(), Some("log"));
        assert!(entries.iter().all(|e| !e.suggested || e.source == "ctrl+y"));
        assert!(of("ctrl+y").suggested);
        assert_eq!(keys(of("ctrl+y")), ["ctrl+y → my-panel:go"]);
    }

    #[test]
    fn test_the_variables_come_in_the_order_of_the_file_with_their_uses() {
        let set = KeybindingSet::from_source(&lines_source()).unwrap();
        let variables = set.variables();
        let names: Vec<&str> = variables.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["target-here", "target-other", "target-new", "tags"]);
        let uses = |name: &str| variables.iter().find(|v| v.name == name).unwrap().uses;
        assert_eq!(uses("tags"), 1);
        // A target prefix is used by every template.
        assert_eq!(uses("target-new"), 1);
        assert_eq!(variables[3].keys, ",");
    }

    #[test]
    fn test_a_line_is_checked_against_the_others_and_not_itself() {
        let set = KeybindingSet::from_source(&lines_source()).unwrap();
        let known = |name: &str| name != "nothing";
        let template = EntryDraft { target: Some("here".into()), ..draft("o", "y {target}") };

        // Saved again as it is: no clash with its own keys, nor with the
        // literal line that overrides one of them.
        let check = set.check_entry(known, &template, Some(&line("o")));
        assert_eq!((check.command.as_deref(), check.key.as_deref()), (None, None), "{check:?}");
        // A literal key over one a template binds overrides it, and says so.
        let check = set.check_entry(known, &draft("i o", "other"), None);
        assert_eq!(check.key, None);
        assert!(check.note.as_deref().unwrap_or("").contains("\"o\""), "{check:?}");
        // Two literal lines on one key clash; a key alone hides the sequences
        // it starts.
        let check = set.check_entry(known, &draft("b o", "other"), None);
        assert!(check.key.unwrap().contains("already bound"));
        // The binding in the way is named, for the panel to mark its row.
        assert_eq!(check.in_the_way.unwrap().keys, ["b", "o"]);
        let check = set.check_entry(known, &draft("{tags}", "other"), None);
        assert!(check.key.unwrap().contains("would hide"));
        // Variables are read; an unknown one is named.
        let check = set.check_entry(known, &draft("{tags} q", "other"), None);
        assert_eq!(check.key, None);
        let check = set.check_entry(known, &draft("{far} q", "other"), None);
        assert!(check.key.unwrap().contains("far"));
        let check = set.check_entry(known, &draft("q", "nothing"), None);
        assert!(check.command.unwrap().contains("nothing"));
        // Another scope is another place.
        let z = EntryDraft { when: Some("log".into()), ..draft("z", "other") };
        assert!(set.check_entry(known, &z, None).key.is_some());
        assert!(set
            .check_entry(known, &z, Some(&EntryPlace { when: Some("log".into()), ..line("z") }))
            .key
            .is_none());
    }

    #[test]
    fn test_a_suggestion_can_be_overridden_in_its_place_but_not_moved() {
        let mut set = KeybindingSet::from_source("").unwrap();
        set.add_suggestion("ctrl+y", "my-panel:go", None, false, None).unwrap();
        let known = |_: &str| true;
        let check = set.check_entry(known, &draft("ctrl+y", "mine"), Some(&line("ctrl+y")));
        assert_eq!(check.key, None);
        let check = set.check_entry(known, &draft("ctrl+u", "mine"), Some(&line("ctrl+y")));
        assert!(check.key.unwrap().contains("suggested"));
    }

    #[test]
    fn test_sequence_clashes_name_the_key_that_hides_another() {
        let table = KeybindingSet::from_source(
            "\"g\" = { command = \"a\" }\n\"g g\" = { command = \"b\" }\n\
             \"h\" = { command = \"c\", when = \"log\" }\n\"h h\" = { command = \"d\" }\n",
        )
        .unwrap()
        .compiled();
        let clashes = sequence_clashes(&table);
        assert_eq!(clashes.len(), 1, "{clashes:?}");
        assert!(clashes[0].contains("`g`") && clashes[0].contains("`g g`"), "{clashes:?}");
        // The shipped keys have none.
        assert!(sequence_clashes(&crate::doc_gen::default_bindings()).is_empty());
    }

    #[test]
    fn test_a_key_of_the_file_is_the_same_however_it_is_spelled() {
        assert!(same_source("shift+ctrl+a", "ctrl+shift+a"));
        assert!(same_source("{go}  l", "{go} l"));
        assert!(!same_source("{go} l", "g l"));
        assert_eq!(normalize_source("Shift+Ctrl+A  g").unwrap(), "ctrl+shift+a g");
        assert_eq!(normalize_source(" {go}   shift+{add} ").unwrap(), "{go} shift+{add}");
        // A brace that names no variable is a key.
        assert_eq!(normalize_source("{").unwrap(), "{");
    }

    #[test]
    fn test_a_suggestion_is_told_apart_from_a_binding_of_the_file() {
        let mut set = KeybindingSet::from_source(r#""x" = { command = "mine" }"#).unwrap();
        set.add_suggestion("shift+ctrl+y", "my-panel:go", Some("my-panel"), false, None).unwrap();
        assert!(set.is_suggestion("ctrl+shift+Y", Some("my-panel"), None));
        assert!(!set.is_suggestion("ctrl+shift+y", None, None));
        assert!(!set.is_suggestion("x", None, None));
    }

    #[test]
    fn test_suggestions_survive_a_reread_of_the_file() {
        // Editing a binding or `config:reload keybindings` reads the file
        // again; what the panels suggested is not in it and must be kept.
        let mut set = KeybindingSet::from_source("").unwrap();
        set.add_suggestion("x", "my-panel:go", Some("my-panel"), false, None).unwrap();
        set.add_suggestion("y", "my-panel:back", Some("my-panel"), false, None).unwrap();

        let reread = KeybindingSet::from_source(r#""x" = { command = "mine", when = "my-panel" }"#)
            .unwrap()
            .with_suggestions_of(&set);
        let table = reread.compiled();
        // The file now binds `x` in that scope: its suggestion gives way.
        let x: Vec<_> = table.iter().filter(|b| b.keys == ["x"]).collect();
        assert_eq!(x.len(), 1);
        assert_eq!(x[0].invocation, "mine");
        assert!(table.iter().any(|b| b.keys == ["y"] && b.invocation == "my-panel:back"));
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
        assert!(table.iter().any(|b| b.invocation == "workspace:new"));
        assert!(table.iter().any(|b| b.keys == [":"]));
        // `x` is the command input (doc "Choosing keybindings"), `:` its alias.
        assert!(table
            .iter()
            .any(|b| b.keys == ["x"] && b.invocation == "command-input:focus command"));
        assert!(table.iter().any(|b| b.keys == ["v", "x"] && b.invocation == "panel:swap"));

        // Moving a cursor is one generic command for every panel (doc "Generic
        // panel commands"): one global `down`, plus the panels it drives from
        // elsewhere — the viewer panel and the finder drive the list's cursor.
        let down: Vec<_> = table.iter().filter(|b| b.keys == ["down"]).collect();
        assert!(down
            .iter()
            .any(|b| b.when.is_none() && b.focus.is_none() && b.invocation == "cursor:next self"));
        assert!(down.iter().any(|b| b.when.as_deref() == Some("viewer")
            && b.invocation == "cursor:next metarecord-list"));
        // `enter` mixes a global text-input binding with per-panel actions.
        let enter: Vec<_> = table.iter().filter(|b| b.keys == ["enter"]).collect();
        assert!(enter.iter().any(|b| b.when.is_none() && b.text_input));
        assert!(enter.iter().any(
            |b| b.when.as_deref() == Some("path-ref") && b.invocation == "cursor:descend self"
        ));

        // The finder's in-input shortcuts are focus-scoped (doc "Keybindings").
        assert!(down.iter().any(|b| b.focus.as_deref() == Some("finder")));
        assert!(table.iter().any(|b| b.keys == ["ctrl+enter"]
            && b.focus.as_deref() == Some("finder")
            && b.invocation == "pick:confirm"));

        // Find in panel (doc "Find in a panel"): ctrl+f opens it from
        // anywhere, text input included, and the stepping keys are scoped to
        // the find bar's own focus so they never shadow a panel's Enter.
        assert!(table
            .iter()
            .any(|b| b.keys == ["ctrl+f"] && b.invocation == "find:open" && b.text_input));
        assert!(enter
            .iter()
            .any(|b| b.focus.as_deref() == Some("find") && b.invocation == "find:next"));
        assert!(table.iter().any(|b| b.keys == ["shift+enter"]
            && b.focus.as_deref() == Some("find")
            && b.invocation == "find:prev"));
        // Shift+Enter shows a tree node in the detail panel, as it does a row.
        assert!(table.iter().any(|b| b.keys == ["shift+enter"]
            && b.when.as_deref() == Some("path-ref")
            && b.invocation == "panel:open other metarecord-detail"));
        assert!(table.iter().any(|b| b.keys == ["escape"]
            && b.focus.as_deref() == Some("find")
            && b.invocation == "find:close"));
        // The find commands the bindings name must exist as builtins, or the
        // keys would dispatch into nothing.
        let registry = crate::command_registry::CommandRegistry::default();
        crate::register_builtins(&registry);
        for name in ["find:open", "find:next", "find:prev", "find:close"] {
            assert!(registry.list().iter().any(|c| c.name == name), "missing builtin {name}");
        }
    }

    #[test]
    fn test_a_chord_has_the_level_of_what_it_costs_to_type() {
        for (chord, level) in [
            ("x", Some(0)),
            (",", Some(1)),
            ("f6", Some(1)),
            ("delete", Some(1)),
            ("shift+x", Some(2)),
            ("shift+delete", Some(2)),
            // Shift on bépo and AZERTY: a level of its own, alone only.
            ("1", Some(2)),
            (":", Some(2)),
            ("ctrl+x", Some(2)),
            ("alt+up", Some(2)),
            ("ctrl+-", Some(2)),
            ("ctrl+alt+x", Some(2)),
            ("ctrl+shift+a", Some(3)),
            ("alt+shift+up", Some(3)),
            ("ctrl+alt+shift+x", Some(3)),
            ("ctrl+.", None),
            ("shift+.", None),
            ("[", None),
            ("é", None),
            ("f13", None),
        ] {
            assert_eq!(key_level(chord), level, "{chord}");
        }
    }

    /// The rules the shipped keys follow (doc "Choosing keybindings"), on the
    /// file as shipped, variables expanded.
    #[test]
    fn test_shipped_keys_follow_the_rules_of_choice() {
        let defaults = include_str!("../default-config/keybindings.toml");
        let table = KeybindingSet::from_source(defaults).unwrap().compiled();
        for binding in &table {
            for chord in &binding.keys {
                assert!(
                    key_level(chord).is_some(),
                    "{chord} (in {:?} → {}) is not typed alike on bépo, AZERTY and QWERTY",
                    binding.keys,
                    binding.invocation
                );
            }
        }

        // A single letter is for what is done all the time; every other one
        // starts a domain, so that no letter is spent on a verb alone.
        const CONSTANT: [&str; 6] = ["x", "shift+x", "o", "f", "shift+f", "space"];
        for binding in table.iter().filter(|b| b.keys.len() == 1) {
            let key = binding.keys[0].as_str();
            let bare = key.trim_start_matches("shift+");
            if bare.len() == 1 && bare.chars().all(|c| c.is_ascii_lowercase()) {
                assert!(
                    CONSTANT.contains(&key),
                    "{key} runs {} alone: a letter alone is for the commands of every moment",
                    binding.invocation
                );
            }
        }

        // What cannot be undone has no key.
        for irreversible in ["trash:prune all", "log:prune", "orphan:delete", "trash:empty"] {
            assert!(
                table.iter().all(|b| b.invocation != irreversible),
                "{irreversible} is bound: what cannot be undone has no key"
            );
        }
    }

    /// Both ways into the command input are reserved from scripts.
    ///
    /// They used to be two commands (`command-input:activate` and
    /// `bash-input:activate`) and are now one with the mode as its argument,
    /// which is exactly the shape that can silently narrow a reservation: a
    /// list naming `command-input:focus command` would leave `!` takeable
    /// while looking correct.
    #[test]
    fn test_both_command_input_modes_are_reserved_from_scripts() {
        let defaults = include_str!("../default-config/keybindings.toml");
        let compiled = KeybindingSet::from_sources(defaults, "").unwrap().compiled();
        let reserved = reserved_combos(&compiled);

        for mode in ["command", "bash"] {
            let combo = compiled
                .iter()
                .find(|b| {
                    b.invocation == format!("command-input:focus {mode}") && b.keys.len() == 1
                })
                .map(|b| b.keys[0].clone())
                .unwrap_or_else(|| panic!("nothing opens the input in {mode} mode"));
            assert!(
                is_reserved(&reserved, &combo),
                "`{combo}` opens the input in {mode} mode but a script could take it"
            );
        }
    }

    /// Every command a shipped keybinding names must exist.
    ///
    /// The registry is filled from three places that cannot see each other: the
    /// builtins, listed in Rust; the panel commands, registered by each panel
    /// type's JavaScript at runtime; and the user commands the shipped
    /// `commands.js` defines (registered when the GUI boots). A keybinding
    /// naming none of them is dead — the key does nothing and says nothing —
    /// and nothing else in the build catches it, because the halves are only
    /// ever brought together in a running GUI. So this test reads the config
    /// sources the way the shell would, and checks the shipped table against
    /// the union.
    ///
    /// It is the guard for a mass rename: an invocation left on an old name
    /// fails here instead of silently going dead in someone's GUI.
    #[test]
    fn test_every_shipped_binding_names_an_existing_command() {
        // The three sources, read the way the shell would (`crate::doc_gen`,
        // which also feeds the documentation wiki). The scan is a superset at
        // worst: an extra name only weakens this check, a missing one fails it.
        let commands = crate::doc_gen::commands();
        let panels = commands
            .iter()
            .filter_map(|c| match &c.owner {
                crate::doc_gen::Owner::Panel(p) => Some(p.as_str()),
                _ => None,
            })
            .collect::<std::collections::HashSet<_>>()
            .len();
        assert!(panels >= 12, "expected the shipped panel types, found {panels}");
        let known: std::collections::HashSet<String> =
            commands.into_iter().map(|c| c.name).collect();

        let defaults = include_str!("../default-config/keybindings.toml");
        let table = KeybindingSet::from_sources(defaults, "").unwrap().compiled();
        assert!(table.len() > 100, "the shipped table looks truncated: {}", table.len());

        // A binding may pre-fill arguments, so only the first token is a name.
        let mut dead: Vec<String> = table
            .iter()
            .map(|b| b.invocation.split_whitespace().next().unwrap_or("").to_string())
            .filter(|name| !name.is_empty() && !known.contains(name))
            .collect();
        dead.sort();
        dead.dedup();
        assert!(dead.is_empty(), "keybindings name commands that do not exist: {dead:?}");
    }

    /// The tag family sits under `,`, a key typed without a modifier on both
    /// AZERTY and QWERTY — `#`, the tag sigil, needs AltGr or Shift.
    #[test]
    fn test_shipped_defaults_put_the_tag_family_under_comma() {
        let defaults = include_str!("../default-config/keybindings.toml");
        let table = KeybindingSet::from_sources(defaults, "").unwrap().compiled();
        let tags: Vec<_> = table.iter().filter(|b| b.invocation.starts_with("tag:")).collect();
        assert_eq!(tags.len(), 9, "the tag family: {tags:?}");
        for b in &tags {
            assert_eq!(b.keys[0], ",", "{:?} → {}", b.keys, b.invocation);
        }
        // The plain tag gets the verbs directly, the other two fields a noun
        // each (`, -` negative, `, =` mixed) followed by the verb.
        for (keys, invocation) in [
            (vec![",", "a"], "tag:add tag"),
            (vec![",", "r"], "tag:remove tag"),
            (vec![",", "q"], "tag:query tag"),
            (vec![",", "-", "a"], "tag:add negative_tag"),
            (vec![",", "-", "r"], "tag:remove negative_tag"),
            (vec![",", "-", "q"], "tag:query negative_tag"),
            (vec![",", "=", "a"], "tag:add mixed_tag"),
            (vec![",", "=", "r"], "tag:remove mixed_tag"),
            (vec![",", "=", "q"], "tag:query mixed_tag"),
        ] {
            let found: Vec<_> = tags.iter().filter(|b| b.keys == keys).collect();
            assert_eq!(found.len(), 1, "{keys:?}: {found:?}");
            assert_eq!(found[0].invocation, invocation, "{keys:?}");
        }
        assert!(table.iter().all(|b| b.keys[0] != "#"));
    }

    /// The field-editing families are one command each, with the operation as
    /// the first argument; every `m` binding is that command with the
    /// operation pre-filled. The listing expands exactly these, so a binding
    /// that names no operation would also hide that operation from the
    /// command input (doc "The command input").
    #[test]
    fn test_shipped_defaults_prefill_the_field_operations() {
        let defaults = include_str!("../default-config/keybindings.toml");
        let table = KeybindingSet::from_sources(defaults, "").unwrap().compiled();
        let bound = |keys: &[&str]| -> Vec<String> {
            table
                .iter()
                .filter(|b| b.keys == keys)
                .map(|b| b.invocation.clone())
                .collect::<Vec<_>>()
        };

        for (keys, invocation) in [
            (["m", "s"], "metarecord:field set"),
            (["m", "a"], "metarecord:field add"),
            (["m", "e"], "metarecord:field edit"),
            (["m", "r"], "metarecord:field remove"),
            (["m", "u"], "metarecord:field unset"),
            (["m", "n"], "metarecord:field rename"),
            (["m", "t"], "metarecord:field retype"),
        ] {
            let found = bound(&keys);
            assert!(
                found.iter().all(|i| i == invocation) && !found.is_empty(),
                "{keys:?} should run `{invocation}`, got {found:?}"
            );
            // Reachable wherever a metarecord is seen or selected — the file
            // panel, the file manager, a tree — not only from the list and
            // the detail: a command dispatches to its own panel whatever is
            // focused, and acts on the workspace's selected metarecord.
            let scopes: Vec<_> =
                table.iter().filter(|b| b.keys == keys).map(|b| b.when.clone()).collect();
            assert_eq!(scopes, vec![None], "{keys:?} should be bound in every panel");
        }

        // `m m <op>` and shift+m <op> name the checked metarecords, `m q <op>`
        // what the list's query matches: the prefixes must stay free of an
        // exact binding.
        assert!(bound(&["m", "m"]).is_empty(), "an exact `m m` would shadow every `m m <op>`");
        assert!(bound(&["m", "q"]).is_empty(), "an exact `m q` would shadow every `m q <op>`");
        for (keys, invocation) in [
            (vec!["m", "m", "s"], "metarecord:bulk selection set"),
            (vec!["shift+m", "s"], "metarecord:bulk selection set"),
            (vec!["m", "m", "a"], "metarecord:bulk selection add"),
            (vec!["m", "m", "r"], "metarecord:bulk selection remove"),
            (vec!["m", "m", "u"], "metarecord:bulk selection unset"),
            (vec!["m", "m", "d"], "metarecord:bulk selection delete"),
            (vec!["m", "q", "s"], "metarecord:bulk query set"),
            (vec!["m", "q", "a"], "metarecord:bulk query add"),
            (vec!["m", "q", "r"], "metarecord:bulk query remove"),
            (vec!["m", "q", "u"], "metarecord:bulk query unset"),
            (vec!["m", "q", "d"], "metarecord:bulk query delete"),
        ] {
            let keys: Vec<&str> = keys;
            let found = bound(&keys);
            assert!(
                found.iter().all(|i| i == invocation) && !found.is_empty(),
                "{keys:?} should run `{invocation}`, got {found:?}"
            );
        }
    }

    #[test]
    fn test_shipped_defaults_bind_find_per_panel_and_fullscreen() {
        // doc "Jumping to an entry": every list panel with a row cursor puts
        // its find on the same key, so the gesture is one gesture — one
        // generic command, run wherever the focused panel can find a row
        // (doc "Generic panel commands").
        let defaults = include_str!("../default-config/keybindings.toml");
        let table = KeybindingSet::from_sources(defaults, "").unwrap().compiled();
        let f: Vec<_> = table.iter().filter(|b| b.keys == ["f"]).collect();
        assert!(f.iter().any(|b| b.when.is_none() && b.invocation == "cursor:find self select"));
        // shift+f walks down by name where a row can be entered (a directory,
        // a node with children): asked again from inside.
        assert!(table.iter().any(|b| b.keys == ["shift+f"]
            && b.when.is_none()
            && b.invocation == "cursor:find self walk"));
        // The path-ref field picker is a view setting.
        assert!(table.iter().any(|b| b.keys == ["v", "t"] && b.invocation == "path-ref:set field"));
        assert!(!f.iter().any(|b| b.invocation == "path-ref:set field"));

        // Refreshing is F5 and ctrl+r, as everywhere else; `r` is free.
        for key in ["f5", "ctrl+r"] {
            assert!(table.iter().any(|b| b.keys == [key]
                && b.when.is_none()
                && b.invocation == "view:refresh self"));
        }
        assert!(table.iter().all(|b| b.keys != ["r"]));
        assert!(table.iter().any(|b| b.keys == ["p", "p"]
            && b.when.as_deref() == Some("viewer")
            && b.invocation == "viewer:restart"));

        // = - 0 zoom every panel (`plus` too); the viewer zooms its image.
        for (key, direction) in [("=", "in"), ("plus", "in"), ("-", "out"), ("0", "reset")] {
            let bound: Vec<_> = table.iter().filter(|b| b.keys == [key]).collect();
            let generic = format!("view:zoom self {direction}");
            assert!(bound.iter().any(|b| b.when.is_none() && b.invocation == generic), "{key}");
            assert!(bound.iter().any(|b| b.when.as_deref() == Some("viewer")), "{key}");
        }
        assert!(table
            .iter()
            .any(|b| b.keys == ["p", "shift+a"] && b.invocation == "metarecord:row-add"));

        // Fullscreen is a builtin toggle, and now has a key of its own.
        for keys in [vec!["f11"], vec!["v", "f"]] {
            assert!(table
                .iter()
                .any(|b| b.keys == keys && b.invocation == "panel:toggle fullscreen"));
        }
        let registry = crate::command_registry::CommandRegistry::default();
        crate::register_builtins(&registry);
        // The registry holds the command; the binding pre-fills its argument.
        assert!(registry.list().iter().any(|c| c.name == "panel:toggle"));
    }

    // ── Checking a new binding ───────────────────────────────────────────

    fn place(combo: &str, when: Option<&str>, focus: Option<&str>) -> BindingPlace {
        BindingPlace {
            combo: combo.to_string(),
            when: when.map(str::to_string),
            focus: focus.map(str::to_string),
        }
    }

    fn table(source: &str) -> Vec<CompiledBinding> {
        KeybindingSet::from_source(source).unwrap().compiled()
    }

    #[test]
    fn test_compiled_table_tells_suggestions_apart() {
        let mut set = KeybindingSet::from_source(r#""x" = { command = "mine" }"#).unwrap();
        set.add_suggestion("y", "my-panel:go", Some("my-panel"), false, None).unwrap();
        let table = set.compiled();
        assert!(!table.iter().find(|b| b.keys == ["x"]).unwrap().suggested);
        assert!(table.iter().find(|b| b.keys == ["y"]).unwrap().suggested);
    }

    #[test]
    fn test_a_key_bound_in_the_same_place_is_refused() {
        let table = table(r#""ctrl+k" = { command = "workspace:new" }"#);
        let error = check_binding(&table, &place("K+ctrl", None, None)).unwrap_err();
        assert!(error.contains("ctrl+k"), "{error}");
        assert!(error.contains("workspace:new"), "{error}");
        assert!(error.contains("globally"), "{error}");
    }

    #[test]
    fn test_a_key_bound_elsewhere_is_free_here() {
        let table = table(r#""ctrl+k" = { command = "workspace:new" }"#);
        check_binding(&table, &place("ctrl+k", Some("metarecord-list"), None)).unwrap();
        check_binding(&table, &place("ctrl+k", None, Some("finder"))).unwrap();
    }

    #[test]
    fn test_a_sequence_and_its_prefix_in_the_same_place_are_refused() {
        let table = table(
            r#""g g" = { command = "a" }
"t" = { command = "b", when = "log" }"#,
        );
        // A key alone runs before any sequence it starts: `g g` would be dead.
        let error = check_binding(&table, &place("g", None, None)).unwrap_err();
        assert!(error.contains("g g"), "{error}");
        // And the other way round.
        let error = check_binding(&table, &place("t x", Some("log"), None)).unwrap_err();
        assert!(error.contains("`t`"), "{error}");
        // Elsewhere, both are free.
        check_binding(&table, &place("g", Some("log"), None)).unwrap();
        check_binding(&table, &place("t x", None, None)).unwrap();
    }

    #[test]
    fn test_the_edited_line_does_not_hide_its_own_replacement() {
        let set = KeybindingSet::from_source(r#""g g" = { command = "a" }"#).unwrap();
        let check = set.check_entry(|_| true, &draft("g", "a"), Some(&line("g g")));
        assert_eq!(check, BindingCheck::default());
    }

    #[test]
    fn test_a_malformed_key_is_refused() {
        assert!(check_binding(&[], &place("ctrl+", None, None)).is_err());
        let set = KeybindingSet::from_source("").unwrap();
        assert!(set.check_entry(|_| true, &draft("ctrl+", "a"), None).key.is_some());
    }

    #[test]
    fn test_the_set_checks_a_line_field_by_field() {
        let set =
            KeybindingSet::from_source(r#""ctrl+k" = { command = "workspace:new" }"#).unwrap();
        let known = |name: &str| name == "workspace:new";

        let check = set.check_entry(known, &draft("ctrl+k", "nope"), None);
        assert!(check.command.unwrap().contains("nope"));
        assert!(check.key.unwrap().contains("workspace:new"));

        let free = set.check_entry(known, &draft("ctrl+j", "workspace:new"), None);
        assert_eq!(free, BindingCheck::default());
        assert!(free.into_result().is_ok());
    }

    #[test]
    fn test_a_command_must_exist() {
        let known = |name: &str| name == "panel:open";
        check_command("panel:open here viewer", known).unwrap();
        check_command("  panel:open", known).unwrap();
        // A shell line names no command.
        check_command("!ls -l", known).unwrap();
        let error = check_command("panel:opne here", known).unwrap_err();
        assert!(error.contains("panel:opne"), "{error}");
        assert!(check_command("  ", known).is_err());
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

    #[test]
    fn test_reserved_combos_are_the_exits_plus_escape() {
        // The user's ways out of a script question must never be a script's to
        // take (doc "Script sessions"): whatever opens the command input,
        // whatever toggles the script keys, and escape.
        let defaults = r#"
":" = { command = "command-input:focus command" }
"ctrl+p" = { command = "command-input:focus command" }
"tab" = { command = "script-keys:toggle" }
"f6" = { command = "panel:focus cycle" }
"o" = { command = "panel:focus next" }
"y" = { command = "metarecord-list:next" }
"g g" = { command = "metarecord-list:goto-top" }
"#;
        let set = KeybindingSet::from_sources(defaults, "").unwrap();
        let reserved = reserved_combos(&set.compiled());
        // F6 walks to the question and back from it: it is a way out too.
        // `panel:focus next` is not — it moves between the panels only.
        assert_eq!(
            reserved,
            vec![":", "backspace", "ctrl+escape", "ctrl+p", "escape", "f6", "tab"]
        );
        // Both ways of ending the run: escape asks, ctrl+escape kills.
        assert!(is_reserved(&reserved, "Ctrl+Escape"));

        // A script's key is compared normalized, so "Escape" and "CTRL+P" hit.
        assert!(is_reserved(&reserved, "Escape"));
        assert!(is_reserved(&reserved, "ctrl+P"));
        assert!(is_reserved(&reserved, "tab"));
        // "back" is the user's way out of an answer, so no script may take it.
        assert!(is_reserved(&reserved, "backspace"));
        assert!(is_reserved(&reserved, "Backspace"));
        assert!(!is_reserved(&reserved, "y"));
        // Garbage is not reserved: the caller reports it as a malformed key.
        assert!(!is_reserved(&reserved, "ctrl+"));
    }

    #[test]
    fn test_reserved_combos_ignore_sequences() {
        // A multi-chord sequence cannot be a script key at all (a script awaits
        // single presses), so it never enters the reserved set.
        let defaults = r#"
"g c" = { command = "command-input:focus command" }
"#;
        let set = KeybindingSet::from_sources(defaults, "").unwrap();
        // Only the literals remain: they are reserved whatever is bound.
        assert_eq!(reserved_combos(&set.compiled()), vec!["backspace", "ctrl+escape", "escape"]);
    }
}
