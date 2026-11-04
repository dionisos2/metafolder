//! `reconcile:run` (doc "Running a reconcile"): triggers a full reconcile on
//! the workspace's active repo, with a busy status while it runs, a
//! summary in the status bar and the full result in the message log.

use crate::daemon_proxy::DaemonProxy;
use crate::state::GuiState;
use serde_json::Value;
use std::sync::Arc;

/// `"Reconcile: N created, M moved, P candidates."`
pub fn format_summary(result: &Value) -> String {
    let count = |key: &str| match &result[key] {
        Value::Number(n) => n.as_u64().unwrap_or(0),
        Value::Array(items) => items.len() as u64,
        _ => 0,
    };
    format!(
        "Reconcile: {} created, {} moved, {} candidates.",
        count("created"),
        count("moved"),
        count("candidates"),
    )
}

/// Timings for a reconcile run (config.toml `[settings]`/`[panels]`): the
/// interval between task polls and how long error status messages stay.
#[derive(Clone, Copy)]
pub struct Timings {
    pub poll: std::time::Duration,
    pub error_ms: u64,
    pub done_ms: u64,
}

impl Default for Timings {
    fn default() -> Self {
        Timings { poll: std::time::Duration::from_millis(200), error_ms: 8000, done_ms: 8000 }
    }
}

pub async fn run(
    gui: Arc<GuiState>,
    daemon: Arc<DaemonProxy>,
    ws_id: String,
    timings: Timings,
) -> Result<(), String> {
    let repo = gui.active_repo(&ws_id)?;

    gui.post_status(&ws_id, "Reconciling…", "busy", None)?;

    // Reconcile is asynchronous (doc "Tasks"): start it (202 + task id), then
    // poll the task, surfacing progress in the status bar.
    let started = daemon
        .request("POST", &format!("/repos/{repo}/reconcile"), None)
        .await
        .inspect_err(|error| {
            let _ = gui.post_status(&ws_id, error, "error", Some(timings.error_ms));
        })?;
    if started.status != 202 {
        let message = crate::daemon_proxy::error_message(&started.body, || {
            format!("reconcile failed ({})", started.status)
        });
        gui.post_status(&ws_id, &message, "error", Some(timings.error_ms))?;
        return Err(message);
    }
    let task_id = started.body["task_id"]
        .as_str()
        .ok_or_else(|| "reconcile: missing task_id in response".to_string())?
        .to_string();

    match wait_task(&gui, &daemon, &ws_id, &repo, &task_id, &timings, "reconcile").await? {
        TaskEnd::Done(result) => {
            gui.post_status(&ws_id, &format_summary(&result), "info", Some(timings.done_ms))?;
            let detail =
                serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string());
            gui.append_message(&ws_id, &detail)?;
        }
        TaskEnd::Cancelled => {
            // What it reconciled before the stop is kept: not a failure.
            gui.post_status(&ws_id, "Reconcile cancelled.", "info", Some(timings.done_ms))?;
        }
    }
    Ok(())
}

/// How a task the GUI waited for ended, when it did not fail.
pub(crate) enum TaskEnd {
    /// `done`, with the task's `result`.
    Done(Value),
    /// `cancelled`, at the user's request (doc "Cancelling a task").
    Cancelled,
}

/// Polls task `task_id` of `repo` until it is over. Live progress belongs to
/// the task bar, which polls `GET /tasks` itself, so nothing is posted per
/// poll. A failed task — or one the daemon no longer knows (evicted, or a
/// daemon started again meanwhile: polled on, it would answer 404 for ever) —
/// is the error, posted in the status bar.
pub(crate) async fn wait_task(
    gui: &GuiState,
    daemon: &DaemonProxy,
    ws_id: &str,
    repo: &str,
    task_id: &str,
    timings: &Timings,
    what: &str,
) -> Result<TaskEnd, String> {
    let fail = |message: String| {
        let _ = gui.post_status(ws_id, &message, "error", Some(timings.error_ms));
        Err(message)
    };
    loop {
        let response =
            match daemon.request("GET", &format!("/repos/{repo}/tasks/{task_id}"), None).await {
                Ok(response) => response,
                Err(error) => return fail(error),
            };
        let task = &response.body;
        if response.status != 200 {
            let status = response.status;
            return fail(crate::daemon_proxy::error_message(task, || {
                format!("{what}: reading its task failed ({status})")
            }));
        }
        match task["status"].as_str() {
            Some("done") => return Ok(TaskEnd::Done(task["result"].clone())),
            Some("cancelled") => return Ok(TaskEnd::Cancelled),
            Some("failed") => {
                return fail(crate::daemon_proxy::error_message(task, || format!("{what} failed")))
            }
            _ => tokio::time::sleep(timings.poll).await,
        }
    }
}

#[tauri::command]
pub async fn reconcile_run(
    app: tauri::State<'_, Arc<crate::commands::App>>,
    ws_id: String,
) -> Result<(), String> {
    let timings = Timings {
        poll: app.settings.reconcile_poll(),
        error_ms: app.panel_settings.status_error_ms as u64,
        done_ms: app.panel_settings.status_error_ms as u64,
    };
    run(app.gui.clone(), app.daemon.clone(), ws_id, timings).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_format_summary() {
        let result = json!({"created": 3, "moved": 1, "candidates": [{"a": 1}, {"b": 2}]});
        assert_eq!(format_summary(&result), "Reconcile: 3 created, 1 moved, 2 candidates.");
        assert_eq!(format_summary(&json!({})), "Reconcile: 0 created, 0 moved, 0 candidates.");
    }
}
