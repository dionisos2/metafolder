//! The GUI's catalogs, read from the code for the documentation wiki (and for
//! the shipped-keybindings check): every command — the builtins listed in
//! Rust, the panel commands each panel type's JavaScript registers, the user
//! commands of the shipped `commands.js` — and every panel type. The golden
//! test writes them as the wiki's generated notes
//! (`metafolder_core::doc_gen`); test-only, so nothing of it is in the binary.

use crate::keybindings::{CompiledBinding, KeybindingSet};
use metafolder_core::doc_gen::{code, generated_dir, slug, sync_generated, tid};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Who defines a command.
#[derive(Clone, Debug, PartialEq)]
pub enum Owner {
    Shell,
    Panel(String),
    UserCommands,
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

    // Panel commands: `commands.register('<name>'` and the shared
    // `registerFind(metafolder, '<name>'` helper (panel-shim/find-entry.js).
    for panel in panel_types() {
        let main_js = default_config().join("panel-types").join(&panel).join("main.js");
        let Ok(source) = std::fs::read_to_string(&main_js) else { continue };
        for (name, label) in scan(&source, &["commands.register('", "registerFind(metafolder, '"]) {
            out.push(Command { name, label, owner: Owner::Panel(panel.clone()) });
        }
    }

    // User commands: `commands.js` maps each name to its definition, the name
    // written as the entry's quoted key (`'name': {`, a name holds a colon),
    // outside the comments.
    let source =
        std::fs::read_to_string(default_config().join("commands.js")).expect("commands.js");
    let mut rest = source.as_str();
    let mut keys = Vec::new();
    while let Some(at) = rest.find("': {") {
        if let Some(start) = rest[..at].rfind('\'') {
            let offset = source.len() - rest.len() + start + 1;
            // A key on a comment line is an example (the file's header shows
            // the shape of an entry), not a command.
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
        out.push(Command { name: name.clone(), label, owner: Owner::UserCommands });
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
        Owner::UserCommands => format!("the shipped {}", code("commands.js")),
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
    let target = format!("{panel} panel");
    let title = format!("$:/mf/gen/Panel/{target}");
    let summary = format!("The {panel} panel type");
    tid(
        &[("title", &title), ("catalog", "Panel"), ("target", &target), ("summary", &summary)],
        &text,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_reads_names_and_their_labels() {
        let source = "commands.register('a:one', {\n  label: 'First',\n});\n\
                      commands.register('a:two', { handler });\n\
                      registerFind(metafolder, 'a:find', { label: `Find` });\n\
                      commands.register('a:page', variant('page', PAGES, 'Go to a page'));";
        assert_eq!(
            scan(source, &["commands.register('", "registerFind(metafolder, '"]),
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
        assert_eq!(owner("metarecord:remove"), Some(Owner::UserCommands));
        // The example in commands.js's header comment is not a command.
        assert_eq!(owner("user:thing"), None);
        let restore = all.iter().find(|c| c.name == "trash:restore").unwrap();
        assert_eq!(restore.label, "Trash: restore the selected entry");
    }

    /// The wiki's generated notes for the `GUI command` and `Panel` catalogs
    /// are what the code has (`MF_DOC_UPDATE=1` rewrites them: scripts/doc gen).
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
    }
}
