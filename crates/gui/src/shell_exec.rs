//! `!` shell commands from the command input (spec-gui "Command input"):
//! run as a subprocess; stdout/stderr lines go to the workspace message
//! log (message panel type) and to the terminal that launched the GUI.

use crate::state::GuiState;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

/// Runs the command line and streams its output; returns when the
/// process exits. The Tauri command spawns this in the background.
pub async fn run_to_completion(
    gui: Arc<GuiState>,
    ws_id: String,
    command_line: String,
) -> Result<(), String> {
    // Fail fast on unknown workspaces (and log the invocation).
    gui.append_message(&ws_id, &format!("$ {command_line}"))?;

    // A per-run id the subprocess can address its own progress with
    // (`mf gui progress` reads it from METAFOLDER_GUI_TASK); session-unique.
    static RUN_SEQ: AtomicU64 = AtomicU64::new(1);
    let task_id = format!("script-{}", RUN_SEQ.fetch_add(1, Ordering::Relaxed));

    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&command_line)
        .env("METAFOLDER_GUI_TASK", &task_id)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run shell command: {e}"))?;

    // Show a running indicator until this function returns (spec-gui
    // "Scripting"). The guard clears it on every exit path — early `?`
    // returns and panics included — so the spinner can never get stuck on.
    gui.script_begin(&task_id, &ws_id, &script_label(&command_line));
    let _running = RunningGuard { gui: gui.clone(), task_id: task_id.clone() };

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    let out_task = tokio::spawn(forward(gui.clone(), ws_id.clone(), stdout, false));
    let err_task = tokio::spawn(forward(gui.clone(), ws_id.clone(), stderr, true));

    let status = child.wait().await.map_err(|e| format!("shell command failed: {e}"))?;
    let _ = out_task.await;
    let _ = err_task.await;

    if !status.success() {
        let code = status.code().map_or("?".to_string(), |c| c.to_string());
        gui.append_message(&ws_id, &format!("[exit {code}]"))?;
        // The message log alone is not enough: a GUI script writes it into a
        // scratch workspace its own teardown removes, so a run killed by
        // `set -e` would vanish without a trace. Say so on the launching
        // workspace's status bar too (spec-gui "Script session").
        let _ = gui.post_status(
            &ws_id,
            &format!("{} failed (exit {code})", script_label(&command_line)),
            "error",
            None,
        );
    }
    Ok(())
}

/// Clears the running indicator for a workspace when dropped, so it is removed
/// on every exit path of `run_to_completion` (including an early `?` return).
struct RunningGuard {
    gui: Arc<GuiState>,
    task_id: String,
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        self.gui.script_end(&self.task_id);
    }
}

/// A short human label for the running indicator: the script's base name for a
/// `bash <path>` launch (the `script:run` builtin), otherwise the command line
/// itself.
pub fn script_label(command_line: &str) -> String {
    let trimmed = command_line.trim();
    if let Some(rest) = trimmed.strip_prefix("bash ") {
        let first = rest.split_whitespace().next().unwrap_or("");
        let path = first.trim_matches('\'').trim_matches('"');
        if let Some(name) = std::path::Path::new(path).file_name().and_then(|n| n.to_str()) {
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }
    trimmed.to_string()
}

/// Streams one output pipe into the message log, echoing to the terminal
/// that launched the GUI.
async fn forward(
    gui: Arc<GuiState>,
    ws_id: String,
    reader: impl tokio::io::AsyncRead + Unpin,
    to_stderr: bool,
) {
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if to_stderr {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
        let _ = gui.append_message(&ws_id, &line);
    }
}

#[tauri::command]
pub fn run_shell(
    app: tauri::State<'_, Arc<crate::commands::App>>,
    ws_id: String,
    command_line: String,
) -> Result<(), String> {
    let gui = app.gui.clone();
    let message_ms = app.status_timeouts().message_ms;
    tauri::async_runtime::spawn(async move {
        if let Err(error) = run_to_completion(gui.clone(), ws_id.clone(), command_line).await {
            let _ = gui.post_status(&ws_id, &error, "error", Some(message_ms));
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notifier::RecordingNotifier;
    use serde_json::json;

    fn gui() -> Arc<GuiState> {
        Arc::new(GuiState::new(Arc::new(RecordingNotifier::new())))
    }

    #[tokio::test]
    async fn test_output_lines_reach_the_message_log() {
        let gui = gui();
        run_to_completion(gui.clone(), "ws-1".into(), "echo hello; echo oops 1>&2".into())
            .await
            .unwrap();

        let log = gui.messages("ws-1").unwrap();
        let texts: Vec<&str> = log.iter().map(|m| m.text.as_str()).collect();
        assert!(texts.iter().any(|t| t.contains("hello")), "stdout missing: {texts:?}");
        assert!(texts.iter().any(|t| t.contains("oops")), "stderr missing: {texts:?}");
    }

    #[tokio::test]
    async fn test_nonzero_exit_is_logged() {
        let gui = gui();
        run_to_completion(gui.clone(), "ws-1".into(), "exit 3".into()).await.unwrap();
        let log = gui.messages("ws-1").unwrap();
        assert!(log.iter().any(|m| m.text.contains("exit") && m.text.contains('3')));
    }

    #[tokio::test]
    async fn test_unknown_workspace_errors() {
        let gui = gui();
        assert!(run_to_completion(gui, "ws-99".into(), "echo hi".into()).await.is_err());
    }

    #[test]
    fn test_script_label() {
        // script:run: `bash <quoted path>` → the script's base name.
        assert_eq!(
            script_label("bash '/home/u/.config/metafolder/scripts/gui-tag-folder.sh'"),
            "gui-tag-folder.sh",
        );
        assert_eq!(script_label("bash /tmp/x/foo.sh"), "foo.sh");
        // A plain `!` shell command keeps its command line.
        assert_eq!(script_label("echo hello"), "echo hello");
    }

    #[test]
    fn test_running_indicator_begins_and_clears() {
        let notifier = Arc::new(RecordingNotifier::new());
        let gui = Arc::new(GuiState::new(notifier.clone()));
        gui.script_begin("script-1", "ws-1", "gui-tag-folder.sh");
        gui.script_end("script-1");

        let payloads = notifier.payloads(crate::events::SCRIPT_TASK_CHANGED);
        assert_eq!(payloads.len(), 2, "one emit for begin, one for end");
        let running = payloads[0]["tasks"].as_array().unwrap();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0]["task"], "script-1");
        assert_eq!(running[0]["label"], "gui-tag-folder.sh");
        assert_eq!(running[0]["workspace_id"], "ws-1");
        assert_eq!(payloads[1]["tasks"].as_array().unwrap().len(), 0, "cleared on end");
    }

    #[test]
    fn test_script_progress_updates_done_total_phase() {
        let notifier = Arc::new(RecordingNotifier::new());
        let gui = Arc::new(GuiState::new(notifier.clone()));
        gui.script_begin("script-1", "ws-1", "gui-tag-pair.sh");
        gui.script_progress("script-1", Some(3), Some(10), Some("/music/x.mp3".into()));
        // A later call overwrites only the fields it provides (done here).
        gui.script_progress("script-1", Some(4), None, None);
        // An unknown run id is ignored (no panic, no new emit).
        let before = notifier.payloads(crate::events::SCRIPT_TASK_CHANGED).len();
        gui.script_progress("nope", Some(9), Some(9), None);
        assert_eq!(notifier.payloads(crate::events::SCRIPT_TASK_CHANGED).len(), before);

        let last = notifier.payloads(crate::events::SCRIPT_TASK_CHANGED);
        let task = &last.last().unwrap()["tasks"][0];
        assert_eq!(task["done"], 4);
        assert_eq!(task["total"], 10, "total persists across a done-only update");
        assert_eq!(task["phase"], "/music/x.mp3");
    }

    #[test]
    fn test_script_claims_the_workspaces_it_creates() {
        let notifier = Arc::new(RecordingNotifier::new());
        let gui = Arc::new(GuiState::new(notifier.clone()));
        gui.script_begin("script-1", "ws-launch", "gui-tag-folder.sh");
        // A script that opens two scratch workspaces owns all three.
        gui.script_claim_workspace("script-1", "ws-a");
        gui.script_claim_workspace("script-1", "ws-b");
        gui.script_claim_workspace("script-1", "ws-a"); // idempotent

        let payloads = notifier.payloads(crate::events::SCRIPT_TASK_CHANGED);
        let task = &payloads.last().unwrap()["tasks"][0];
        assert_eq!(task["workspaces"], json!(["ws-launch", "ws-a", "ws-b"]));
        assert_eq!(gui.script_workspaces("script-1"), vec!["ws-launch", "ws-a", "ws-b"]);
        // An unknown run id claims nothing and has no workspaces.
        gui.script_claim_workspace("nope", "ws-c");
        assert!(gui.script_workspaces("nope").is_empty());
    }

    #[test]
    fn test_script_waiting_flag_tracks_the_input_wait() {
        let notifier = Arc::new(RecordingNotifier::new());
        let gui = Arc::new(GuiState::new(notifier.clone()));
        gui.script_begin("script-1", "ws-1", "gui-tag-pair.sh");
        let running = notifier.payloads(crate::events::SCRIPT_TASK_CHANGED);
        assert_eq!(running.last().unwrap()["tasks"][0]["waiting"], json!(false));

        gui.script_waiting("script-1", true);
        let waiting = notifier.payloads(crate::events::SCRIPT_TASK_CHANGED);
        assert_eq!(waiting.last().unwrap()["tasks"][0]["waiting"], json!(true));

        gui.script_waiting("script-1", false);
        let back = notifier.payloads(crate::events::SCRIPT_TASK_CHANGED);
        assert_eq!(back.last().unwrap()["tasks"][0]["waiting"], json!(false));

        // An unknown run id is ignored (no panic, no new broadcast).
        let before = notifier.payloads(crate::events::SCRIPT_TASK_CHANGED).len();
        gui.script_waiting("nope", true);
        assert_eq!(notifier.payloads(crate::events::SCRIPT_TASK_CHANGED).len(), before);
    }

    #[tokio::test]
    async fn test_a_failing_script_says_so_in_the_status_bar() {
        // A script killed by `set -e` leaves nothing on screen: its message log
        // lives in a scratch workspace the session teardown removes. The exit
        // code must therefore also reach the launching workspace's status bar.
        let notifier = Arc::new(RecordingNotifier::new());
        let gui = Arc::new(GuiState::new(notifier.clone()));
        let ws = gui.workspaces()[0].id.clone();
        run_to_completion(gui.clone(), ws.clone(), "echo boom 1>&2; exit 4".into()).await.unwrap();

        let statuses = notifier.payloads(crate::events::STATUS_MESSAGE);
        let failed = statuses
            .iter()
            .find(|p| p["kind"] == "error")
            .expect("a failing script posts an error status");
        assert_eq!(failed["workspace_id"], json!(ws));
        assert!(
            failed["text"].as_str().unwrap().contains('4'),
            "the exit code is named: {failed:?}"
        );
    }

    #[tokio::test]
    async fn test_run_to_completion_clears_the_indicator() {
        let notifier = Arc::new(RecordingNotifier::new());
        let gui = Arc::new(GuiState::new(notifier.clone()));
        run_to_completion(gui.clone(), "ws-1".into(), "true".into()).await.unwrap();
        // The last running-set broadcast is empty: nothing left running.
        let payloads = notifier.payloads(crate::events::SCRIPT_TASK_CHANGED);
        assert!(!payloads.is_empty());
        assert_eq!(payloads.last().unwrap()["tasks"].as_array().unwrap().len(), 0);
    }
}
