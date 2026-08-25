//! Ignore-preset Tauri commands (spec-gui "Ignore patterns"): the GUI's half of
//! `mf ignore`. Preset expansion reads a *config file*
//! (`~/.config/metafolder/core/ignore-presets.toml`), which a panel cannot do,
//! so it lives here; everything else is [`metafolder_core::ignore`], the same
//! add/remove/set orchestration the CLI drives. The two introspection endpoints
//! (`POST …/eligibility`, `GET …/ignore/effective`) need no command — a panel
//! calls them straight through `metafolder.daemon`.

use std::sync::Arc;

use serde::Serialize;
use uuid::Uuid;

use metafolder_core::ignore::{self, IgnoreError, Mode};
use metafolder_core::ignore_presets::Presets;
use metafolder_core::trash::DaemonClient;

use crate::commands::App;
use crate::trash::BlockingClient;

/// One installed preset, fully expanded (group members included) so the caller
/// can show what applying it would write without re-implementing expansion.
#[derive(Debug, Clone, Serialize)]
pub struct PresetInfo {
    pub name: String,
    pub description: String,
    pub patterns: Vec<String>,
}

/// Loads the presets file, pointing at `metafolder-sync-config` when it is
/// missing or malformed — never a silent fallback (spec-config).
fn load_presets() -> Result<Presets, String> {
    metafolder_core::ignore_presets::load()
        .map_err(|e| format!("{e}; run metafolder-sync-config to install it"))
}

/// Expands every installed preset into a [`PresetInfo`], sorted by name.
fn preset_infos(presets: &Presets) -> Result<Vec<PresetInfo>, String> {
    presets
        .descriptions()
        .into_iter()
        .map(|(name, description)| {
            let patterns = presets.expand(&[name.as_str()])?;
            Ok(PresetInfo { name, description, patterns })
        })
        .collect()
}

/// The wire form of [`Mode`] — the three `mf ignore` verbs.
fn parse_mode(mode: &str) -> Result<Mode, String> {
    match mode {
        "add" => Ok(Mode::Add),
        "remove" => Ok(Mode::Remove),
        "set" => Ok(Mode::Set),
        other => Err(format!("unknown ignore mode '{other}' (add, remove or set)")),
    }
}

fn flatten(e: IgnoreError) -> String {
    e.to_string()
}

/// Expands `names` and applies them to `target`'s `mf_ignore` set, returning the
/// resulting rows. Split from the Tauri command so it can be driven by a stub.
fn apply_with(
    client: &dyn DaemonClient,
    repo: &str,
    target: Uuid,
    presets: &Presets,
    names: &[String],
    mode: Mode,
) -> Result<Vec<String>, String> {
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    ignore::apply(client, repo, target, presets, &refs, mode).map_err(flatten)
}

/// 32-char hex (the daemon's wire form) → uuid.
fn parse_target(target: &str) -> Result<Uuid, String> {
    Uuid::parse_str(target).map_err(|_| format!("invalid metarecord uuid '{target}'"))
}

/// `ignore_presets`: the installed presets, each fully expanded.
#[tauri::command]
pub async fn ignore_presets() -> Result<Vec<PresetInfo>, String> {
    tokio::task::spawn_blocking(|| preset_infos(&load_presets()?))
        .await
        .map_err(|e| format!("ignore presets task panicked: {e}"))?
}

/// `ignore_current`: the target metarecord's own `mf_ignore` rows, in order.
#[tauri::command]
pub async fn ignore_current(
    app: tauri::State<'_, Arc<App>>,
    repo: String,
    target: String,
) -> Result<Vec<String>, String> {
    let base = app.daemon.base_url();
    let target = parse_target(&target)?;
    tokio::task::spawn_blocking(move || {
        ignore::current_patterns(&BlockingClient::new(base), &repo, target).map_err(flatten)
    })
    .await
    .map_err(|e| format!("ignore current task panicked: {e}"))?
}

/// `ignore_apply`: applies named presets to a target with `add`/`remove`/`set`,
/// returning the resulting pattern list.
#[tauri::command]
pub async fn ignore_apply(
    app: tauri::State<'_, Arc<App>>,
    repo: String,
    target: String,
    presets: Vec<String>,
    mode: String,
) -> Result<Vec<String>, String> {
    let base = app.daemon.base_url();
    let target = parse_target(&target)?;
    let mode = parse_mode(&mode)?;
    tokio::task::spawn_blocking(move || {
        let loaded = load_presets()?;
        apply_with(&BlockingClient::new(base), &repo, target, &loaded, &presets, mode)
    })
    .await
    .map_err(|e| format!("ignore apply task panicked: {e}"))?
}

/// `ignore_write`: replaces the target's `mf_ignore` rows with exactly
/// `patterns` (ad-hoc patterns, reordering, deletion). An empty list unsets it.
#[tauri::command]
pub async fn ignore_write(
    app: tauri::State<'_, Arc<App>>,
    repo: String,
    target: String,
    patterns: Vec<String>,
) -> Result<(), String> {
    let base = app.daemon.base_url();
    let target = parse_target(&target)?;
    tokio::task::spawn_blocking(move || {
        ignore::write_patterns(&BlockingClient::new(base), &repo, target, &patterns).map_err(flatten)
    })
    .await
    .map_err(|e| format!("ignore write task panicked: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use metafolder_core::ignore_presets::Presets;
    use metafolder_core::trash::{DaemonClient, DaemonError};
    use serde_json::{json, Value};
    use std::cell::RefCell;
    use uuid::Uuid;

    const SRC: &str = r#"
[git]
description = "Git metadata"
patterns = ['\.git(/.*)?$']

[node]
description = "npm dependencies"
patterns = ['node_modules(/.*)?$']

[default]
description = "Everything"
pattern-set = ["node", "git"]
"#;

    /// Records every call; answers the `mf_ignore` GET with `current`.
    struct Stub {
        current: Vec<String>,
        calls: RefCell<Vec<(String, String, Option<Value>)>>,
    }

    impl DaemonClient for Stub {
        fn request(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, DaemonError> {
            self.calls.borrow_mut().push((method.into(), path.into(), body.cloned()));
            let values: Vec<Value> = self
                .current
                .iter()
                .map(|p| json!({"type": "string", "value": p}))
                .collect();
            Ok(json!({"name": "mf_ignore", "values": values}))
        }
    }

    #[test]
    fn test_preset_infos_expand_groups_and_sort_by_name() {
        let presets = Presets::parse(SRC).unwrap();
        let infos = preset_infos(&presets).unwrap();
        let names: Vec<&str> = infos.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["default", "git", "node"], "sorted by name");
        let default = &infos[0];
        assert_eq!(default.description, "Everything");
        assert_eq!(
            default.patterns,
            vec!["node_modules(/.*)?$".to_string(), r"\.git(/.*)?$".to_string()],
            "a group carries its members' expansion, in reference order"
        );
    }

    #[test]
    fn test_apply_add_appends_to_the_existing_set() {
        let stub = Stub {
            current: vec!["keep-me".into()],
            calls: RefCell::new(Vec::new()),
        };
        let target = Uuid::new_v4();
        let presets = Presets::parse(SRC).unwrap();
        let result =
            apply_with(&stub, "repo1", target, &presets, &["git".into()], Mode::Add).unwrap();
        assert_eq!(result, vec!["keep-me".to_string(), r"\.git(/.*)?$".to_string()]);
        let put = stub
            .calls
            .borrow()
            .iter()
            .find(|(m, _, _)| m == "PUT")
            .cloned()
            .expect("the resulting set is written back");
        assert!(put.1.ends_with("/fields/mf_ignore"), "written on the target's field: {}", put.1);
    }

    #[test]
    fn test_apply_rejects_an_unknown_preset() {
        let stub = Stub { current: Vec::new(), calls: RefCell::new(Vec::new()) };
        let presets = Presets::parse(SRC).unwrap();
        let err = apply_with(&stub, "repo1", Uuid::new_v4(), &presets, &["nope".into()], Mode::Add)
            .unwrap_err();
        assert!(err.contains("nope"), "the message names the unknown preset: {err}");
        assert!(stub.calls.borrow().is_empty(), "nothing is written when expansion fails");
    }

    #[test]
    fn test_mode_parsing() {
        assert_eq!(parse_mode("add").unwrap(), Mode::Add);
        assert_eq!(parse_mode("remove").unwrap(), Mode::Remove);
        assert_eq!(parse_mode("set").unwrap(), Mode::Set);
        assert!(parse_mode("clobber").is_err());
    }
}
