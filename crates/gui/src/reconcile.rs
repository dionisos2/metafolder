//! `reconcile:run` (spec-gui "Reconcile"): triggers a full reconcile on
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

pub async fn run(gui: Arc<GuiState>, daemon: Arc<DaemonProxy>, ws_id: String) -> Result<(), String> {
    let repo = match gui.get_var(&ws_id, "active_repo")? {
        Value::String(repo) => repo,
        _ => return Err("no active repository in this workspace".into()),
    };

    gui.post_status(&ws_id, "Reconciling…", "busy", None)?;
    let response = daemon
        .request("POST", &format!("/repos/{repo}/reconcile"), None)
        .await;

    match response {
        Ok(response) if response.status == 200 => {
            gui.post_status(&ws_id, &format_summary(&response.body), "info", Some(8000))?;
            let detail = serde_json::to_string_pretty(&response.body)
                .unwrap_or_else(|_| response.body.to_string());
            gui.append_message(&ws_id, &detail)?;
            Ok(())
        }
        Ok(response) => {
            let message = response.body["error"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| format!("reconcile failed ({})", response.status));
            gui.post_status(&ws_id, &message, "error", Some(8000))?;
            Err(message)
        }
        Err(error) => {
            gui.post_status(&ws_id, &error, "error", Some(8000))?;
            Err(error)
        }
    }
}

#[tauri::command]
pub async fn reconcile_run(
    app: tauri::State<'_, Arc<crate::commands::App>>,
    ws_id: String,
) -> Result<(), String> {
    run(app.gui.clone(), app.daemon.clone(), ws_id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_format_summary() {
        let result = json!({"created": 3, "moved": 1, "candidates": [{"a": 1}, {"b": 2}]});
        assert_eq!(format_summary(&result), "Reconcile: 3 created, 1 moved, 2 candidates.");
        assert_eq!(
            format_summary(&json!({})),
            "Reconcile: 0 created, 0 moved, 0 candidates."
        );
    }
}
