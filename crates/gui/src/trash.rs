//! Trash-bin Tauri commands (spec-trash.org "GUI"). The filesystem layer is
//! shared with the CLI ([`metafolder_core::trash`]); this module is the GUI
//! glue: it resolves the repo's `internal_dir`/`root` and a metarecord's path
//! through the [`DaemonProxy`], then drives `TrashDir`. Like the CLI, the daemon
//! is never asked to touch files — only queried for locations and to re-link the
//! metarecord after a restore.

use crate::commands::App;
use crate::daemon_proxy::{DaemonProxy, ProxyResponse};
use metafolder_core::trash::{PruneMode, Reason, TrashDir, TrashEntry, TrashedNode};
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

/// The filesystem root metarecord's uuid: the `mfr_path` forest root whose name
/// is `""` (created by repo init). Every top-level file hangs off it — reconcile
/// starts `ensure_parent_metarecords` there — so a top-level restore must
/// re-link under it, never under the root sentinel (`parent = None`), which
/// would forge a second forest root.
async fn repo_root_metarecord(daemon: &DaemonProxy, repo: &str) -> Result<Uuid, String> {
    let response = daemon
        .request("GET", &format!("/repos/{repo}/tree/roots?field=mfr_path"), None)
        .await?;
    let hex = response.body
        .as_array()
        .and_then(|rs| rs.iter().find(|r| r["name"].as_str() == Some("")))
        .and_then(|r| r["uuid"].as_str())
        .ok_or("repository has no filesystem root metarecord")?;
    Uuid::parse_str(hex).map_err(|_| "daemon returned an invalid root uuid".to_string())
}

/// Reads a metarecord JSON (`{uuid, fields}`) into a [`TrashedNode`] — its uuid
/// plus its first `mfr_path` TreeRef — or None when it has no present tree_ref
/// `mfr_path`.
fn subtree_node(record: &Value) -> Option<TrashedNode> {
    let uuid = record["uuid"].as_str()?;
    let mfr = record["fields"].as_array()?.iter().find(|f| f["name"] == "mfr_path")?;
    let value = &mfr["value"];
    if value["type"].as_str()? != "tree_ref" {
        return None;
    }
    Some(TrashedNode {
        uuid: uuid.to_string(),
        parent: value["value"]["parent"].as_str().map(str::to_owned),
        name: value["value"]["name"].as_str()?.to_string(),
    })
}

/// Captures the metarecords of a trashed subtree with their original `mfr_path`
/// TreeRefs — the target (`top`) plus every descendant (empty for a plain file)
/// Walks the `mfr_path` parent chain from `parent` upward, capturing each
/// ancestor directory metarecord's TreeRef. Stops at (and excludes) the forest
/// root (the node whose parent is `None`). Bounded by the forest depth limit.
async fn capture_ancestors(
    daemon: &DaemonProxy,
    repo: &str,
    mut parent: Option<String>,
) -> Result<Vec<TrashedNode>, String> {
    let mut nodes = Vec::new();
    for _ in 0..1000 {
        let Some(uuid) = parent else { break };
        let record = repo_info_metarecord(daemon, repo, &uuid).await?;
        let Some(node) = subtree_node(&record) else { break };
        // The forest root (parent = None, name = "") is always live: stop here.
        if node.parent.is_none() {
            break;
        }
        parent = node.parent.clone();
        nodes.push(node);
    }
    Ok(nodes)
}

/// — so a restore re-links the whole tree, not just the top metarecord.
async fn capture_subtree(
    daemon: &DaemonProxy,
    repo: &str,
    top: &Value,
    root: &str,
    abs: &Path,
) -> Result<Vec<TrashedNode>, String> {
    let mut nodes = Vec::new();
    if let Some(node) = subtree_node(top) {
        // Ancestor directory metarecords, up to (but not including) the forest
        // root: if a parent directory is later trashed too, restoring this
        // nested item re-links the ancestor so its recreated directory is
        // tracked by the original metarecord rather than left orphaned for the
        // watcher to duplicate. Live ancestors are skipped at re-link time.
        nodes.extend(capture_ancestors(daemon, repo, node.parent.clone()).await?);
        nodes.push(node);
    }
    let rel = abs
        .strip_prefix(root)
        .map_err(|_| format!("{} is outside the repository", abs.display()))?;
    let comps: Vec<String> = rel
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(n) => Some(n.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    let target = format!("/{}", comps.join("/"));
    let query = json!({"type": "follows_transitive", "field": "mfr_path", "target": target});
    let mut cursor: Option<String> = None;
    loop {
        let mut body = json!({"query": query, "select": ["mfr_path"], "limit": 500});
        if let Some(c) = &cursor {
            body["cursor"] = json!(c);
        }
        let response =
            daemon.request("POST", &format!("/repos/{repo}/query"), Some(body)).await?;
        for obj in response.body["results"].as_array().into_iter().flatten() {
            if let Some(node) = subtree_node(obj) {
                nodes.push(node);
            }
        }
        match response.body["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }
    Ok(nodes)
}

/// Re-links every metarecord of a trashed subtree to its recorded `mfr_path`
/// TreeRef, so the directory and all its descendants return to where they were.
/// A node still carrying an `mfr_path` (reused since the trashing) is left
/// alone, and a node whose tree position is already held by another metarecord
/// is skipped; any other write failure is surfaced rather than swallowed.
async fn relink_subtree(
    daemon: &DaemonProxy,
    repo: &str,
    subtree: &[TrashedNode],
) -> Result<(), String> {
    // Parent-before-child: a child re-linked before its parent (still orphaned)
    // is rejected by the forest validation and left orphaned.
    for node in metafolder_core::trash::relink_order(subtree) {
        let node = &node;
        let uuid = Uuid::parse_str(&node.uuid).map_err(|_| "invalid subtree uuid")?;
        // The metarecord may be gone (deleted while trashed): skip it, the
        // watcher makes a fresh one. Re-link only an orphaned metarecord.
        let Ok(record) = repo_info_metarecord(daemon, repo, &node.uuid).await else {
            continue;
        };
        if has_mfr_path(&record) {
            continue;
        }
        let parent = node
            .parent
            .as_deref()
            .map(Uuid::parse_str)
            .transpose()
            .map_err(|_| "invalid subtree parent uuid")?;
        let value = serde_json::to_value(metafolder_core::metarecord::Value::TreeRef {
            parent,
            name: node.name.clone(),
        })
        .map_err(|e| e.to_string())?;
        let response = daemon
            .request(
                "PUT",
                &format!("/repos/{repo}/metarecords/{}/fields/mfr_path", uuid.as_simple()),
                Some(json!({"value": value, "force": true})),
            )
            .await?;
        check_relink(&response)?; // skip the expected conflict, surface anything else
    }
    Ok(())
}

/// Interprets a `PUT …/mfr_path` re-link response: `Ok` on success or on the
/// expected "tree position already occupied" conflict (benign — another
/// metarecord already holds the path and will track the restored bytes); `Err`
/// on any other failure, so a genuine problem is surfaced rather than hidden.
fn check_relink(response: &ProxyResponse) -> Result<(), String> {
    if (200..300).contains(&response.status) {
        return Ok(());
    }
    let message = response.body["error"].as_str().unwrap_or("");
    if message.contains("already occupied") {
        return Ok(());
    }
    Err(format!("re-link failed (HTTP {}): {message}", response.status))
}

/// After a restore, re-link the associated metarecord to `restored` by writing
/// its `mfr_path` (authoritative, mirroring the CLI's `relink_after_restore`).
/// A no-op when the metarecord still has an `mfr_path`, when it is gone, or when
/// the path/parent is untracked; the expected "position already taken" conflict
/// is skipped, and any other write failure is surfaced.
async fn relink_after_restore(
    daemon: &DaemonProxy,
    repo: &str,
    root: &str,
    metarecord: &str,
    restored: &Path,
) -> Result<(), String> {
    // A gone metarecord (deleted while trashed) is skipped — the watcher makes
    // a fresh one. Only re-link an orphaned metarecord (mfr_path absent/Nothing).
    let Ok(record) = repo_info_metarecord(daemon, repo, metarecord).await else {
        return Ok(());
    };
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
        // Top-level file: its parent is the filesystem root metarecord, exactly
        // as reconcile assigns it — not None, which would forge a second forest
        // root and leave the file to be re-tracked as a duplicate.
        Some(repo_root_metarecord(daemon, repo).await?)
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
    let response = daemon
        .request(
            "PUT",
            &format!("/repos/{repo}/metarecords/{}/fields/mfr_path", uuid.as_simple()),
            Some(json!({"value": value, "force": true})),
        )
        .await?;
    check_relink(&response) // skip the expected conflict, surface anything else
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

    // The metarecord's current version (for rollback correlation, spec-trash)
    // and its whole subtree, captured *before* the move while every metarecord
    // is still linked, so a restore re-links the directory and all its
    // descendants — not just the top metarecord.
    let record = repo_info_metarecord(&app.daemon, &repo, &uuid).await.ok();
    let version = record.as_ref().and_then(|r| r["version"].as_u64());
    let subtree = match &record {
        Some(rec) => capture_subtree(&app.daemon, &repo, rec, &root, &abs).await.unwrap_or_default(),
        None => Vec::new(),
    };

    let entry = trash_dir(&internal)
        .trash_path(&abs, Reason::Manual, None, Some(uuid), version)
        .map_err(|e| e.0)?;
    trash_dir(&internal).attach_subtree(&entry.id, subtree).map_err(|e| e.0)?;
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

    // Check the restore can proceed (a free target, or a mergeable directory)
    // before re-linking, so we don't re-link a metarecord to a path a refused
    // restore never fills. Re-linking happens before the move so the metarecord
    // already claims the path and the watcher sees a plain refresh rather than
    // fingerprint-searching or creating a duplicate (spec-trash).
    let target = PathBuf::from(&entry.original_path);
    dir.preflight_restore(&id).map_err(|e| e.0)?;
    if !entry.subtree.is_empty() {
        // The whole subtree was recorded at trash time: re-link every node to
        // its original TreeRef (the directory and all its descendants).
        relink_subtree(&app.daemon, &repo, &entry.subtree).await?;
    } else if let Some(metarecord) = &entry.metarecord {
        // A pre-subtree entry (or a plain single file): fall back to the
        // path-resolving re-link of the one recorded metarecord.
        relink_after_restore(&app.daemon, &repo, &root, metarecord, &target).await?;
    }
    let restored = dir.restore(&id).map_err(|e| e.0)?;
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
