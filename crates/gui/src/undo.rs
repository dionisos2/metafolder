//! `log:undo` / `log:redo` (spec-gui "Event log"): undo the last change the
//! *user* made to the active repository, and re-apply what a rollback undid.
//!
//! Undo is not "step HEAD back one revision": the watcher writes revisions of
//! its own, and rewinding HEAD past them would unrecord what the filesystem
//! really did. The choice — roll HEAD back, or write the inverse at HEAD — is
//! [`metafolder_core::undo`]'s, shared with `mf log undo`; this module only
//! carries it out and reports it.
//!
//! Redo re-applies the revision ahead of HEAD in the operation tree (the most
//! recent branch when several exist), which the daemon has no direct target
//! for. It is the exact counterpart of an undo that rolled back; an undo that
//! had to revert leaves HEAD at a tip, and is itself undone by reverting the
//! revert (`log:revert` on it).

use crate::daemon_proxy::DaemonProxy;
use crate::state::GuiState;
use metafolder_core::undo::{self, UndoPlan};
use serde_json::{json, Value};
use std::sync::Arc;

/// The operation id to navigate to for a redo: the last operation of the
/// revision of HEAD's most recent child. `None` when HEAD is at a tip
/// (nothing to redo). A `head` of `None` redoes from the empty state.
pub fn redo_target(operations: &[Value], head: Option<i64>) -> Option<i64> {
    let child = operations
        .iter()
        .filter(|op| op["parent_id"].as_i64() == head)
        .max_by_key(|op| op["id"].as_i64())?;
    operations
        .iter()
        .filter(|op| op["rev_id"] == child["rev_id"])
        .filter_map(|op| op["id"].as_i64())
        .max()
}

/// Reads enough of the log to decide what undo should undo — two bounded reads
/// rather than one unbounded one (spec-event-log "mf log undo").
async fn undo_plan(daemon: &DaemonProxy, repo: &str) -> Result<UndoPlan, String> {
    for limit in [undo::WINDOW, undo::WIDE_WINDOW] {
        let log = daemon
            .request("GET", &format!("/repos/{repo}/log?mode=linear&limit={limit}"), None)
            .await?;
        let plan = undo::plan_from_log(&log.body);
        if plan != UndoPlan::Nothing || !undo::window_exhausted(&log.body, limit) {
            return Ok(plan);
        }
    }
    Ok(UndoPlan::Nothing)
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

    // Tree mode keeps the revisions ahead of HEAD listed.
    let log = daemon.request("GET", &format!("/repos/{repo}/log?mode=tree"), None).await?;
    let operations = log.body["operations"].as_array().cloned().unwrap_or_default();
    let target = match redo_target(&operations, log.body["head"].as_i64()) {
        Some(id) => json!({ "id": id }),
        None => {
            gui.post_status(&ws_id, "Nothing to redo.", "info", Some(timeouts.message_ms))?;
            return Ok(());
        }
    };
    let response = rollback(&gui, &daemon, &ws_id, &repo, target, &timeouts).await?;
    let applied = response["operations_applied"].as_u64().unwrap_or(0);
    finish(&gui, &ws_id, &format!("Redo: {applied} operations re-applied."), &timeouts)
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
/// log panel (`log:revert-with-dependents`) and `mf log undo` are the two ways
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
                "Cannot undo revision {rev_id}: a later change{where_} overwrote what it wrote. \
                 Open the log panel and use log:revert-with-dependents to undo both."
            ),
            timeouts,
        );
    }
    if plan.body["requires_lock"] == json!(true) {
        return report(
            gui,
            ws_id,
            &format!(
                "Undoing revision {rev_id} moves files on disk; run `mf log undo` so the moves \
                 are coordinated with the metadata."
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
        None => "Undo: nothing was reverted.".to_string(),
        Some(new_rev) => format!(
            "Undo: revision {rev_id} reverted as revision {new_rev} ({count} operations) — \
             the watcher's revisions were left in place."
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

#[cfg(test)]
mod tests {
    use super::*;

    fn op(id: i64, parent_id: Option<i64>, rev_id: i64) -> Value {
        json!({"id": id, "parent_id": parent_id, "rev_id": rev_id})
    }

    #[test]
    fn test_redo_target_walks_to_the_end_of_the_child_revision() {
        let ops = [op(1, None, 1), op(2, Some(1), 1), op(3, Some(2), 2), op(4, Some(3), 2)];
        assert_eq!(redo_target(&ops, Some(2)), Some(4));
        // Mid-revision HEAD: the rest of the same revision is re-applied.
        assert_eq!(redo_target(&ops, Some(3)), Some(4));
        // From the empty state the first revision is re-applied.
        assert_eq!(redo_target(&ops, None), Some(2));
    }

    #[test]
    fn test_redo_target_at_a_tip_is_none() {
        let ops = [op(1, None, 1), op(2, Some(1), 1)];
        assert_eq!(redo_target(&ops, Some(2)), None);
        assert_eq!(redo_target(&[], None), None);
    }

    #[test]
    fn test_redo_target_prefers_the_most_recent_branch() {
        let ops = [op(1, None, 1), op(2, Some(1), 2), op(3, Some(1), 3), op(4, Some(3), 3)];
        assert_eq!(redo_target(&ops, Some(1)), Some(4));
    }
}
