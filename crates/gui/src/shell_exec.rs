//! `!` shell commands from the command input (spec-gui "Command input"):
//! run as a subprocess; stdout/stderr lines go to the workspace message
//! log (message panel type) and to the terminal that launched the GUI.

use crate::state::GuiState;
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

    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&command_line)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run shell command: {e}"))?;

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    let out_task = tokio::spawn(forward(gui.clone(), ws_id.clone(), stdout, false));
    let err_task = tokio::spawn(forward(gui.clone(), ws_id.clone(), stderr, true));

    let status = child
        .wait()
        .await
        .map_err(|e| format!("shell command failed: {e}"))?;
    let _ = out_task.await;
    let _ = err_task.await;

    if !status.success() {
        let code = status.code().map_or("?".to_string(), |c| c.to_string());
        gui.append_message(&ws_id, &format!("[exit {code}]"))?;
    }
    Ok(())
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
    tauri::async_runtime::spawn(async move {
        if let Err(error) = run_to_completion(gui.clone(), ws_id.clone(), command_line).await {
            let _ = gui.post_status(&ws_id, &error, "error", Some(5000));
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notifier::RecordingNotifier;

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
        run_to_completion(gui.clone(), "ws-1".into(), "exit 3".into())
            .await
            .unwrap();
        let log = gui.messages("ws-1").unwrap();
        assert!(log.iter().any(|m| m.text.contains("exit") && m.text.contains('3')));
    }

    #[tokio::test]
    async fn test_unknown_workspace_errors() {
        let gui = gui();
        assert!(run_to_completion(gui, "ws-99".into(), "echo hi".into())
            .await
            .is_err());
    }
}
