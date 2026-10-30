//! The GUI's catalogs, read from the code for the documentation wiki (and for
//! the shipped-keybindings check): every command — the builtins listed in
//! Rust, the panel commands each panel type's JavaScript registers, the user
//! commands of the shipped `commands.js` and `user-commands.js` — and every panel type. The golden
//! test writes them as the wiki's generated notes
//! (`metafolder_core::doc_gen`); test-only, so nothing of it is in the binary.

use crate::keybindings::{CompiledBinding, KeybindingSet};
use metafolder_core::doc_gen::{code, generated_dir, slug, sync_generated, tid};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Who defines a command.
#[derive(Clone, Debug, PartialEq)]
pub enum Owner {
    Shell,
    Panel(String),
    /// An entry of a shipped command module: `commands.js` or `user-commands.js`.
    UserCommands(&'static str),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Command {
    pub name: String,
    pub label: String,
    pub owner: Owner,
}

fn default_config() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("default-config")
}

/// The panel types shipped in `default-config/panel-types/`, sorted.
pub fn panel_types() -> Vec<String> {
    let mut types: Vec<String> = std::fs::read_dir(default_config().join("panel-types"))
        .expect("panel-types/ is readable")
        .flatten()
        .filter(|e| e.path().join("index.html").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    types.sort();
    types
}

/// The first quoted string after `label:` in `source`, if any.
fn label_in(source: &str) -> String {
    let Some(at) = source.find("label:") else { return String::new() };
    let rest = source[at + 6..].trim_start();
    let Some(quote) = rest.chars().next().filter(|c| matches!(c, '\'' | '"' | '`')) else {
        return String::new();
    };
    // A long label is wrapped as `'first half ' + 'second half'`: follow the
    // concatenation, literal after literal.
    let mut label = String::new();
    let mut rest = &rest[1..];
    while let Some(end) = rest.find(quote) {
        label.push_str(&rest[..end]);
        let after = rest[end + 1..].trim_start();
        let Some(next) = after.strip_prefix('+').map(str::trim_start) else { break };
        let Some(next) = next.strip_prefix(quote) else { break };
        rest = next;
    }
    if quote == '`' {
        without_interpolations(&label)
    } else {
        label
    }
}

/// A template label without what only the running panel knows: a parenthesised
/// aside holding an `${…}` goes whole (with the space before it), and any other
/// `${…}` becomes an ellipsis.
fn without_interpolations(label: &str) -> String {
    let mut out = String::new();
    let mut rest = label;
    while let Some(open) = rest.find('(') {
        let Some(close) = rest[open..].find(')').map(|c| open + c) else { break };
        // Nested parentheses (a `join(' / ')` call) close later: extend to the
        // parenthesis that balances the aside's.
        let mut depth = 0;
        let mut end = close;
        for (i, ch) in rest[open..].char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = open + i;
                        break;
                    }
                }
                _ => {}
            }
        }
        if rest[open..end].contains("${") {
            out.push_str(rest[..open].trim_end());
        } else {
            out.push_str(&rest[..=end]);
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    let mut text = String::new();
    let mut rest = out.as_str();
    while let Some(start) = rest.find("${") {
        text.push_str(&rest[..start]);
        text.push('…');
        rest = rest[start..].find('}').map_or("", |e| &rest[start + e + 1..]);
    }
    text.push_str(rest);
    text
}

/// Every `(name, label)` a source registers after one of `markers` (`marker`
/// then the name up to the closing quote). A label is looked for between a
/// name and the next registration: a `label:` property, else the last string
/// of the registering call.
fn scan(source: &str, markers: &[&str]) -> Vec<(String, String)> {
    let mut starts: Vec<(usize, usize)> = markers
        .iter()
        .flat_map(|m| source.match_indices(m).map(|(at, m)| (at, at + m.len())))
        .collect();
    starts.sort();
    let mut out = Vec::new();
    for (i, &(_, name_at)) in starts.iter().enumerate() {
        let Some(end) = source[name_at..].find('\'') else { continue };
        let name = &source[name_at..name_at + end];
        let next = starts.get(i + 1).map_or(source.len(), |s| s.0);
        let span = &source[name_at + end + 1..next];
        let mut label = label_in(span);
        if label.is_empty() {
            // No `label:` — a helper builds the definition from its arguments
            // (`variant('page', PAGES, 'File: go to a page')`): the call's
            // last string literal.
            let call = span.split(';').next().unwrap_or("");
            label = call.split('\'').skip(1).step_by(2).last().unwrap_or("").to_string();
        }
        out.push((name.to_string(), label));
    }
    out
}

/// Every command the shipped GUI defines, sorted by name.
pub fn commands() -> Vec<Command> {
    let registry = crate::command_registry::CommandRegistry::default();
    crate::register_builtins(&registry);
    let mut out: Vec<Command> = registry
        .list()
        .into_iter()
        .map(|c| Command { name: c.name, label: c.label, owner: Owner::Shell })
        .collect();

    // Panel commands: `commands.register('<name>'`, in every module of the
    // panel — a command may live in a sub-module that `main.js` imports
    // (metarecord-detail/bulk-commands.js).
    for panel in panel_types() {
        let dir = default_config().join("panel-types").join(&panel);
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let mut modules: Vec<_> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "js"))
            .collect();
        modules.sort();
        for module in modules {
            let Ok(source) = std::fs::read_to_string(&module) else { continue };
            for (name, label) in scan(&source, &["commands.register('"]) {
                out.push(Command { name, label, owner: Owner::Panel(panel.clone()) });
            }
        }
    }

    // User commands: each command module maps a name to its definition, the
    // name written as the entry's quoted key (`'name': {`, a name holds a
    // colon), outside the comments.
    for file in ["commands.js", "user-commands.js"] {
        let source = std::fs::read_to_string(default_config().join(file)).expect(file);
        let mut rest = source.as_str();
        let mut keys = Vec::new();
        while let Some(at) = rest.find("': {") {
            if let Some(start) = rest[..at].rfind('\'') {
                let offset = source.len() - rest.len() + start + 1;
                // A key on a comment line is an example (the file's header
                // shows the shape of an entry), not a command.
                let line_start = source[..offset].rfind('\n').map_or(0, |n| n + 1);
                if !source[line_start..offset].trim_start().starts_with("//") {
                    keys.push((offset, rest[start + 1..at].to_string()));
                }
            }
            rest = &rest[at + 4..];
        }
        for (i, (at, name)) in keys.iter().enumerate() {
            let next = keys.get(i + 1).map_or(source.len(), |k| k.0);
            let label = label_in(&source[*at..next]);
            out.push(Command { name: name.clone(), label, owner: Owner::UserCommands(file) });
        }
    }

    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.dedup_by(|a, b| a.name == b.name);
    out
}

/// The shipped keybinding table, compiled.
pub fn default_bindings() -> Vec<CompiledBinding> {
    let defaults = include_str!("../default-config/keybindings.toml");
    KeybindingSet::from_sources(defaults, "").unwrap().compiled()
}

/// One binding as the notes show it: its keys, what it runs, where.
fn binding_line(b: &CompiledBinding) -> String {
    let mut line = format!("* {} → {}", code(&b.keys.join(" ")), code(&b.invocation));
    let mut scope = Vec::new();
    if let Some(when) = &b.when {
        scope.push(format!("in the {} panel", code(when)));
    }
    if let Some(focus) = &b.focus {
        scope.push(format!("while {} is focused", code(focus)));
    }
    if b.text_input {
        scope.push("also in a text input".to_string());
    }
    if !scope.is_empty() {
        line.push_str(&format!(" ({})", scope.join(", ")));
    }
    line.push('\n');
    line
}

fn command_note(c: &Command, bindings: &[CompiledBinding]) -> String {
    let owner = match &c.owner {
        Owner::Shell => "the shell (a builtin)".to_string(),
        Owner::Panel(p) => format!("the {} panel", code(p)),
        Owner::UserCommands(file) => format!("the shipped {}", code(file)),
    };
    let mut text = format!("!! Reference\n\n* Defined by {owner}\n");
    let keys: Vec<&CompiledBinding> = bindings
        .iter()
        .filter(|b| b.invocation.split_whitespace().next() == Some(c.name.as_str()))
        .collect();
    if keys.is_empty() {
        text.push_str("* No default key\n");
    } else {
        text.push_str("\n!! Default keys\n\n");
        for b in keys {
            text.push_str(&binding_line(b));
        }
    }
    let title = format!("$:/mf/gen/GUI command/{}", c.name);
    tid(
        &[
            ("title", &title),
            ("catalog", "GUI command"),
            ("target", &c.name),
            ("summary", &c.label),
        ],
        &text,
    )
}

/// The names a panel module provides to the generic commands (doc "Generic
/// panel commands"): the top-level keys of the object literal handed to
/// `metafolder.provide({…})`, in order. Strings and comments are skipped, and
/// so is whatever nests deeper than the object itself.
fn provided(source: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = source;
    while let Some(at) = rest.find("metafolder.provide({") {
        let body = &rest[at + "metafolder.provide({".len()..];
        let mut chars = body.char_indices().peekable();
        let (mut depth, mut expect_key) = (1usize, true);
        let mut end = body.len();
        while let Some((i, c)) = chars.next() {
            match c {
                '\'' | '"' | '`' => {
                    let mut escaped = false;
                    for (_, d) in chars.by_ref() {
                        if escaped {
                            escaped = false;
                        } else if d == '\\' {
                            escaped = true;
                        } else if d == c {
                            break;
                        }
                    }
                }
                '/' if chars.peek().is_some_and(|&(_, d)| d == '/') => {
                    for (_, d) in chars.by_ref() {
                        if d == '\n' {
                            break;
                        }
                    }
                }
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = i;
                        break;
                    }
                }
                ',' if depth == 1 => expect_key = true,
                c if expect_key && depth == 1 && (c.is_alphabetic() || c == '_') => {
                    let mut name = c.to_string();
                    while let Some(&(_, d)) = chars.peek() {
                        if !(d.is_alphanumeric() || d == '_') {
                            break;
                        }
                        name.push(d);
                        chars.next();
                    }
                    out.push(name);
                    expect_key = false;
                }
                _ => {}
            }
        }
        rest = &body[end..];
    }
    out
}

/// Every module of a shipped panel type, concatenated.
fn panel_source(panel: &str) -> String {
    let dir = default_config().join("panel-types").join(panel);
    let Ok(entries) = std::fs::read_dir(&dir) else { return String::new() };
    let mut modules: Vec<_> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "js"))
        .collect();
    modules.sort();
    modules.iter().filter_map(|m| std::fs::read_to_string(m).ok()).collect()
}

fn panel_note(panel: &str, commands: &[Command]) -> String {
    let mut text = String::from("!! Commands\n\n");
    let own: Vec<&Command> =
        commands.iter().filter(|c| c.owner == Owner::Panel(panel.to_string())).collect();
    if own.is_empty() {
        text.push_str("None of its own.\n");
    }
    for c in own {
        text.push_str(&format!("* {} — {}\n", code(&c.name), c.label));
    }
    text.push_str("\n!! Generic commands\n\n");
    let names = provided(&panel_source(panel));
    if names.is_empty() {
        text.push_str("Provides nothing to [[Generic panel commands]].\n");
    } else {
        let list: Vec<String> = names.iter().map(|n| code(n)).collect();
        text.push_str(&format!("Provides {} to [[Generic panel commands]].\n", list.join(", ")));
    }
    let target = format!("{panel} panel");
    let title = format!("$:/mf/gen/Panel/{target}");
    let summary = format!("The {panel} panel type");
    tid(
        &[("title", &title), ("catalog", "Panel"), ("target", &target), ("summary", &summary)],
        &text,
    )
}

/// What a key of each level is, the first column of doc "Key levels".
const KEY_LEVELS: [&str; 4] = [
    "a letter alone: `a` … `z`",
    "another key typed without a modifier: `,` `-` `=`, the named keys — arrows, Enter, \
     Escape, Backspace, Delete, Insert, Tab, Space, Home, End, PageUp, PageDown, Menu, `plus` \
     — and F1 … F12",
    "Shift, Ctrl, Alt or Ctrl+Alt on a key of level 0 or 1; and alone, a character that needs \
     Shift on one of the three layouts — the digits and `. / ; : ! * ( ) \" $ % ?`",
    "Ctrl, Alt or Ctrl+Alt with Shift, on a key of level 0 or 1",
];

/// The keys of a level sharing their modifiers.
struct KeyGroup {
    level: usize,
    with: &'static str,
    prefix: &'static str,
    keys: Vec<String>,
}

impl KeyGroup {
    fn chord(&self, key: &str) -> String {
        format!("{}{key}", self.prefix)
    }

    /// The group's keys a first key of the shipped file uses, and the others.
    fn split(&self, first_keys: &BTreeSet<String>) -> (Vec<&String>, Vec<&String>) {
        self.keys.iter().partition(|k| first_keys.contains(&self.chord(k)))
    }
}

/// Every key that has a level (`key_level`), by level and modifiers.
fn key_groups() -> Vec<KeyGroup> {
    use crate::keybindings::{DIRECT_KEYS, NAMED_KEYS, SHIFTED_KEYS};
    let letters: Vec<String> = ('a'..='z').map(String::from).collect();
    let others: Vec<String> = DIRECT_KEYS
        .iter()
        .chain(NAMED_KEYS.iter())
        .map(|k| k.to_string())
        .chain((1..=12).map(|n| format!("f{n}")))
        .collect();
    let plain = [letters.clone(), others.clone()].concat();
    let shifted = SHIFTED_KEYS.iter().map(|k| k.to_string()).collect();
    let group = |level, with, prefix, keys| KeyGroup { level, with, prefix, keys };
    vec![
        group(0, "Alone", "", letters),
        group(1, "Alone", "", others),
        group(2, "Shift", "shift+", plain.clone()),
        group(2, "Alone, needing Shift on a layout", "", shifted),
        group(2, "Ctrl", "ctrl+", plain.clone()),
        group(2, "Alt", "alt+", plain.clone()),
        group(2, "Ctrl+Alt", "ctrl+alt+", plain.clone()),
        group(3, "Ctrl+Shift", "ctrl+shift+", plain.clone()),
        group(3, "Alt+Shift", "alt+shift+", plain.clone()),
        group(3, "Ctrl+Alt+Shift", "ctrl+alt+shift+", plain),
    ]
}

fn shipped_first_keys() -> BTreeSet<String> {
    let defaults = include_str!("../default-config/keybindings.toml");
    let table = KeybindingSet::from_source(defaults).unwrap().compiled();
    table.iter().map(|b| b.keys[0].clone()).collect()
}

/// The table opening doc "Key levels", which transcludes it: what each level
/// is, how many keys it has and how many the shipped file uses as a first key.
fn key_levels_recap_note() -> String {
    let first_keys = shipped_first_keys();
    let groups = key_groups();
    let mut text = String::from("|!Level |!Keys |!Count |!Used as a first key |!Left |\n");
    for (level, keys) in KEY_LEVELS.iter().enumerate() {
        let (mut used, mut left) = (0, 0);
        for group in groups.iter().filter(|g| g.level == level) {
            let (u, l) = group.split(&first_keys);
            used += u.len();
            left += l.len();
        }
        text.push_str(&format!("|{level} |{keys} |{} |{used} |{left} |\n", used + left));
    }
    tid(
        &[
            ("title", "$:/mf/gen/Key usage/Key levels recap"),
            ("summary", "What each key level is, its size, and how much of it is used"),
        ],
        &text,
    )
}

/// The generated half of doc "Key levels": the 26 letters of level 0 — what
/// each one is used for, and the ones left —, then the keys used and left of
/// the other levels.
fn key_levels_note() -> String {
    let defaults = include_str!("../default-config/keybindings.toml");
    let set = KeybindingSet::from_source(defaults).unwrap();
    let table = set.compiled();
    let letters: Vec<String> = ('a'..='z').map(String::from).collect();
    let mut text = String::new();

    // Level 0: what each letter is used for.
    let entries = set.entries();
    let variables = set.variables();
    // The variables that start a line — a domain — or prefix a template's
    // keys — a target.
    let starts_a_line = |name: &str| {
        let reference = format!("{{{name}}}");
        name.starts_with("target-")
            || entries.iter().any(|e| e.source.split_whitespace().next() == Some(&reference))
    };
    let mut used = Vec::new();
    let mut left = Vec::new();
    for letter in &letters {
        let alone: Vec<String> = table
            .iter()
            .filter(|b| b.keys == [letter.clone()])
            .map(|b| match &b.when {
                Some(when) => format!("{} (in {})", code(&b.invocation), code(when)),
                None => code(&b.invocation),
            })
            .collect();
        let sequences: Vec<_> =
            table.iter().filter(|b| b.keys.len() > 1 && b.keys[0] == *letter).collect();
        if alone.is_empty() && sequences.is_empty() {
            left.push(code(letter));
            continue;
        }
        let mut uses = alone;
        if !sequences.is_empty() {
            let names: Vec<String> = variables
                .iter()
                .filter(|v| v.keys.split_whitespace().next() == Some(letter.as_str()))
                .filter(|v| starts_a_line(&v.name))
                .map(|v| code(&format!("{{{}}}", v.name)))
                .collect();
            let mut panels: Vec<&str> =
                sequences.iter().filter_map(|b| b.when.as_deref()).collect();
            panels.sort();
            panels.dedup();
            let anywhere = sequences.iter().any(|b| b.when.is_none());
            let mut line = format!(
                "starts {} key{}",
                sequences.len(),
                if sequences.len() == 1 { "" } else { "s" }
            );
            if !names.is_empty() {
                line.push_str(&format!(" ({})", names.join(", ")));
            }
            if !anywhere {
                let list: Vec<String> = panels.iter().map(|p| code(p)).collect();
                line.push_str(&format!(", in {} only", list.join(", ")));
            }
            uses.push(line);
        }
        used.push(format!("|{} |{} |\n", code(letter), uses.join("; ")));
    }
    text.push_str(&format!("!! Level 0: the letters used ({})\n\n|!Letter |!For |\n", used.len()));
    text.extend(used);
    text.push_str(&format!("\n!! Level 0: the letters left ({})\n\n", left.len()));
    text.push_str(&if left.is_empty() { "None.".to_string() } else { left.join(" ") });
    text.push('\n');

    // The other levels: the first keys used and left, by modifiers.
    let first_keys = shipped_first_keys();
    let groups = key_groups();
    let keys_cell = |keys: &[&String]| {
        let list: Vec<String> = keys.iter().map(|k| code(k)).collect();
        format!("{}: {}", keys.len(), if list.is_empty() { "—".into() } else { list.join(" ") })
    };
    for level in 1..KEY_LEVELS.len() {
        text.push_str(&format!("\n!! Level {level}\n\n|!With |!Used |!Left |\n"));
        for group in groups.iter().filter(|g| g.level == level) {
            let (used, left) = group.split(&first_keys);
            text.push_str(&format!(
                "|{} |{} |{} |\n",
                group.with,
                keys_cell(&used),
                keys_cell(&left)
            ));
        }
    }
    tid(
        &[
            ("title", "$:/mf/gen/Key usage/Key levels"),
            ("catalog", "Key usage"),
            ("target", "Key levels"),
            ("summary", "The keys used and left at each level"),
        ],
        &text,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_group_holds_keys_of_its_level_and_covers_the_shipped_keys() {
        let groups = key_groups();
        let mut seen = BTreeSet::new();
        for group in &groups {
            for key in &group.keys {
                let chord = group.chord(key);
                assert_eq!(
                    crate::keybindings::key_level(&chord),
                    Some(group.level as u8),
                    "{chord}"
                );
                assert!(seen.insert(chord.clone()), "{chord} is listed twice");
            }
        }
        for first_key in shipped_first_keys() {
            assert!(seen.contains(&first_key), "{first_key} is shipped but listed nowhere");
        }
    }

    #[test]
    fn scan_reads_names_and_their_labels() {
        let source = "commands.register('a:one', {\n  label: 'First',\n});\n\
                      commands.register('a:two', { handler });\n\
                      helper(metafolder, 'a:find', { label: `Find` });\n\
                      commands.register('a:page', variant('page', PAGES, 'Go to a page'));";
        assert_eq!(
            scan(source, &["commands.register('", "helper(metafolder, '"]),
            vec![
                ("a:one".to_string(), "First".to_string()),
                ("a:two".into(), String::new()),
                ("a:find".into(), "Find".into()),
                // Through a helper: the call's last string.
                ("a:page".into(), "Go to a page".into()),
            ]
        );
    }

    #[test]
    fn a_template_label_loses_what_only_runtime_knows() {
        // A backtick label may interpolate a list the panel builds at run time;
        // the scan cannot evaluate it, so the parenthesised aside holding it
        // goes, and a bare interpolation becomes an ellipsis.
        let label = |src: &str| label_in(src);
        assert_eq!(label("label: `Focus a zone (${ZONES.join(' / ')})`,"), "Focus a zone");
        assert_eq!(label("label: `Seek (e.g. +${STEP}, -${LONG}) now`,"), "Seek now");
        assert_eq!(label("label: `Step by ${STEP} seconds`,"), "Step by … seconds");
        assert_eq!(label("label: 'Plain (as is)',"), "Plain (as is)");
    }

    #[test]
    fn a_label_split_over_concatenated_literals_is_read_whole() {
        // rustfmt-style wrapping of a long label: `'a ' +\n  'b'`.
        let src = "label:\n      'File manager: toggle a view flag (root / ' +\n      'sort by activity)',\n    args: [";
        assert_eq!(label_in(src), "File manager: toggle a view flag (root / sort by activity)");
    }

    #[test]
    fn commands_come_from_the_three_sources() {
        let all = commands();
        let owner = |name: &str| all.iter().find(|c| c.name == name).map(|c| c.owner.clone());
        assert_eq!(owner("help:key"), Some(Owner::Shell));
        assert_eq!(owner("trash:restore"), Some(Owner::Panel("trash".into())));
        assert_eq!(owner("item:delete"), Some(Owner::UserCommands("commands.js")));
        assert_eq!(owner("user:rate"), Some(Owner::UserCommands("user-commands.js")));
        // The example in a module's header comment is not a command.
        assert_eq!(owner("user:thing"), None);
        let restore = all.iter().find(|c| c.name == "trash:restore").unwrap();
        assert_eq!(restore.label, "Trash: restore the selected entry");
    }

    #[test]
    fn provided_reads_the_names_a_panel_provides() {
        // doc "Generic panel commands": the top-level keys of the object
        // handed to `metafolder.provide`, in order — a nested object or a
        // function body's own keys are not.
        let source = "x();\n  metafolder.provide({\n    index: () => cursorIndex,\n    \
                      count: () => rows.length,\n    select,\n    entries: () =>\n      \
                      rows.map((row) => ({ name: row.n, label: row.l })),\n    \
                      descend: async (index) => {\n      const item = { a: 1 };\n      \
                      return true;\n    },\n  });\n";
        assert_eq!(provided(source), ["index", "count", "select", "entries", "descend"]);
        assert!(provided("commands.register('a:b', {});").is_empty());
    }

    #[test]
    fn a_panel_note_lists_what_the_panel_provides() {
        let note = panel_note("trash", &[]);
        assert!(note.contains("!! Generic commands"), "{note}");
        assert!(note.contains("``index``, ``count``, ``select``, ``entries``"), "{note}");
        assert!(panel_note("hello", &[]).contains("Provides nothing"));
    }

    /// The wiki's generated notes for the `GUI command` and `Panel` catalogs
    /// are what the code has (`MF_DOC_UPDATE=1` rewrites them: scripts/dev/doc gen).
    #[test]
    fn the_wiki_catalogs_match_the_code() {
        let all = commands();
        let bindings = default_bindings();
        let notes: BTreeMap<String, String> = all
            .iter()
            .map(|c| (format!("{}.tid", slug(&c.name)), command_note(c, &bindings)))
            .collect();
        assert_eq!(notes.len(), all.len(), "two command names share a file name");
        sync_generated(&generated_dir(env!("CARGO_MANIFEST_DIR"), "GUI command"), &notes).unwrap();
        let panels: BTreeMap<String, String> = panel_types()
            .iter()
            .map(|p| (format!("{}.tid", slug(p)), panel_note(p, &all)))
            .collect();
        sync_generated(&generated_dir(env!("CARGO_MANIFEST_DIR"), "Panel"), &panels).unwrap();
        let levels = BTreeMap::from([
            ("key_levels.tid".to_string(), key_levels_note()),
            ("key_levels_recap.tid".to_string(), key_levels_recap_note()),
        ]);
        sync_generated(&generated_dir(env!("CARGO_MANIFEST_DIR"), "Key usage"), &levels).unwrap();
    }
}
