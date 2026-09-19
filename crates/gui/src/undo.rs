//! `log:undo` / `log:redo` (spec-gui "Event log"): undo the last change the
//! *user* made to the active repository, and re-apply what a rollback undid.
//!
//! Undo is not "step HEAD back one revision": the watcher writes revisions of
//! its own, and rewinding HEAD past them would unrecord what the filesystem
//! really did. The choice — roll HEAD back, or write the inverse at HEAD — is
//! [`metafolder_core::undo`]'s, shared with `mf log undo`; this module only
//! carries it out and reports it.
//!
//! Redo is its mirror: it takes back the newest *undo*, by moving HEAD forward
//! onto what a rollback unapplied, by rolling back over the revert an undo
//! wrote, or — when the watcher has written since — by reverting that revert.
//! The choice is [`metafolder_core::undo::plan_redo`]'s, shared with
//! `mf log redo`; this module only carries it out and reports it.

use crate::daemon_proxy::DaemonProxy;
use crate::state::GuiState;
use metafolder_core::undo::{self, RedoPlan, UndoPlan};
use serde_json::{json, Value};
use std::sync::Arc;

/// Reads enough of the log to decide what undo should undo — two bounded reads
/// rather than one unbounded one (spec-event-log "mf log undo").
async fn undo_plan(daemon: &DaemonProxy, repo: &str) -> Result<UndoPlan, String> {
    for limit in [undo::WINDOW, undo::WIDE_WINDOW] {
        let log = read_log(daemon, repo, "linear", limit).await?;
        let plan = undo::plan_from_log(&log);
        if plan != UndoPlan::Nothing || !undo::window_exhausted(&log, limit) {
            return Ok(plan);
        }
    }
    Ok(UndoPlan::Nothing)
}

/// The same for redo, over the *active* line: it carries HEAD's forward
/// continuation, which is what a redo re-applies when a rollback left one.
async fn redo_plan(daemon: &DaemonProxy, repo: &str) -> Result<RedoPlan, String> {
    for limit in [undo::WINDOW, undo::WIDE_WINDOW] {
        let log = read_log(daemon, repo, "active", limit).await?;
        let plan = undo::plan_redo_from_log(&log);
        if plan != RedoPlan::Nothing || !undo::window_exhausted(&log, limit) {
            return Ok(plan);
        }
    }
    Ok(RedoPlan::Nothing)
}

async fn read_log(
    daemon: &DaemonProxy,
    repo: &str,
    mode: &str,
    limit: usize,
) -> Result<Value, String> {
    let path = format!("/repos/{repo}/log?mode={mode}&limit={limit}");
    Ok(daemon.request("GET", &path, None).await?.body)
}

pub async fn navigate(
    gui: Arc<GuiState>,
    daemon: Arc<DaemonProxy>,
    ws_id: String,
    redo: bool,
    timeouts: crate::commands::StatusTimeouts,
) -> Result<(), String> {
    let repo = match gui.get_var(&ws_id, "active_repo")? {
        Value::String(repo) => repo,
        _ => return Err("no active repository in this workspace".into()),
    };

    if !redo {
        return undo(gui, daemon, ws_id, repo, timeouts).await;
    }

    match redo_plan(&daemon, &repo).await? {
        RedoPlan::Nothing => {
            gui.post_status(&ws_id, "Nothing to redo.", "info", Some(timeouts.message_ms))?;
            Ok(())
        }
        RedoPlan::Forward { op_id, rev_id } => {
            let response =
                rollback(&gui, &daemon, &ws_id, &repo, json!({ "id": op_id }), &timeouts).await?;
            let applied = response["operations_applied"].as_u64().unwrap_or(0);
            finish(
                &gui,
                &ws_id,
                &format!("Redo: revision {rev_id} re-applied ({applied} operations)."),
                &timeouts,
            )
        }
        RedoPlan::Rollback { rev_id } => {
            let response =
                rollback(&gui, &daemon, &ws_id, &repo, json!({"prev_revision": true}), &timeouts)
                    .await?;
            let count = response["operations_unapplied"].as_u64().unwrap_or(0);
            finish(
                &gui,
                &ws_id,
                &format!("Redo: the undo in revision {rev_id} rolled back ({count} operations)."),
                &timeouts,
            )
        }
        // The watcher has written since the undo, so HEAD cannot be rewound
        // over it: the undo is undone in place, exactly as undo itself does.
        RedoPlan::Revert { rev_id, ops } => {
            revert(&gui, &daemon, &ws_id, &repo, rev_id, ops, &timeouts).await
        }
    }
}

/// Undo: whichever of the two mechanisms the selection picked.
async fn undo(
    gui: Arc<GuiState>,
    daemon: Arc<DaemonProxy>,
    ws_id: String,
    repo: String,
    timeouts: crate::commands::StatusTimeouts,
) -> Result<(), String> {
    match undo_plan(&daemon, &repo).await? {
        UndoPlan::Nothing => {
            gui.post_status(&ws_id, "Nothing to undo.", "info", Some(timeouts.message_ms))?;
            Ok(())
        }
        UndoPlan::Rollback { rev_id } => {
            let response =
                rollback(&gui, &daemon, &ws_id, &repo, json!({"prev_revision": true}), &timeouts)
                    .await?;
            let count = response["operations_unapplied"].as_u64().unwrap_or(0);
            finish(
                &gui,
                &ws_id,
                &format!("Undo: revision {rev_id} rolled back ({count} operations)."),
                &timeouts,
            )
        }
        UndoPlan::Revert { rev_id, ops } => {
            revert(&gui, &daemon, &ws_id, &repo, rev_id, ops, &timeouts).await
        }
    }
}

/// The revert half of an undo: check the plan, then write the inverse at HEAD.
/// A blocked or filesystem-coordinated revert is *reported*, not forced — the
/// log panel (`log:revert with-dependents`) and `mf log undo` are the two ways
/// through, and neither is something to do behind the user's back.
async fn revert(
    gui: &Arc<GuiState>,
    daemon: &Arc<DaemonProxy>,
    ws_id: &str,
    repo: &str,
    rev_id: i64,
    ops: Option<Vec<i64>>,
    timeouts: &crate::commands::StatusTimeouts,
) -> Result<(), String> {
    let target = match &ops {
        Some(ops) => json!({ "op_ids": ops }),
        None => json!({ "rev_id": rev_id }),
    };
    let query = match &ops {
        Some(ops) => format!(
            "target_op_ids={}",
            ops.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
        ),
        None => format!("target_rev_id={rev_id}"),
    };
    let plan = daemon.request("GET", &format!("/repos/{repo}/revert/plan?{query}"), None).await?;
    if plan.status != 200 {
        return report(gui, ws_id, &error_of(&plan.body, "the revert plan failed"), timeouts);
    }
    if plan.body["revertable"] == json!(false) {
        let blocker = plan.body["blocked"].as_array().and_then(|b| b.first()).cloned();
        let where_ = blocker
            .as_ref()
            .and_then(|b| b["rev_id"].as_i64())
            .map(|r| format!(" by revision {r}"))
            .unwrap_or_default();
        return report(
            gui,
            ws_id,
            &format!(
                "Cannot take revision {rev_id} back: a later change{where_} overwrote what it wrote. \
                 Open the log panel and use log:revert with-dependents to undo both."
            ),
            timeouts,
        );
    }
    if plan.body["requires_lock"] == json!(true) {
        return report(
            gui,
            ws_id,
            &format!(
                "Taking revision {rev_id} back moves files on disk; run `mf log undo` (or \
                 `mf log redo`) so the moves are coordinated with the metadata."
            ),
            timeouts,
        );
    }

    let response = daemon
        .request("POST", &format!("/repos/{repo}/revert"), Some(json!({ "target": target })))
        .await?;
    if response.status != 200 {
        return report(gui, ws_id, &error_of(&response.body, "the revert failed"), timeouts);
    }
    let count = response.body["reverted_operations"].as_array().map(|a| a.len()).unwrap_or(0);
    let summary = match response.body["revision"].as_i64() {
        None => "Nothing was reverted.".to_string(),
        Some(new_rev) => format!(
            "Revision {rev_id} reverted as revision {new_rev} ({count} operations) — \
             the daemon's own revisions were left in place."
        ),
    };
    finish(gui, ws_id, &summary, timeouts)
}

/// Drives `POST /rollback`, turning a non-200 into the error it carries.
async fn rollback(
    gui: &Arc<GuiState>,
    daemon: &Arc<DaemonProxy>,
    ws_id: &str,
    repo: &str,
    target: Value,
    timeouts: &crate::commands::StatusTimeouts,
) -> Result<Value, String> {
    let response = daemon
        .request("POST", &format!("/repos/{repo}/rollback"), Some(json!({ "target": target })))
        .await?;
    if response.status != 200 {
        let message = error_of(&response.body, &format!("rollback failed ({})", response.status));
        gui.post_status(ws_id, &message, "error", Some(timeouts.error_ms))?;
        return Err(message);
    }
    Ok(response.body)
}

fn error_of(body: &Value, fallback: &str) -> String {
    body["error"].as_str().map(str::to_string).unwrap_or_else(|| fallback.to_string())
}

/// Says why nothing happened, without failing the command: the user is being
/// told what to do next, not handed an error.
fn report(
    gui: &Arc<GuiState>,
    ws_id: &str,
    message: &str,
    timeouts: &crate::commands::StatusTimeouts,
) -> Result<(), String> {
    gui.post_status(ws_id, message, "error", Some(timeouts.error_ms))
}

/// Reports what was done and asks the panels to refresh.
fn finish(
    gui: &Arc<GuiState>,
    ws_id: &str,
    summary: &str,
    timeouts: &crate::commands::StatusTimeouts,
) -> Result<(), String> {
    gui.post_status(ws_id, summary, "info", Some(timeouts.message_ms))?;
    // Refresh metarecord-list / metarecord-detail / log panels.
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
pub async fn log_navigate(
    app: tauri::State<'_, Arc<crate::commands::App>>,
    ws_id: String,
    redo: bool,
) -> Result<(), String> {
    navigate(app.gui.clone(), app.daemon.clone(), ws_id, redo, app.status_timeouts()).await
}
