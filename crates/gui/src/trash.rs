//! Trash-bin Tauri commands (spec-trash.org "GUI"). The filesystem layer is
//! shared with the CLI ([`metafolder_core::trash`]); this module is the GUI
//! glue: it resolves the repo's `internal_dir`/`root` and a metarecord's path
//! through the [`DaemonProxy`], then drives `TrashDir`. Like the CLI, the daemon
//! is never asked to touch files — only queried for locations and to re-link the
//! metarecord after a restore.

use crate::commands::App;
use crate::daemon_proxy::DaemonProxy;
use metafolder_core::trash::{DaemonClient, DaemonError, PruneMode, Reason, TrashDir, TrashEntry};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// `GET /repos/:repo`, erroring unless it is a 200 with a body.
async fn repo_info(daemon: &DaemonProxy, repo: &str) -> Result<Value, String> {
    let response = daemon.request("GET", &format!("/repos/{repo}"), None).await?;
    if response.status != 200 {
        return Err(response.body["error"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| format!("cannot read repository {repo} (HTTP {})", response.status)));
    }
    Ok(response.body)
}

/// Extracts the repo `root` and `internal_dir` from a `GET /repos/:repo` body.
fn root_and_internal(info: &Value) -> Result<(String, String), String> {
    let root = info["root"]
        .as_str()
        .ok_or("the daemon did not report the repo root")?
        .to_string();
    let internal = info["internal_dir"]
        .as_str()
        .ok_or("the daemon did not report the repo internal_dir")?
        .to_string();
    Ok((root, internal))
}

/// The `TrashDir` for an `internal_dir` (`internal/trash/`).
fn trash_dir(internal: &str) -> TrashDir {
    TrashDir::new(Path::new(internal).join("trash"))
}

/// Absolute path of a repo-relative (root-relative, no leading slash — the
/// [`paths_of`] shape) path returned by `resolve-tree`.
fn abs_path(root: &str, rel: &str) -> PathBuf {
    PathBuf::from(root).join(rel.trim_start_matches('/'))
}

/// Parses a `selected_metarecord` workspace var (`{uuid, repo}` | null).
fn parse_selected(value: &Value) -> Option<(String, String)> {
    let uuid = value.get("uuid")?.as_str()?.to_string();
    let repo = value.get("repo")?.as_str()?.to_string();
    Some((uuid, repo))
}

// ── Blocking daemon client for the shared re-link glue ───────────────────────
//
// Core's trash re-link orchestration ([`metafolder_core::trash`]) is
// synchronous, so — like `core::sync` — it rides a blocking `ureq` client on a
// `spawn_blocking` thread rather than the async [`DaemonProxy`]. Mirrors the
// CLI's client (auth token, `{"error": …}` bodies → the message, transport too).
struct BlockingClient {
    base: String,
    token: Option<String>,
    agent: ureq::Agent,
}

impl BlockingClient {
    fn new(base: String) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
            token: metafolder_core::auth::read_token("daemon").ok(),
            agent: ureq::Agent::new(),
        }
    }
}

impl DaemonClient for BlockingClient {
    fn request(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, DaemonError> {
        let url = format!("{}{}", self.base, path);
        let mut req = self.agent.request(method, &url);
        if let Some(token) = &self.token {
            req = req.set("Authorization", &format!("Bearer {token}"));
        }
        let result = match body {
            Some(json) => req.send_json(json),
            None => req.call(),
        };
        match result {
            Ok(response) => Ok(response.into_json().unwrap_or(Value::Null)),
            Err(ureq::Error::Status(code, response)) => {
                let body: Value = response.into_json().unwrap_or(Value::Null);
                let message = body["error"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("daemon returned HTTP {code}"));
                Err(DaemonError { status: Some(code), message })
            }
            Err(ureq::Error::Transport(t)) => {
                Err(DaemonError { status: None, message: format!("cannot reach the daemon at {}: {t}", self.base) })
            }
        }
    }
}

/// The selected metarecord's first `mfr_path` (root-relative), via `resolve-tree`.
/// `None` when the file is gone (`mfr_path` absent or `Nothing`).
fn first_mfr_path(client: &BlockingClient, repo: &str, uuid: &str) -> Result<Option<String>, String> {
    let resp = client
        .get(&format!("/repos/{repo}/metarecords/{uuid}/fields/mfr_path/resolve-tree"))
        .map_err(|e| e.message)?;
    Ok(resp["paths"]
        .as_array()
        .and_then(|paths| paths.first())
        .and_then(Value::as_str)
        .map(str::to_string))
}

/// Blocking worker behind [`trash_selected_metarecord`]: resolves the selected
/// metarecord's file, captures its subtree, and moves it into the trash.
/// Returns the trashed basename.
fn trash_selected_blocking(base: String, uuid: String, repo: String) -> Result<String, String> {
    let client = BlockingClient::new(base);
    let info = client.get(&format!("/repos/{repo}")).map_err(|e| e.message)?;
    let (root, internal) = root_and_internal(&info)?;

    let rel = first_mfr_path(&client, &repo, &uuid)?
        .ok_or("the selected metarecord has no file (already deleted)")?;
    let abs = abs_path(&root, &rel);
    let name =
        abs.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| rel.clone());

    // The top record's version (rollback correlation) and the whole subtree,
    // captured *before* the move while every metarecord is still linked.
    let record = client.get(&format!("/repos/{repo}/metarecords/{uuid}")).map_err(|e| e.message)?;
    let version = record["version"].as_u64();
    let subtree =
        metafolder_core::trash::capture_nodes(&client, &repo, &record, &rel).map_err(|e| e.message)?;

    let dir = trash_dir(&internal);
    let entry = dir.trash_path(&abs, Reason::Manual, None, Some(uuid), version).map_err(|e| e.0)?;
    dir.attach_subtree(&entry.id, subtree).map_err(|e| e.0)?;
    Ok(name)
}

/// Blocking worker behind [`trash_restore`]: validates the restore, re-links the
/// metarecords, then moves the blob back. Returns the restored path.
fn restore_blocking(base: String, repo: String, id: String) -> Result<String, String> {
    let client = BlockingClient::new(base);
    let info = client.get(&format!("/repos/{repo}")).map_err(|e| e.message)?;
    let (root, internal) = root_and_internal(&info)?;
    let dir = trash_dir(&internal);
    let entry = dir.entry(&id).map_err(|e| e.0)?;

    // Validate the restore can proceed before re-linking (so we don't re-link a
    // metarecord to a path a refused restore never fills); re-link *before* the
    // move so the metarecord already claims the path (spec-trash).
    dir.preflight_restore(&id).map_err(|e| e.0)?;
    let rel = Path::new(&entry.original_path)
        .strip_prefix(&root)
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| entry.original_name.clone());
    metafolder_core::trash::restore_relink(&client, &repo, &entry, &rel).map_err(|e| e.message)?;

    let restored = dir.restore(&id).map_err(|e| e.0)?;
    Ok(restored.display().to_string())
}

// ── Tauri commands ───────────────────────────────────────────────────────────

/// Lists the repo's trash entries, newest first.
#[tauri::command]
pub async fn trash_list(
    app: tauri::State<'_, Arc<App>>,
    repo: String,
) -> Result<Vec<TrashEntry>, String> {
    let info = repo_info(&app.daemon, &repo).await?;
    let (_root, internal) = root_and_internal(&info)?;
    let mut entries = trash_dir(&internal).entries().map_err(|e| e.0)?;
    entries.sort_by_key(|e| std::cmp::Reverse(e.trashed_at));
    Ok(entries)
}

/// Sends the file of the workspace's `selected_metarecord` to the trash
/// (`reason = manual`). The confirmation is the caller's (the shell); this posts
/// the outcome to the status bar and marks metarecords dirty so lists refresh.
#[tauri::command]
pub async fn trash_selected_metarecord(
    app: tauri::State<'_, Arc<App>>,
    ws_id: String,
) -> Result<(), String> {
    let timeouts = app.status_timeouts();
    let base = app.daemon.base_url();
    // Resolve the selection synchronously (in-memory state), then do the daemon +
    // filesystem work off the async runtime (core's glue is blocking).
    let selected = app
        .gui
        .get_var(&ws_id, "selected_metarecord")
        .ok()
        .and_then(|v| parse_selected(&v));
    let result = match selected {
        Some((uuid, repo)) => tokio::task::spawn_blocking(move || {
            trash_selected_blocking(base, uuid, repo)
        })
        .await
        .map_err(|e| format!("trash task panicked: {e}"))?,
        None => Err("no metarecord is selected in this workspace".to_string()),
    };
    match &result {
        Ok(name) => {
            app.gui.post_status(
                &ws_id,
                &format!("Trashed {name} — restore it from the trash panel"),
                "info",
                Some(timeouts.message_ms),
            )?;
            // Refresh any metarecord/file listing showing this repo.
            app.gui.set_var(&ws_id, "metarecords:dirty", json!(now_ms()))?;
        }
        Err(error) => {
            app.gui.post_status(&ws_id, error, "error", Some(timeouts.error_ms))?;
        }
    }
    result.map(|_| ())
}

/// Sends a raw filesystem path (tracked or not) to the repo's trash. Used by the
/// file-manager panel's delete, which operates on the disk directly (spec-gui
/// "file-manager panel type"): the blob is captured as a manual entry with no
/// metarecord correlation, so a later restore just puts the bytes back where any
/// stale metarecord still points. Returns the trashed basename.
#[tauri::command]
pub async fn trash_path(
    app: tauri::State<'_, Arc<App>>,
    repo: String,
    path: String,
) -> Result<String, String> {
    let info = repo_info(&app.daemon, &repo).await?;
    let (_root, internal) = root_and_internal(&info)?;
    tokio::task::spawn_blocking(move || {
        let entry = trash_dir(&internal)
            .trash_path(Path::new(&path), Reason::Manual, None, None, None)
            .map_err(|e| e.0)?;
        Ok(entry.original_name)
    })
    .await
    .map_err(|e| format!("trash task panicked: {e}"))?
}

/// Restores a trash entry to its original path (re-linking the metarecords).
/// Returns the restored path for display.
#[tauri::command]
pub async fn trash_restore(
    app: tauri::State<'_, Arc<App>>,
    repo: String,
    id: String,
) -> Result<String, String> {
    let base = app.daemon.base_url();
    tokio::task::spawn_blocking(move || restore_blocking(base, repo, id))
        .await
        .map_err(|e| format!("restore task panicked: {e}"))?
}

/// Permanently deletes a single trash entry.
#[tauri::command]
pub async fn trash_remove(
    app: tauri::State<'_, Arc<App>>,
    repo: String,
    id: String,
) -> Result<(), String> {
    let info = repo_info(&app.daemon, &repo).await?;
    let (_root, internal) = root_and_internal(&info)?;
    trash_dir(&internal).remove(&id).map_err(|e| e.0)
}

/// Empties the trash (also sweeping orphan blobs). Returns the entry count.
#[tauri::command]
pub async fn trash_empty(app: tauri::State<'_, Arc<App>>, repo: String) -> Result<usize, String> {
    let info = repo_info(&app.daemon, &repo).await?;
    let (_root, internal) = root_and_internal(&info)?;
    let removed = trash_dir(&internal).prune(PruneMode::All, false).map_err(|e| e.0)?;
    Ok(removed.len())
}

/// Milliseconds since the Unix epoch (the `metarecords:dirty` nonce).
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn root_and_internal_reads_both() {
        let info = json!({"root": "/data/music", "internal_dir": "/data/music/.metafolder/internal"});
        let (root, internal) = root_and_internal(&info).unwrap();
        assert_eq!(root, "/data/music");
        assert_eq!(internal, "/data/music/.metafolder/internal");
    }

    #[test]
    fn root_and_internal_errors_when_missing() {
        assert!(root_and_internal(&json!({"root": "/x"})).is_err());
        assert!(root_and_internal(&json!({"internal_dir": "/x"})).is_err());
    }

    #[test]
    fn abs_path_joins_root_relative() {
        assert_eq!(abs_path("/data", "music/song.mp3"), PathBuf::from("/data/music/song.mp3"));
        // Defensive against a leading slash.
        assert_eq!(abs_path("/data", "/song.mp3"), PathBuf::from("/data/song.mp3"));
        assert_eq!(abs_path("/data", "song.mp3"), PathBuf::from("/data/song.mp3"));
    }

    #[test]
    fn parse_selected_reads_uuid_and_repo() {
        let value = json!({"uuid": "abc", "repo": "r1"});
        assert_eq!(parse_selected(&value), Some(("abc".to_string(), "r1".to_string())));
        assert_eq!(parse_selected(&Value::Null), None);
        assert_eq!(parse_selected(&json!({"uuid": "abc"})), None);
    }
}
