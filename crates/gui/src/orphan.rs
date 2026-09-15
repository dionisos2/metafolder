//! `orphan:detect` / `orphan:delete` / `orphan:detect-delete` (spec-gui
//! "Orphans"): the GUI half of `mf orphan detect`.
//!
//! Detection is the daemon's (`POST /orphans/mark`): it needs the disk. What it
//! writes is an ordinary field, `orphan = true`, so everything downstream is a
//! plain query — listing the orphans is `orphan = true` typed in the DSL zone,
//! and deleting them is `POST /query/delete` over the same predicate. That is
//! why this module holds no set of uuids: the marked set lives in the
//! repository, not in the panel, and survives a restart.
//!
//! The confirmation before a deletion is the shell's (as for `metarecord:trash`)
//! — hence [`count`], which is what the prompt names.

use crate::commands::StatusTimeouts;
use crate::daemon_proxy::DaemonProxy;
use crate::state::GuiState;
use serde_json::{json, Value};
use std::sync::Arc;

/// The marker the daemon writes, and the query everything else here is built on.
const ORPHAN_FIELD: &str = "orphan";

/// `orphan = true` as the query IR.
fn marked_query() -> Value {
    json!({"type": "eq", "field": ORPHAN_FIELD, "value": {"type": "bool", "value": true}})
}

/// The workspace's active repository, or the error the command reports.
fn active_repo(gui: &GuiState, ws_id: &str) -> Result<String, String> {
    match gui.get_var(ws_id, "active_repo")? {
        Value::String(repo) => Ok(repo),
        _ => Err("no active repository in this workspace".into()),
    }
}

/// `orphan:detect` — mark every orphaned metarecord with `orphan = true` and
/// take the marker back from the records that are no longer orphaned. Returns
/// how many carry it afterwards, so the caller can offer the deletion.
pub async fn detect(
    gui: Arc<GuiState>,
    daemon: Arc<DaemonProxy>,
    ws_id: String,
    timeouts: StatusTimeouts,
) -> Result<u64, String> {
    let repo = active_repo(&gui, &ws_id)?;
    gui.post_status(&ws_id, "Looking for orphaned metarecords…", "busy", None)?;
    let response = daemon
        .request("POST", &format!("/repos/{repo}/orphans/mark"), Some(json!({})))
        .await
        .inspect_err(|error| {
            let _ = gui.post_status(&ws_id, error, "error", Some(timeouts.error_ms));
        })?;
    if response.status == 404 {
        // Additive endpoint, so `API_VERSION` is deliberately not bumped and the
        // version banner cannot warn: say it plainly instead.
        return fail(&gui, &ws_id, "this daemon does not support orphan marking", &timeouts);
    }
    if response.status != 200 {
        let message = error_of(&response.body, "orphan detection failed");
        return fail(&gui, &ws_id, &message, &timeouts);
    }
    let count = |key: &str| response.body[key].as_u64().unwrap_or(0);
    let (orphans, marked, unmarked) = (count("orphans"), count("marked"), count("unmarked"));
    let summary = if orphans == 0 {
        "No orphans — every tracked metarecord has its file.".to_string()
    } else {
        format!(
            "{orphans} orphaned metarecord{} marked orphan = true ({marked} new, \
             {unmarked} no longer orphaned).",
            plural(orphans)
        )
    };
    finish(&gui, &ws_id, &summary, &timeouts)?;
    Ok(orphans)
}

/// How many metarecords carry the marker right now — what the deletion prompt
/// names. A plain count query: detection is not re-run.
pub async fn count(
    gui: Arc<GuiState>,
    daemon: Arc<DaemonProxy>,
    ws_id: String,
) -> Result<u64, String> {
    let repo = active_repo(&gui, &ws_id)?;
    let body = json!({"query": marked_query(), "limit": 1, "count": true});
    let response = daemon.request("POST", &format!("/repos/{repo}/query"), Some(body)).await?;
    if response.status != 200 {
        return Err(error_of(&response.body, "counting the marked metarecords failed"));
    }
    Ok(response.body["total"].as_u64().unwrap_or(0))
}

/// `orphan:delete` — delete every metarecord carrying the marker. The files are
/// gone already; what goes is the metadata that was still standing for them.
/// Confirmed by the caller, and rollback-able like any other revision.
pub async fn delete(
    gui: Arc<GuiState>,
    daemon: Arc<DaemonProxy>,
    ws_id: String,
    timeouts: StatusTimeouts,
) -> Result<u64, String> {
    let repo = active_repo(&gui, &ws_id)?;
    gui.post_status(&ws_id, "Deleting the marked metarecords…", "busy", None)?;
    let response = daemon
        .request(
            "POST",
            &format!("/repos/{repo}/query/delete"),
            Some(json!({ "query": marked_query() })),
        )
        .await
        .inspect_err(|error| {
            let _ = gui.post_status(&ws_id, error, "error", Some(timeouts.error_ms));
        })?;
    if response.status != 200 {
        let message = error_of(&response.body, "deleting the marked metarecords failed");
        return fail(&gui, &ws_id, &message, &timeouts);
    }
    let deleted = response.body["deleted"].as_u64().unwrap_or(0);
    let summary =
        format!("Deleted {deleted} orphaned metarecord{} — undo takes them back.", plural(deleted));
    finish(&gui, &ws_id, &summary, &timeouts)?;
    Ok(deleted)
}

fn plural(n: u64) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

fn error_of(body: &Value, fallback: &str) -> String {
    body["error"].as_str().map(str::to_string).unwrap_or_else(|| fallback.to_string())
}

/// Posts the failure and hands it back as the command's error.
fn fail<T>(
    gui: &GuiState,
    ws_id: &str,
    message: &str,
    timeouts: &StatusTimeouts,
) -> Result<T, String> {
    gui.post_status(ws_id, message, "error", Some(timeouts.error_ms))?;
    Err(message.to_string())
}

/// Reports what was done and asks the panels to refresh.
fn finish(
    gui: &GuiState,
    ws_id: &str,
    summary: &str,
    timeouts: &StatusTimeouts,
) -> Result<(), String> {
    gui.post_status(ws_id, summary, "info", Some(timeouts.message_ms))?;
    gui.append_message(ws_id, summary)?;
    // Refresh metarecord-list / metarecord-detail / file-manager.
    gui.set_var(ws_id, "metarecords:dirty", json!(now_ms()))?;
    Ok(())
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[tauri::command]
pub async fn orphan_detect(
    app: tauri::State<'_, Arc<crate::commands::App>>,
    ws_id: String,
) -> Result<u64, String> {
    detect(app.gui.clone(), app.daemon.clone(), ws_id, app.status_timeouts()).await
}

#[tauri::command]
pub async fn orphan_count(
    app: tauri::State<'_, Arc<crate::commands::App>>,
    ws_id: String,
) -> Result<u64, String> {
    count(app.gui.clone(), app.daemon.clone(), ws_id).await
}

#[tauri::command]
pub async fn orphan_delete(
    app: tauri::State<'_, Arc<crate::commands::App>>,
    ws_id: String,
) -> Result<u64, String> {
    delete(app.gui.clone(), app.daemon.clone(), ws_id, app.status_timeouts()).await
}
