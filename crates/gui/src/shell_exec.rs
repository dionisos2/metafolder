//! `!` shell commands from the command input (doc "Bash mode"):
//! run as a subprocess; stdout/stderr lines go to the workspace shell log
//! (shell panel type) and to the terminal that launched the GUI. The message
//! log only records that the line ran, as `$ command`.

use crate::state::workspace::ShellLine;
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
    // A per-run id the subprocess can address its own progress with
    // (`mf gui progress` reads it from METAFOLDER_GUI_TASK); session-unique.
    // It also groups the run's lines in the shell log.
    static RUN_SEQ: AtomicU64 = AtomicU64::new(1);
    let task_id = format!("script-{}", RUN_SEQ.fetch_add(1, Ordering::Relaxed));

    // Fail fast on unknown workspaces (and log the invocation).
    gui.append_message(&ws_id, &format!("$ {command_line}"))?;
    gui.append_shell(&ws_id, &task_id, ShellLine::Command, &command_line)?;

    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(&command_line)
        .env("METAFOLDER_GUI_TASK", &task_id)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // Its own process group, so `script:stop` can signal the script *and* every
    // child it spawned (the `mf gui input` blocked on a question above all) with
    // one killpg — signalling the shell alone would leave those behind.
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().map_err(|e| format!("cannot run shell command: {e}"))?;

    // Show a running indicator until this function returns (doc
    // "Script sessions"). The guard clears it on every exit path — early `?`
    // returns and panics included — so the spinner can never get stuck on.
    gui.script_begin(&task_id, &ws_id, &script_label(&command_line));
    let _running = RunningGuard { gui: gui.clone(), task_id: task_id.clone() };
    gui.script_set_pid(&task_id, child.id());

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    let out_task =
        tokio::spawn(forward(gui.clone(), ws_id.clone(), task_id.clone(), stdout, false));
    let err_task = tokio::spawn(forward(gui.clone(), ws_id.clone(), task_id.clone(), stderr, true));

    let status = child.wait().await.map_err(|e| format!("shell command failed: {e}"))?;
    // Reaped: the pid is now free to be recycled, so it must stop naming this
    // run (a pending `script:stop` follow-up would otherwise signal a stranger).
    gui.script_set_pid(&task_id, None);
    let _ = out_task.await;
    let _ = err_task.await;

    if let Some(how) = gui.script_stopped(&task_id) {
        // Asked for by the user: neither a failure nor an exit code worth
        // naming (a signal has none), and nothing in red.
        gui.append_shell(&ws_id, &task_id, ShellLine::Status, how)?;
        let _ = gui.post_status(
            &ws_id,
            &format!("{} {how}", script_label(&command_line)),
            "info",
            None,
        );
    } else if !status.success() {
        let code = status.code().map_or("?".to_string(), |c| c.to_string());
        gui.append_shell(&ws_id, &task_id, ShellLine::Status, &format!("exit {code}"))?;
        // The shell log alone is not enough: a GUI script writes it into a
        // scratch workspace its own teardown removes, so a run killed by
        // `set -e` would vanish without a trace. Say so on the launching
        // workspace's status bar too (doc "Script sessions").
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

/// Streams one output pipe into the shell log, echoing to the terminal
/// that launched the GUI.
async fn forward(
    gui: Arc<GuiState>,
    ws_id: String,
    run: String,
    reader: impl tokio::io::AsyncRead + Unpin,
    to_stderr: bool,
) {
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let kind = if to_stderr {
            eprintln!("{line}");
            ShellLine::Stderr
        } else {
            println!("{line}");
            ShellLine::Stdout
        };
        let _ = gui.append_shell(&ws_id, &run, kind, &line);
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

/// The two ways the user ends a run (doc "Script sessions").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// `script:stop` — `SIGTERM`: the script is asked to end, and may trap the
    /// signal to finish what it is doing.
    Terminate,
    /// `script:kill` — `SIGKILL`: the script ends now and runs nothing more.
    Kill,
}

/// `script:stop` / `script:kill` — ends run `task_id` by signalling its whole
/// process group. Returns false when no such run is known (already finished,
/// or never GUI-launched), so the caller can say so rather than pretend.
///
/// One signal, and no escalation: a stop the script ignores is not turned into
/// a kill behind the user's back — killing is their own, second command.
///
/// The script's pending question needs no separate resolution: when the group
/// dies its `mf gui input` goes with it, the HTTP connection drops and the
/// wait's guard releases the lock.
pub fn stop_script(gui: &Arc<GuiState>, task_id: &str, how: Stop) -> bool {
    let Some(pid) = gui.script_pid(task_id) else { return false };
    let (signal, word) = match how {
        Stop::Terminate => (SIGTERM, "stopped"),
        Stop::Kill => (SIGKILL, "killed"),
    };
    if !signal_group(pid, signal) {
        return false;
    }
    // Ended by the user: reported under that word, and — a script that is
    // being ended cleans nothing up — cleaned up after by the GUI, once the
    // run has ended (`GuiState::script_end`).
    gui.script_mark_stopped(task_id, word);
    true
}

#[cfg(unix)]
const SIGTERM: i32 = libc::SIGTERM;
#[cfg(unix)]
const SIGKILL: i32 = libc::SIGKILL;
#[cfg(not(unix))]
const SIGTERM: i32 = 15;
#[cfg(not(unix))]
const SIGKILL: i32 = 9;

/// Signals a whole process group (negative pid). Returns whether the signal was
/// delivered — false for an already-reaped group, and always off Unix.
#[cfg(unix)]
fn signal_group(pid: u32, signal: i32) -> bool {
    // SAFETY: `kill` is a plain syscall; a negative pid addresses the group.
    unsafe { libc::kill(-(pid as i32), signal) == 0 }
}

#[cfg(not(unix))]
fn signal_group(_pid: u32, _signal: i32) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notifier::RecordingNotifier;
    use crate::state::layout::SlotId;
    use crate::state::workspace::ShellLine;
    use serde_json::json;
    use std::time::Duration;

    fn gui() -> Arc<GuiState> {
        Arc::new(GuiState::new(Arc::new(RecordingNotifier::new())))
    }

    #[tokio::test]
    async fn test_output_lines_reach_the_shell_log() {
        let gui = gui();
        run_to_completion(gui.clone(), "ws-1".into(), "echo hello; echo oops 1>&2".into())
            .await
            .unwrap();

        let log = gui.shell_log("ws-1").unwrap();
        let lines: Vec<(ShellLine, &str)> = log.iter().map(|e| (e.kind, e.text.as_str())).collect();
        assert_eq!(lines[0], (ShellLine::Command, "echo hello; echo oops 1>&2"));
        assert!(lines.contains(&(ShellLine::Stdout, "hello")), "stdout missing: {lines:?}");
        assert!(lines.contains(&(ShellLine::Stderr, "oops")), "stderr missing: {lines:?}");
        // Every line of one run carries the same run id, so the panel can
        // group a run's lines even when two runs interleave.
        assert!(log.iter().all(|e| e.run == log[0].run && e.run.starts_with("script-")));
    }

    #[tokio::test]
    async fn test_the_message_log_keeps_only_the_command_line() {
        let gui = gui();
        run_to_completion(gui.clone(), "ws-1".into(), "echo hello".into()).await.unwrap();
        let texts: Vec<String> =
            gui.messages("ws-1").unwrap().into_iter().map(|m| m.text).collect();
        assert_eq!(texts, ["$ echo hello"]);
    }

    #[tokio::test]
    async fn test_nonzero_exit_is_logged() {
        let gui = gui();
        run_to_completion(gui.clone(), "ws-1".into(), "exit 3".into()).await.unwrap();
        let log = gui.shell_log("ws-1").unwrap();
        let last = log.last().unwrap();
        assert_eq!((last.kind, last.text.as_str()), (ShellLine::Status, "exit 3"));
    }

    #[tokio::test]
    async fn test_unknown_workspace_errors() {
        let gui = gui();
        assert!(run_to_completion(gui, "ws-99".into(), "echo hi".into()).await.is_err());
    }

    #[tokio::test]
    async fn test_stop_script_kills_the_run_and_its_children() {
        // Escape during a question must actually end the script (doc
        // "Script sessions"): the whole process group dies, so the `mf gui
        // input` child blocked on the question goes with it.
        let notifier = Arc::new(RecordingNotifier::new());
        let gui = Arc::new(GuiState::new(notifier.clone()));
        let running = tokio::spawn({
            let gui = gui.clone();
            // A child that outlives its parent shell unless the GROUP is signalled.
            async move { run_to_completion(gui, "ws-1".into(), "sleep 30 & sleep 30".into()).await }
        });
        // Wait until the run is registered with the pid stop_script needs.
        let mut task_id = None;
        for _ in 0..100 {
            let payloads = notifier.payloads(crate::events::SCRIPT_TASK_CHANGED);
            let id = payloads
                .last()
                .and_then(|p| p["tasks"][0]["task"].as_str().map(str::to_string))
                .filter(|id| gui.script_pid(id).is_some());
            if let Some(id) = id {
                task_id = Some(id);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let task_id = task_id.expect("the run should be registered with a pid");

        // A workspace the script opened: `sleep` has no exit trap to close it.
        let scratch = take_over(&gui, &task_id);

        assert!(
            stop_script(&gui, &task_id, Stop::Terminate),
            "stopping a live run reports success"
        );
        let stopped = tokio::time::timeout(Duration::from_secs(5), running).await;
        assert!(stopped.is_ok(), "the killed script must not outlive its stop");
        assert!(
            gui.workspaces().iter().all(|w| w.id != scratch),
            "the GUI closes what the killed script opened"
        );
        // A stop the user asked for is not a failure, and is not reported as
        // one: "stopped", and nothing in red.
        let statuses = notifier.payloads(crate::events::STATUS_MESSAGE);
        assert!(
            statuses.iter().all(|p| p["kind"] != "error"),
            "a stopped script is not an error: {statuses:?}"
        );
        let last = statuses.last().expect("the stop is reported");
        assert_eq!(last["text"], json!("sleep 30 & sleep 30 stopped"));
        let log = gui.shell_log("ws-1").unwrap();
        assert!(
            log.iter().any(|e| e.kind == ShellLine::Status && e.text == "stopped"),
            "the shell log says so too"
        );
        assert!(log.iter().all(|e| !e.text.starts_with("exit")), "and names no exit code");
        // The run is gone from the registry, so a second stop finds nothing.
        assert!(!stop_script(&gui, &task_id, Stop::Terminate));
    }

    /// Runs `command_line` and returns its run id once it has a pid to signal.
    async fn started(
        gui: &Arc<GuiState>,
        notifier: &Arc<RecordingNotifier>,
        command_line: &str,
    ) -> (String, tokio::task::JoinHandle<Result<(), String>>) {
        let running = tokio::spawn({
            let gui = gui.clone();
            let command_line = command_line.to_string();
            async move { run_to_completion(gui, "ws-1".into(), command_line).await }
        });
        for _ in 0..100 {
            let payloads = notifier.payloads(crate::events::SCRIPT_TASK_CHANGED);
            let id = payloads
                .last()
                .and_then(|p| p["tasks"][0]["task"].as_str().map(str::to_string))
                .filter(|id| gui.script_pid(id).is_some());
            if let Some(id) = id {
                return (id, running);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the run should be registered with a pid");
    }

    #[tokio::test]
    async fn test_a_script_that_ignores_the_stop_is_ended_by_the_kill() {
        // Stopping asks (SIGTERM) and nothing more: a script may trap it to
        // finish what it is doing, or ignore it. Killing (SIGKILL) is the
        // user's second, separate command — the GUI never escalates by itself.
        let notifier = Arc::new(RecordingNotifier::new());
        let gui = Arc::new(GuiState::new(notifier.clone()));
        let (task_id, mut running) = started(&gui, &notifier, "trap '' TERM; sleep 30").await;
        // Let the shell install its trap before it is signalled.
        tokio::time::sleep(Duration::from_millis(300)).await;

        assert!(stop_script(&gui, &task_id, Stop::Terminate));
        let ignored = tokio::time::timeout(Duration::from_millis(2500), &mut running).await;
        assert!(ignored.is_err(), "a stop that is ignored is not turned into a kill");

        assert!(stop_script(&gui, &task_id, Stop::Kill));
        let killed = tokio::time::timeout(Duration::from_secs(5), running).await;
        assert!(killed.is_ok(), "nothing survives the kill");
        let statuses = notifier.payloads(crate::events::STATUS_MESSAGE);
        assert_eq!(statuses.last().unwrap()["text"], json!("trap '' TERM; sleep 30 killed"));
    }

    #[test]
    fn test_script_keys_are_re_enabled_by_every_new_run() {
        // The checkbox holds for the rest of the session, but a *new* script
        // starts with its keys live: nobody should silently lose them.
        let gui = gui();
        assert!(gui.script_keys_enabled());
        gui.set_script_keys(false);
        assert!(!gui.script_keys_enabled());
        gui.script_begin("script-1", "ws-1", "gui-tag-folder.sh");
        assert!(gui.script_keys_enabled());
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

    /// The visible workspace of each slot, as `GET /gui/layout` reports it.
    fn shown(gui: &GuiState) -> (Option<String>, Option<String>) {
        let layout = gui.layout();
        let slot = |s: &crate::state::layout::SlotPayload| {
            s.visible.then(|| s.workspace_id.clone()).flatten()
        };
        (slot(&layout.left), slot(&layout.right))
    }

    /// A script's takeover, the way `mf_gui_session_open` does it: a scratch
    /// workspace of its own, shown in both slots. Returns that workspace.
    fn take_over(gui: &GuiState, task: &str) -> String {
        let scratch = gui.create_workspace_named(None, None);
        gui.script_claim_workspace(task, &scratch);
        gui.tab_assign(&scratch, SlotId::Left).unwrap();
        gui.tab_assign(&scratch, SlotId::Right).unwrap();
        scratch
    }

    #[test]
    fn test_a_stopped_script_is_cleaned_up_by_the_gui() {
        // A stopped script is killed, and a killed script cleans nothing up
        // (doc "Script sessions"): the GUI closes the workspaces the script
        // opened and gives the slots back what they showed at its launch.
        let gui = gui();
        let before = shown(&gui);
        gui.script_begin("script-1", "ws-1", "gui-tag-folder.sh");
        let scratch = take_over(&gui, "script-1");
        assert_eq!(shown(&gui), (Some(scratch.clone()), Some(scratch.clone())));

        gui.script_mark_stopped("script-1", "stopped");
        gui.script_end("script-1");

        assert!(gui.workspaces().iter().all(|w| w.id != scratch), "the scratch workspace goes");
        assert_eq!(shown(&gui), before, "the layout is the one of the launch");
    }

    #[test]
    fn test_a_script_that_ends_by_itself_keeps_what_it_opened() {
        // Only a *stopped* run is cleaned up. One that ends on its own decided
        // what to leave: `!mf gui workspace new` typed in the command input is
        // a run too, and its workspace is the whole point of it.
        let gui = gui();
        gui.script_begin("script-1", "ws-1", "mf gui workspace new");
        let scratch = take_over(&gui, "script-1");
        gui.script_end("script-1");

        assert!(gui.workspaces().iter().any(|w| w.id == scratch));
        assert_eq!(shown(&gui), (Some(scratch.clone()), Some(scratch)));
    }

    #[test]
    fn test_the_cleanup_leaves_a_slot_the_user_moved() {
        // The slots are given back, not reset: one the user pointed elsewhere
        // while the script ran shows what the user chose.
        let gui = gui();
        let other = gui.create_workspace_named(None, None);
        gui.script_begin("script-1", "ws-1", "gui-tag-folder.sh");
        let scratch = take_over(&gui, "script-1");
        gui.tab_assign(&other, SlotId::Right).unwrap();

        gui.script_mark_stopped("script-1", "stopped");
        gui.script_end("script-1");

        assert!(gui.workspaces().iter().all(|w| w.id != scratch));
        let (left, right) = shown(&gui);
        assert_eq!(left.as_deref(), Some("ws-1"));
        assert_eq!(right, Some(other));
    }

    #[test]
    fn test_the_cleanup_keeps_the_launching_workspace() {
        // The workspace a script was launched from is owned, not opened by it.
        let gui = gui();
        gui.script_begin("script-1", "ws-1", "gui-tag-folder.sh");
        gui.script_mark_stopped("script-1", "stopped");
        gui.script_end("script-1");
        assert!(gui.workspaces().iter().any(|w| w.id == "ws-1"));
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
