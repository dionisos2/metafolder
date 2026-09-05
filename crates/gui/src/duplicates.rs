//! `mf:duplicate-scan` (spec-duplicates "GUI"): scans the workspace's active
//! repo for byte-identical files, with a busy status while it runs, a summary
//! in the status bar and the full result in the message log.
//!
//! The GUI half of `mf duplicate scan`, hence the `mf:` prefix (spec-gui
//! "Naming"). Like `reconcile:run` it only polls the task to know when it ends
//! — live progress belongs to the task bar, which polls `GET /tasks` itself.

use crate::daemon_proxy::DaemonProxy;
use crate::reconcile::Timings;
use crate::state::GuiState;
use serde_json::Value;
use std::sync::Arc;

/// `"Duplicates: 12 groups, 31 files, 4.5G reclaimable."`
pub fn format_summary(result: &Value) -> String {
    let count = |key: &str| result[key].as_u64().unwrap_or(0);
    format!(
        "Duplicates: {} groups, {} files, {} reclaimable.",
        count("groups"),
        count("files"),
        metafolder_core::progress::human_size(count("reclaimable")),
    )
}

pub async fn run(
    gui: Arc<GuiState>,
    daemon: Arc<DaemonProxy>,
    ws_id: String,
    min_size: Option<u64>,
    rehash: bool,
    timings: Timings,
) -> Result<(), String> {
    let repo = match gui.get_var(&ws_id, "active_repo")? {
        Value::String(repo) => repo,
        _ => return Err("no active repository in this workspace".into()),
    };

    gui.post_status(&ws_id, "Scanning for duplicates…", "busy", None)?;

    let mut body = serde_json::json!({"rehash": rehash});
    if let Some(min_size) = min_size {
        body["min_size"] = serde_json::json!(min_size);
    }
    let started = daemon
        .request("POST", &format!("/repos/{repo}/duplicates/scan"), Some(body))
        .await
        .inspect_err(|error| {
            let _ = gui.post_status(&ws_id, error, "error", Some(timings.error_ms));
        })?;
    if started.status == 404 {
        // API_VERSION is deliberately not bumped for an additive endpoint
        // (spec-duplicates "Wire compatibility"), so the clear message is ours
        // to produce rather than the version banner's.
        let message = "this daemon does not support duplicate detection".to_string();
        gui.post_status(&ws_id, &message, "error", Some(timings.error_ms))?;
        return Err(message);
    }
    if started.status != 202 {
        let message = started.body["error"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| format!("duplicate scan failed ({})", started.status));
        gui.post_status(&ws_id, &message, "error", Some(timings.error_ms))?;
        return Err(message);
    }
    let task_id = started.body["task_id"]
        .as_str()
        .ok_or_else(|| "duplicate scan: missing task_id in response".to_string())?
        .to_string();

    loop {
        let response = daemon
            .request("GET", &format!("/repos/{repo}/tasks/{task_id}"), None)
            .await
            .inspect_err(|error| {
                let _ = gui.post_status(&ws_id, error, "error", Some(timings.error_ms));
            })?;
        let task = &response.body;
        match task["status"].as_str() {
            Some("done") => {
                let result = &task["result"];
                gui.post_status(&ws_id, &format_summary(result), "info", Some(timings.done_ms))?;
                let detail =
                    serde_json::to_string_pretty(result).unwrap_or_else(|_| result.to_string());
                gui.append_message(&ws_id, &detail)?;
                return Ok(());
            }
            Some("failed") => {
                let message = task["error"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| "duplicate scan failed".to_string());
                gui.post_status(&ws_id, &message, "error", Some(timings.error_ms))?;
                return Err(message);
            }
            Some("cancelled") => {
                // Cancellation is not a failure: the scan keeps the hashes it
                // computed and the groups it wrote (spec-duplicates "Writes and
                // revisions"), so say so rather than shout.
                gui.post_status(
                    &ws_id,
                    "Duplicate scan cancelled; what it found is kept.",
                    "info",
                    Some(timings.done_ms),
                )?;
                return Ok(());
            }
            _ => tokio::time::sleep(timings.poll).await,
        }
    }
}

#[tauri::command]
pub async fn duplicate_scan(
    app: tauri::State<'_, Arc<crate::commands::App>>,
    ws_id: String,
    min_size: Option<u64>,
    rehash: Option<bool>,
) -> Result<(), String> {
    let timings = Timings {
        poll: app.settings.reconcile_poll(),
        error_ms: app.panel_settings.status_error_ms as u64,
        done_ms: app.panel_settings.status_error_ms as u64,
    };
    run(app.gui.clone(), app.daemon.clone(), ws_id, min_size, rehash.unwrap_or(false), timings)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_format_summary() {
        let result = json!({"groups": 12, "files": 31, "reclaimable": 4_831_838_208u64});
        assert_eq!(format_summary(&result), "Duplicates: 12 groups, 31 files, 4.5G reclaimable.");
        assert_eq!(format_summary(&json!({})), "Duplicates: 0 groups, 0 files, 0B reclaimable.");
    }
}
