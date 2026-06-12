//! `log:undo` / `log:redo` (spec-gui "Event log"): navigate the active
//! repo's event log one revision at a time. Undo maps to the daemon's
//! `{"prev_revision": true}` rollback target; redo re-applies the
//! revision ahead of HEAD in the operation tree (the most recent branch
//! when several exist), which the daemon has no direct target for.

use crate::daemon_proxy::DaemonProxy;
use crate::state::GuiState;
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

pub async fn navigate(
    gui: Arc<GuiState>,
    daemon: Arc<DaemonProxy>,
    ws_id: String,
    redo: bool,
) -> Result<(), String> {
    let repo = match gui.get_var(&ws_id, "active_repo")? {
        Value::String(repo) => repo,
        _ => return Err("no active repository in this workspace".into()),
    };

    let target = if redo {
        // Tree mode keeps the revisions ahead of HEAD listed.
        let log = daemon
            .request("GET", &format!("/repos/{repo}/log?mode=tree"), None)
            .await?;
        let operations = log.body["operations"].as_array().cloned().unwrap_or_default();
        match redo_target(&operations, log.body["head"].as_i64()) {
            Some(id) => json!({ "id": id }),
            None => {
                gui.post_status(&ws_id, "Nothing to redo.", "info", Some(5000))?;
                return Ok(());
            }
        }
    } else {
        json!({ "prev_revision": true })
    };

    let response = daemon
        .request(
            "POST",
            &format!("/repos/{repo}/rollback"),
            Some(json!({ "target": target })),
        )
        .await?;
    if response.status != 200 {
        let message = response.body["error"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| format!("rollback failed ({})", response.status));
        gui.post_status(&ws_id, &message, "error", Some(8000))?;
        return Err(message);
    }

    let count = |key: &str| response.body[key].as_u64().unwrap_or(0);
    let summary = if redo {
        format!("Redo: {} operations re-applied.", count("operations_applied"))
    } else {
        format!("Undo: {} operations unapplied.", count("operations_unapplied"))
    };
    gui.post_status(&ws_id, &summary, "info", Some(5000))?;
    // Refresh metarecord-list / metarecord-detail / log panels.
    gui.set_var(&ws_id, "metarecords:dirty", json!(now_ms()))?;
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
    navigate(app.gui.clone(), app.daemon.clone(), ws_id, redo).await
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
        let ops = [
            op(1, None, 1),
            op(2, Some(1), 2),
            op(3, Some(1), 3),
            op(4, Some(3), 3),
        ];
        assert_eq!(redo_target(&ops, Some(1)), Some(4));
    }
}
