//! `mf gui`: thin client over the GUI scripting HTTP API (spec-gui
//! "Scripting / GUI API"). Plain-text output designed for shell scripts.

use std::path::PathBuf;

use serde_json::{json, Value as Json};

use crate::client::{Client, CliError};

const DEFAULT_GUI_URL: &str = "http://127.0.0.1:7524";

/// Resolves the GUI base URL: the explicit value (`--gui-url` /
/// `METAFOLDER_GUI_URL`) wins, then the first readable `gui.port` file
/// among `candidates`, then the default port.
pub fn base_url(explicit: Option<String>, candidates: &[PathBuf]) -> String {
    if let Some(url) = explicit {
        return url;
    }
    for path in candidates {
        if let Ok(content) = std::fs::read_to_string(path) {
            if let Ok(port) = content.trim().parse::<u16>() {
                return format!("http://127.0.0.1:{port}");
            }
        }
    }
    DEFAULT_GUI_URL.to_string()
}

/// The `gui.port` locations written by the GUI (spec-gui "GUI API port
/// persistence"): `$XDG_RUNTIME_DIR/metafolder/`, else the config root.
pub fn port_file_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        candidates.push(PathBuf::from(dir).join("metafolder").join("gui.port"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join(".config/metafolder-gui/gui.port"));
    }
    candidates
}

pub struct GuiCtx {
    pub client: Client,
}

impl GuiCtx {
    pub fn new(base_url: &str) -> Self {
        Self { client: Client::with_peer(base_url, "GUI") }
    }
}

const SLOTS: [&str; 2] = ["left", "right"];

fn check_slot(slot: &str) -> Result<(), CliError> {
    if SLOTS.contains(&slot) {
        Ok(())
    } else {
        Err(CliError::Usage(format!("invalid slot '{slot}' (expected 'left' or 'right')")))
    }
}

pub fn status(ctx: &GuiCtx) -> Result<i32, CliError> {
    let resp = ctx.client.get("/gui/status", &[])?;
    println!("{}", serde_json::to_string_pretty(&resp).expect("JSON serialization"));
    Ok(0)
}

/// Prints the active repository of the focused slot's workspace.
pub fn repo(ctx: &GuiCtx) -> Result<i32, CliError> {
    let status = ctx.client.get("/gui/status", &[])?;
    let layout = status["layout"].as_object().cloned().unwrap_or_default();
    let workspace_id = layout
        .values()
        .find(|slot| slot["focused"] == true)
        .and_then(|slot| slot["workspace_id"].as_str())
        .ok_or_else(|| CliError::Op("no focused workspace".into()))?;
    let repo = status["workspaces"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|ws| ws["id"] == workspace_id)
        .and_then(|ws| ws["active_repo"].as_str())
        .ok_or_else(|| {
            CliError::Op(format!("workspace {workspace_id} has no active repository"))
        })?;
    println!("{repo}");
    Ok(0)
}

pub fn workspace_new(ctx: &GuiCtx, repo: Option<&str>) -> Result<i32, CliError> {
    let body = match repo {
        Some(repo) => json!({"active_repo": repo}),
        None => json!({}),
    };
    let resp = ctx.client.post("/gui/workspaces", &body)?;
    println!("{}", resp["id"].as_str().unwrap_or_default());
    Ok(0)
}

pub fn workspace_rm(ctx: &GuiCtx, id: &str) -> Result<i32, CliError> {
    ctx.client.request("DELETE", &format!("/gui/workspaces/{id}"), &[], None)?;
    Ok(0)
}

/// `mf gui layout` prints both slots ("-" = hidden); with a slot, prints
/// just that slot; with a slot and a value, assigns it ("-" = hide).
pub fn layout(ctx: &GuiCtx, slot: Option<&str>, value: Option<&str>) -> Result<i32, CliError> {
    if let Some(slot) = slot {
        check_slot(slot)?;
    }
    match (slot, value) {
        (None, _) => {
            let resp = ctx.client.get("/gui/layout", &[])?;
            for name in SLOTS {
                println!("{name} {}", resp[name].as_str().unwrap_or("-"));
            }
        }
        (Some(slot), None) => {
            let resp = ctx.client.get("/gui/layout", &[])?;
            println!("{}", resp[slot].as_str().unwrap_or("-"));
        }
        (Some(slot), Some(value)) => {
            let assigned = if value == "-" { Json::Null } else { json!(value) };
            ctx.client.request("PUT", "/gui/layout", &[], Some(&json!({slot: assigned})))?;
        }
    }
    Ok(0)
}

/// `mf gui view <slot>` prints the current panel type; with a type (and
/// optional `--path` / `--state`), sets it.
pub fn view(
    ctx: &GuiCtx,
    slot: &str,
    panel_type: Option<&str>,
    path: Option<&str>,
    state: Option<&str>,
) -> Result<i32, CliError> {
    check_slot(slot)?;
    let url = format!("/gui/panels/{slot}/view");
    match panel_type {
        None => {
            let resp = ctx.client.get(&url, &[])?;
            println!("{}", resp["type"].as_str().unwrap_or_default());
        }
        Some(panel_type) => {
            let mut body = json!({"type": panel_type});
            if let Some(path) = path {
                body["path"] = json!(path);
            }
            if let Some(state) = state {
                body["state"] = serde_json::from_str(state)
                    .map_err(|e| CliError::Usage(format!("invalid --state JSON: {e}")))?;
            }
            ctx.client.request("PUT", &url, &[], Some(&body))?;
        }
    }
    Ok(0)
}

pub fn message(
    ctx: &GuiCtx,
    text: &str,
    workspace: Option<&str>,
    timeout_ms: Option<u64>,
) -> Result<i32, CliError> {
    let query: Vec<(&str, String)> = workspace
        .map(|ws| vec![("workspace_id", ws.to_string())])
        .unwrap_or_default();
    let body = json!({"text": text, "timeout_ms": timeout_ms});
    ctx.client.request("POST", "/gui/message", &query, Some(&body))?;
    Ok(0)
}

/// Blocks until one of `keys` is pressed and prints it; exit 1 on
/// timeout or when the GUI closes the wait.
pub fn input(ctx: &GuiCtx, keys: &[String], timeout_ms: Option<u64>) -> Result<i32, CliError> {
    let body = json!({"keys": keys, "timeout_ms": timeout_ms});
    let resp = ctx.client.post("/gui/input", &body)?;
    match resp["event"].as_str() {
        Some("answer") => {
            println!("{}", resp["value"].as_str().unwrap_or_default());
            Ok(0)
        }
        Some(other) => Err(CliError::Op(format!("input wait ended: {other}"))),
        None => Err(CliError::Op("malformed GUI response".into())),
    }
}

/// Blocks until the prompt is confirmed and prints the text; exit 1 on
/// cancel/timeout/close. `completions` are offered by the command input
/// autocomplete; with `completions_stdin`, one more completion per stdin
/// line (stopping at the first empty line or EOF).
pub fn prompt(
    ctx: &GuiCtx,
    text: &str,
    completions: &[String],
    completions_stdin: bool,
    timeout_ms: Option<u64>,
) -> Result<i32, CliError> {
    let mut completions = completions.to_vec();
    if completions_stdin {
        for line in std::io::stdin().lines() {
            let line = line.map_err(|e| CliError::Op(format!("cannot read stdin: {e}")))?;
            if line.is_empty() {
                break;
            }
            completions.push(line);
        }
    }
    let body = json!({"prompt": text, "completions": completions, "timeout_ms": timeout_ms});
    let resp = ctx.client.post("/gui/prompt", &body)?;
    match resp["event"].as_str() {
        Some("confirm") => {
            println!("{}", resp["text"].as_str().unwrap_or_default());
            Ok(0)
        }
        Some(other) => Err(CliError::Op(format!("prompt ended: {other}"))),
        None => Err(CliError::Op("malformed GUI response".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_base_url_explicit_wins() {
        let url = base_url(Some("http://127.0.0.1:9999".into()), &[]);
        assert_eq!(url, "http://127.0.0.1:9999");
    }

    #[test]
    fn test_base_url_reads_the_first_existing_port_file() {
        let dir = std::env::temp_dir().join(format!("mf_gui_port_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("missing/gui.port");
        let present = dir.join("gui.port");
        std::fs::write(&present, "7600\n").unwrap();
        assert_eq!(base_url(None, &[missing, present]), "http://127.0.0.1:7600");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_base_url_falls_back_to_the_default_port() {
        let bogus = PathBuf::from("/nonexistent/gui.port");
        assert_eq!(base_url(None, &[bogus]), "http://127.0.0.1:7524");
    }

    #[test]
    fn test_base_url_ignores_unparsable_port_files() {
        let dir = std::env::temp_dir().join(format!("mf_gui_bad_port_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("gui.port");
        std::fs::write(&bad, "not a port").unwrap();
        assert_eq!(base_url(None, &[bad]), "http://127.0.0.1:7524");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
