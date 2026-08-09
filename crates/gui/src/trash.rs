//! Trash-bin Tauri commands (spec-trash.org "GUI"). The filesystem layer is
//! shared with the CLI ([`metafolder_core::trash`]); this module is the GUI
//! glue: it resolves the repo's `internal_dir`/`root` and a metarecord's path
//! through the [`DaemonProxy`], then drives `TrashDir`. Like the CLI, the daemon
//! is never asked to touch files — only queried for locations and to re-link the
//! metarecord after a restore.

use crate::commands::App;
use crate::daemon_proxy::DaemonProxy;
use metafolder_core::trash::{PruneMode, Reason, TrashDir, TrashEntry};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

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

/// Whether a metarecord body carries a present `mfr_path` (not `Nothing`).
fn has_mfr_path(record: &Value) -> bool {
    record["fields"].as_array().is_some_and(|fields| {
        fields
            .iter()
            .any(|f| f["name"] == "mfr_path" && f["value"]["type"].as_str() != Some("nothing"))
    })
}

/// The metarecord's first `mfr_path` (root-relative), via `resolve-tree`.
/// `None` when the file is gone (`mfr_path` absent or `Nothing`).
async fn first_mfr_path(daemon: &DaemonProxy, repo: &str, uuid: &str) -> Result<Option<String>, String> {
    let response = daemon
        .request(
            "GET",
            &format!("/repos/{repo}/metarecords/{uuid}/fields/mfr_path/resolve-tree"),
            None,
        )
        .await?;
    if response.status != 200 {
        return Err(response.body["error"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| format!("cannot resolve the file path (HTTP {})", response.status)));
    }
    Ok(response.body["paths"]
        .as_array()
        .and_then(|paths| paths.first())
        .and_then(Value::as_str)
        .map(str::to_string))
}

/// Resolves an on-disk path to the uuid of the metarecord whose `mfr_path`
/// tracks it (the exact-path query idiom, mirroring the CLI's `metarecord_at_path`).
async fn metarecord_at_path(
    daemon: &DaemonProxy,
    repo: &str,
    root: &str,
    abs: &Path,
) -> Result<Option<String>, String> {
    let rel = abs
        .strip_prefix(root)
        .map_err(|_| format!("{} is outside the repository root {root}", abs.display()))?;
    let comps: Vec<String> = rel
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(n) => Some(n.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    let Some(name) = comps.last().cloned() else {
        return Ok(None);
    };
    // Parent path in the tree-cache `resolve_path` shape: "/a/b" for /a/b/name,
    // or "" (the forest root) for a top-level file.
    let parent = format!("/{}", comps[..comps.len() - 1].join("/"));
    let parent = if parent == "/" { String::new() } else { parent };

    let query = json!({
        "type": "and",
        "operands": [
            {"type": "follows", "field": "mfr_path", "target": parent},
            {"type": "eq", "field": "mfr_path", "value": {"type": "string", "value": name}},
        ],
    });
    let response = daemon
        .request("POST", &format!("/repos/{repo}/query"), Some(json!({"query": query, "limit": 1})))
        .await?;
    Ok(response.body["results"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .map(str::to_owned))
}

/// After a restore, re-link the associated metarecord to `restored` by writing
/// its `mfr_path` (authoritative, mirroring the CLI's `relink_after_restore`).
/// A no-op when the metarecord still has an `mfr_path`, or when the path/parent
/// is untracked; PUT failures are swallowed (the file is restored regardless).
async fn relink_after_restore(
    daemon: &DaemonProxy,
    repo: &str,
    root: &str,
    metarecord: &str,
    restored: &Path,
) -> Result<(), String> {
    let record = repo_info_metarecord(daemon, repo, metarecord).await?;
    // Only re-link an orphaned metarecord (mfr_path absent or Nothing).
    if has_mfr_path(&record) {
        return Ok(());
    }
    let Ok(rel) = restored.strip_prefix(root) else {
        return Ok(()); // outside the repo — leave it unlinked
    };
    let comps: Vec<String> = rel
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(n) => Some(n.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    let Some(name) = comps.last().cloned() else {
        return Ok(());
    };
    let parent = if comps.len() == 1 {
        None
    } else {
        match metarecord_at_path(daemon, repo, root, restored.parent().unwrap_or(Path::new(root)))
            .await?
        {
            Some(hex) => Some(Uuid::parse_str(&hex).map_err(|_| "invalid parent uuid")?),
            None => return Ok(()), // parent untracked — leave it unlinked
        }
    };
    let uuid = Uuid::parse_str(metarecord).map_err(|_| "invalid metarecord uuid")?;
    let value = serde_json::to_value(metafolder_core::metarecord::Value::TreeRef { parent, name })
        .map_err(|e| e.to_string())?;
    // Best-effort: a re-link clash is not fatal, the bytes are already back.
    let _ = daemon
        .request(
            "PUT",
            &format!("/repos/{repo}/metarecords/{}/fields/mfr_path", uuid.as_simple()),
            Some(json!({"value": value, "force": true})),
        )
        .await;
    Ok(())
}

/// `GET /repos/:repo/metarecords/:uuid`, erroring unless 200.
async fn repo_info_metarecord(daemon: &DaemonProxy, repo: &str, uuid: &str) -> Result<Value, String> {
    let response = daemon
        .request("GET", &format!("/repos/{repo}/metarecords/{uuid}"), None)
        .await?;
    if response.status != 200 {
        return Err(response.body["error"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| format!("cannot read metarecord {uuid} (HTTP {})", response.status)));
    }
    Ok(response.body)
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
    let result = trash_selected_inner(&app, &ws_id).await;
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

/// The work behind [`trash_selected_metarecord`]; returns the trashed basename.
async fn trash_selected_inner(app: &Arc<App>, ws_id: &str) -> Result<String, String> {
    let selected = app.gui.get_var(ws_id, "selected_metarecord")?;
    let (uuid, repo) =
        parse_selected(&selected).ok_or("no metarecord is selected in this workspace")?;

    let info = repo_info(&app.daemon, &repo).await?;
    let (root, internal) = root_and_internal(&info)?;

    let rel = first_mfr_path(&app.daemon, &repo, &uuid)
        .await?
        .ok_or("the selected metarecord has no file (already deleted)")?;
    let abs = abs_path(&root, &rel);
    let name = abs
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| rel.clone());

    // Record the metarecord's current version so a later rollback can correlate
    // this entry with the exact deletion it undoes (spec-trash).
    let version = repo_info_metarecord(&app.daemon, &repo, &uuid)
        .await
        .ok()
        .and_then(|r| r["version"].as_u64());

    trash_dir(&internal)
        .trash_path(&abs, Reason::Manual, None, Some(uuid), version)
        .map_err(|e| e.0)?;
    Ok(name)
}

/// Restores a trash entry to its original path (re-linking the metarecord).
/// Returns the restored path for display.
#[tauri::command]
pub async fn trash_restore(
    app: tauri::State<'_, Arc<App>>,
    repo: String,
    id: String,
) -> Result<String, String> {
    let info = repo_info(&app.daemon, &repo).await?;
    let (root, internal) = root_and_internal(&info)?;
    let dir = trash_dir(&internal);
    let entry = dir.entry(&id).map_err(|e| e.0)?;

    // The target is always free (restore refuses an occupied one), so re-link
    // the metarecord *before* moving the file into place (spec-trash): the
    // metarecord then already claims the path, so the watcher sees a plain
    // refresh rather than fingerprint-searching or creating a duplicate.
    let target = PathBuf::from(&entry.original_path);
    if std::fs::symlink_metadata(&target).is_ok() {
        return Err(format!(
            "{} already exists; restore is refused (move it aside first)",
            target.display()
        ));
    }
    if let Some(metarecord) = &entry.metarecord {
        relink_after_restore(&app.daemon, &repo, &root, metarecord, &target).await?;
    }
    let restored = dir.restore(&id, None).map_err(|e| e.0)?;
    Ok(restored.display().to_string())
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

    #[test]
    fn has_mfr_path_detects_present_and_nothing() {
        let present = json!({"fields": [{"name": "mfr_path", "value": {"type": "tree_ref"}}]});
        assert!(has_mfr_path(&present));
        let nothing = json!({"fields": [{"name": "mfr_path", "value": {"type": "nothing"}}]});
        assert!(!has_mfr_path(&nothing));
        let absent = json!({"fields": [{"name": "rating", "value": {"type": "int", "value": 3}}]});
        assert!(!has_mfr_path(&absent));
    }
}
