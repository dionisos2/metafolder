//! Trash-bin Tauri commands (doc "Trash"). The filesystem layer is
//! shared with the CLI ([`metafolder_core::trash`]); this module is the GUI
//! glue: it resolves the repo's `internal_dir`/`root` and a metarecord's path
//! through the [`DaemonProxy`], then drives `TrashDir`. Like the CLI, the daemon
//! is never asked to touch files — only queried for locations and to re-link the
//! metarecord after a restore.

use crate::blocking_client::BlockingClient;
use crate::commands::App;
use crate::daemon_proxy::DaemonProxy;
use metafolder_core::daemon_client::DaemonClient;
use metafolder_core::trash::{PruneMode, Reason, TrashDir, TrashEntry};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// `GET /repos/:repo`, erroring unless it is a 200 with a body.
async fn repo_info(daemon: &DaemonProxy, repo: &str) -> Result<Value, String> {
    let response = daemon.request("GET", &format!("/repos/{repo}"), None).await?;
    if response.status != 200 {
        return Err(crate::daemon_proxy::error_message(&response.body, || {
            format!("cannot read repository {repo} (HTTP {})", response.status)
        }));
    }
    Ok(response.body)
}

/// Extracts the repo `root` and `internal_dir` from a `GET /repos/:repo` body.
fn root_and_internal(info: &Value) -> Result<(String, String), String> {
    let root = info["root"].as_str().ok_or("the daemon did not report the repo root")?.to_string();
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

/// Absolute path of a repo-root-relative path returned by `resolve-tree` (the
/// [`paths_of`] shape — leading-`/`-rooted for the filesystem forest; the
/// leading `/` is trimmed here so it joins onto `root`).
fn abs_path(root: &str, rel: &str) -> PathBuf {
    PathBuf::from(root).join(rel.trim_start_matches('/'))
}

/// Parses a `selected_metarecord` workspace var (`{uuid, repo}` | null).
fn parse_selected(value: &Value) -> Option<(String, String)> {
    let uuid = value.get("uuid")?.as_str()?.to_string();
    let repo = value.get("repo")?.as_str()?.to_string();
    Some((uuid, repo))
}

/// The selected metarecord's first `mfr_path` (root-relative), via `resolve-tree`.
/// `None` when the file is gone (`mfr_path` absent or `Nothing`).
fn first_mfr_path(
    client: &BlockingClient,
    repo: &str,
    uuid: &str,
) -> Result<Option<String>, String> {
    let resp = client
        .get(&format!("/repos/{repo}/metarecords/{uuid}/fields/mfr_path/resolve-tree"))
        .map_err(|e| e.message)?;
    Ok(resp["paths"]
        .as_array()
        .and_then(|paths| paths.first())
        .and_then(Value::as_str)
        .map(str::to_string))
}

/// Trashes `abs`, taking the repository's metarecords for it with it. Returns
/// the trashed basename.
///
/// This is the whole of the GUI's trashing, whichever way the user reached it:
/// the file-manager deletes a raw path and the metarecord panel deletes a
/// selected record, but a *tracked* path has to lose its metarecords either way.
/// Otherwise the file vanishes, the watcher finds a metarecord pointing at
/// nothing, and it orphans the very record the redesign stopped orphaning.
///
/// `root` places `abs` inside the repository, which is what lets a metarecord be
/// looked up for it; a path outside (or a `uuid` already in hand) skips that
/// step. `uuid` is the record when the caller already knows it, otherwise the
/// path is resolved.
///
/// The order is the spec's (doc "What trashing does"): capture
/// while everything is still linked, delete through the daemon, and only then
/// move the bytes — deleting before moving is what leaves the watcher nothing
/// to orphan when the file disappears. An untracked path has no metarecord and
/// only its bytes move.
pub fn trash_tracked(
    base: &str,
    repo: &str,
    internal: &str,
    abs: &Path,
    root: Option<&Path>,
    uuid: Option<String>,
) -> Result<String, String> {
    let client = BlockingClient::new(base.to_string());
    // The path the repository knows this file by. `strip_prefix` fails for a
    // path outside the root — nothing there is tracked.
    let rel = root.and_then(|r| abs.strip_prefix(r).ok()).map(|p| p.to_string_lossy().into_owned());
    let uuid = match (uuid, &rel) {
        (Some(u), _) => Some(u),
        (None, Some(rel)) => {
            metafolder_core::trash::metarecord_at_path(&client, repo, rel).map_err(|e| e.message)?
        }
        (None, None) => None,
    };

    let mut version = None;
    let mut subtree = Vec::new();
    if let (Some(uuid), Some(rel)) = (&uuid, &rel) {
        // Capture first: once the metarecords are gone there is nothing left to
        // read. This takes the target and everything under it — which the
        // trashing deletes — plus its ancestors, which it does not.
        let record =
            client.get(&format!("/repos/{repo}/metarecords/{uuid}")).map_err(|e| e.message)?;
        version = record["version"].as_u64();
        subtree = metafolder_core::trash::capture_nodes(&client, repo, &record, rel)
            .map_err(|e| e.message)?;
        // Then the metadata half, then the bytes. Not forced: something else
        // still pointing at this record is a refusal the user should see, not
        // a reference the GUI silently breaks on their behalf.
        metafolder_core::trash::delete_trashed(&client, repo, &subtree, false)
            .map_err(|e| e.message)?;
    }

    let dir = trash_dir(internal);
    let entry = dir.trash_path(abs, Reason::Manual, None, uuid, version).map_err(|e| e.0)?;
    if !subtree.is_empty() {
        dir.attach_subtree(&entry.id, subtree).map_err(|e| e.0)?;
    }
    Ok(entry.original_name)
}

/// Blocking worker behind [`trash_selected_metarecord`]: resolves the selected
/// metarecord's file, then trashes it through [`trash_tracked`].
fn trash_selected_blocking(base: String, uuid: String, repo: String) -> Result<String, String> {
    let client = BlockingClient::new(base.clone());
    let info = client.get(&format!("/repos/{repo}")).map_err(|e| e.message)?;
    let (root, internal) = root_and_internal(&info)?;

    let rel = first_mfr_path(&client, &repo, &uuid)?
        .ok_or("the selected metarecord has no file (already deleted)")?;
    let abs = abs_path(&root, &rel);
    trash_tracked(&base, &repo, &internal, &abs, Some(Path::new(&root)), Some(uuid))
}

/// What a bulk trashing ([`trash_query`]) did, for the status line.
#[derive(Debug, Default, serde::Serialize)]
pub struct BulkTrashOutcome {
    /// Trash entries made: one per file or directory moved.
    pub trashed: usize,
    /// Metarecords of the set under a directory of the set: they went with it.
    pub inside: usize,
    /// Metarecords of the set with no file (no `mfr_path`, or an orphan).
    pub without_file: usize,
    /// Whether the set held the repository root, which is never trashed.
    pub root_kept: bool,
    /// The paths whose bytes could not be moved, with the reason.
    pub failed: Vec<String>,
}

/// Splits `resolve-tree`'s answer (uuid → paths) into the paths to trash: the
/// first path of each metarecord, minus those under another path of the set
/// (they go with their directory) and the root. Sorted, so a directory comes
/// before what it holds.
fn plan_bulk_trash(
    paths: &serde_json::Map<String, Value>,
) -> (Vec<(String, String)>, BulkTrashOutcome) {
    let mut outcome = BulkTrashOutcome::default();
    let mut items: Vec<(String, String)> = Vec::new();
    for (uuid, list) in paths {
        match list.as_array().and_then(|l| l.first()).and_then(Value::as_str) {
            None => outcome.without_file += 1,
            Some("") => outcome.root_kept = true,
            Some(rel) => items.push((rel.to_string(), uuid.clone())),
        }
    }
    items.sort();
    let mut kept: Vec<(String, String)> = Vec::new();
    for (rel, uuid) in items {
        let under = kept.iter().any(|(dir, _)| rel.starts_with(&format!("{dir}/")));
        if under {
            outcome.inside += 1;
        } else {
            kept.push((rel, uuid));
        }
    }
    (kept.into_iter().map(|(rel, uuid)| (uuid, rel)).collect(), outcome)
}

/// Trashes the files of every metarecord `query` matches (doc "Sending files
/// to the trash"): the order of [`trash_tracked`], over a set. Everything is
/// captured first, then the metarecords are deleted in **one** call — one
/// revision to undo, and a reference from one member of the set to another is
/// not a refusal — and only then do the bytes move, one trash entry per file
/// or directory. A refusal from the daemon therefore leaves every file where
/// it was; a move that fails afterwards is reported in `failed` and the others
/// go on.
pub fn trash_query(base: &str, repo: &str, query: &Value) -> Result<BulkTrashOutcome, String> {
    let client = BlockingClient::new(base.to_string());
    let info = client.get(&format!("/repos/{repo}")).map_err(|e| e.message)?;
    let (root, internal) = root_and_internal(&info)?;
    let resolved = client
        .post(
            &format!("/repos/{repo}/query/fields/resolve-tree"),
            &serde_json::json!({"query": query, "field": "mfr_path"}),
        )
        .map_err(|e| e.message)?;
    let paths = resolved.as_object().ok_or("the daemon did not answer with paths")?;
    let (items, mut outcome) = plan_bulk_trash(paths);

    // Capture: the record (for its version) and its subtree, per item.
    let mut captured = Vec::with_capacity(items.len());
    let mut doomed: Vec<metafolder_core::trash::TrashedNode> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (uuid, rel) in &items {
        let record =
            client.get(&format!("/repos/{repo}/metarecords/{uuid}")).map_err(|e| e.message)?;
        let subtree = metafolder_core::trash::capture_nodes(&client, repo, &record, rel)
            .map_err(|e| e.message)?;
        for node in subtree.iter().filter(|n| n.trashed) {
            if seen.insert(node.uuid.clone()) {
                doomed.push(node.clone());
            }
        }
        captured.push((uuid.clone(), rel.clone(), record["version"].as_u64(), subtree));
    }
    metafolder_core::trash::delete_trashed(&client, repo, &doomed, false).map_err(|e| e.message)?;

    let dir = trash_dir(&internal);
    for (uuid, rel, version, subtree) in captured {
        let abs = abs_path(&root, &rel);
        let moved =
            dir.trash_path(&abs, Reason::Manual, None, Some(uuid), version).and_then(|entry| {
                if subtree.is_empty() {
                    Ok(())
                } else {
                    dir.attach_subtree(&entry.id, subtree)
                }
            });
        match moved {
            Ok(()) => outcome.trashed += 1,
            Err(e) => outcome.failed.push(format!("{rel}: {}", e.0)),
        }
    }
    Ok(outcome)
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
    // move so the metarecord already claims the path (doc "Trash").
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
    let selected =
        app.gui.get_var(&ws_id, "selected_metarecord").ok().and_then(|v| parse_selected(&v));
    let result = match selected {
        Some((uuid, repo)) => {
            tokio::task::spawn_blocking(move || trash_selected_blocking(base, uuid, repo))
                .await
                .map_err(|e| format!("trash task panicked: {e}"))?
        }
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
            app.gui.mark_metarecords_dirty(&ws_id)?;
        }
        Err(error) => {
            app.gui.post_status(&ws_id, error, "error", Some(timeouts.error_ms))?;
        }
    }
    result.map(|_| ())
}

/// Sends the files of every metarecord `query` matches to the trash
/// ([`trash_query`]) — the bulk form of [`trash_selected_metarecord`], behind
/// `metarecord:bulk <target> trash`. The confirmation is the caller's; this
/// posts the outcome to the status bar and marks metarecords dirty.
#[tauri::command]
pub async fn trash_query_metarecords(
    app: tauri::State<'_, Arc<App>>,
    ws_id: String,
    repo: String,
    query: Value,
) -> Result<BulkTrashOutcome, String> {
    let timeouts = app.status_timeouts();
    let base = app.daemon.base_url();
    let result = tokio::task::spawn_blocking(move || trash_query(&base, &repo, &query))
        .await
        .map_err(|e| format!("trash task panicked: {e}"))?;
    match &result {
        Ok(outcome) => {
            app.gui.post_status(
                &ws_id,
                &bulk_trash_message(outcome),
                if outcome.failed.is_empty() { "info" } else { "error" },
                Some(timeouts.message_ms),
            )?;
            app.gui.mark_metarecords_dirty(&ws_id)?;
        }
        Err(error) => {
            app.gui.post_status(&ws_id, error, "error", Some(timeouts.error_ms))?;
        }
    }
    result
}

/// The status line of a bulk trashing: what moved, then what did not and why.
fn bulk_trash_message(outcome: &BulkTrashOutcome) -> String {
    let plural =
        |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    let mut parts = vec![format!(
        "Trashed {} — restore from the trash panel",
        plural(outcome.trashed, "file", "files")
    )];
    if outcome.inside > 0 {
        parts.push(format!(
            "{} inside a trashed folder",
            plural(outcome.inside, "metarecord", "metarecords")
        ));
    }
    if outcome.without_file > 0 {
        parts.push(format!(
            "{} without a file kept",
            plural(outcome.without_file, "metarecord", "metarecords")
        ));
    }
    if outcome.root_kept {
        parts.push("the repository root kept".to_string());
    }
    if !outcome.failed.is_empty() {
        parts.push(format!("failed: {}", outcome.failed.join("; ")));
    }
    parts.join(" · ")
}

/// Sends a raw filesystem path to the repo's trash. Used by the file-manager
/// panel's delete, which operates on the disk directly
/// (doc "file-manager panel").
///
/// Operating on a path does not mean operating behind the repository's back: if
/// a metarecord tracks that path it is trashed along with the bytes, exactly as
/// deleting the record itself would (doc "Trash"). An untracked path has none and
/// only its bytes move. Returns the trashed basename.
#[tauri::command]
pub async fn trash_path(
    app: tauri::State<'_, Arc<App>>,
    repo: String,
    path: String,
) -> Result<String, String> {
    let info = repo_info(&app.daemon, &repo).await?;
    let (root, internal) = root_and_internal(&info)?;
    let base = app.daemon.base_url();
    tokio::task::spawn_blocking(move || {
        trash_tracked(&base, &repo, &internal, Path::new(&path), Some(Path::new(&root)), None)
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn root_and_internal_reads_both() {
        let info =
            json!({"root": "/data/music", "internal_dir": "/data/music/.metafolder/internal"});
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
