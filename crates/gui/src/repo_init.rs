//! Creating a repository from the GUI (spec-file-tracking "Ignore presets"):
//! the same `core::repo_init` orchestration the CLI's `mf repo init` uses —
//! `POST /repos/init` then applying the `default` ignore preset to the new root
//! — so a GUI-created repo gets the same default ignores (the daemon itself
//! writes none). Reuses the trash module's blocking `ureq` client, since core's
//! orchestration is synchronous.

use std::sync::Arc;

use serde_json::json;

use crate::commands::App;
use crate::trash::BlockingClient;

/// `repo_init` Tauri command: initialises a repository and applies its ignore
/// preset. Mirrors `mf repo init` — `default` unless `no_ignore` is set (or
/// `ignore` names other presets). Returns the new repo's uuid.
#[tauri::command]
pub async fn repo_init(
    app: tauri::State<'_, Arc<App>>,
    root: String,
    name: Option<String>,
    metafolder: Option<String>,
    no_ignore: Option<bool>,
    ignore: Option<Vec<String>>,
) -> Result<String, String> {
    let base = app.daemon.base_url();
    tokio::task::spawn_blocking(move || {
        repo_init_blocking(
            base,
            root,
            name,
            metafolder,
            no_ignore.unwrap_or(false),
            ignore.unwrap_or_default(),
        )
    })
    .await
    .map_err(|e| format!("repo init task panicked: {e}"))?
}

fn repo_init_blocking(
    base: String,
    root: String,
    name: Option<String>,
    metafolder: Option<String>,
    no_ignore: bool,
    ignore: Vec<String>,
) -> Result<String, String> {
    use metafolder_core::repo_init::{init_repo, InitIgnore};

    let client = BlockingClient::new(base);
    let mut body = json!({ "root": root });
    if let Some(n) = name.filter(|n| !n.trim().is_empty()) {
        body["name"] = json!(n);
    }
    if let Some(m) = metafolder.filter(|m| !m.trim().is_empty()) {
        body["metafolder"] = json!(m);
    }

    let outcome = if no_ignore {
        init_repo(&client, &body, InitIgnore::None)
    } else {
        let names: Vec<String> = if ignore.is_empty() { vec!["default".into()] } else { ignore };
        let presets = metafolder_core::ignore_presets::load()
            .map_err(|e| format!("{e}; run metafolder-sync-config to install it"))?;
        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        init_repo(&client, &body, InitIgnore::Presets { presets: &presets, names: &name_refs })
    }
    .map_err(|e| e.to_string())?;

    Ok(outcome.repo_uuid)
}
