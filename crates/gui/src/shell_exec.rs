//! `!` shell commands from the command input (doc "Bash mode"):
//! run as a subprocess; stdout/stderr lines go to the workspace shell log
//! (shell panel type) and to the terminal that launched the GUI. The message
//! log only records that the line ran, as `$ command`.
//!
//! A line runs where the selection is, and where the previous line's `cd`
//! left the shell for as long as the selection stays (doc "shell panel").

use crate::state::workspace::{ShellLine, ShellPlace};
use crate::state::GuiState;
use std::path::{Path, PathBuf};
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
    run_in(gui, ws_id, command_line, None, None, None).await
}

/// [`run_to_completion`], the subprocess passed `context` as
/// `METAFOLDER_LABEL`: every `mf` it starts writes in that label (doc
/// "Labels and contexts"). `root` is the workspace repository's root, where
/// a line runs with nothing selected ([`working_dir`]). With `rc`, the line
/// runs in an interactive `bash` that has loaded that file first (config
/// `shell-rc`, [`rc_path`]); without, in a plain `sh`.
pub async fn run_in(
    gui: Arc<GuiState>,
    ws_id: String,
    command_line: String,
    context: Option<metafolder_core::context::Context>,
    root: Option<PathBuf>,
    rc: Option<&Path>,
) -> Result<(), String> {
    // A per-run id the subprocess can address its own progress with
    // (`mf gui progress` reads it from METAFOLDER_GUI_TASK); session-unique.
    // It also groups the run's lines in the shell log.
    static RUN_SEQ: AtomicU64 = AtomicU64::new(1);
    let task_id = format!("script-{}", RUN_SEQ.fetch_add(1, Ordering::Relaxed));

    // Fail fast on unknown workspaces (and log the invocation).
    gui.append_message(&ws_id, &format!("$ {command_line}"))?;
    gui.append_shell(&ws_id, &task_id, ShellLine::Command, &command_line)?;

    let place = gui.shell_place(&ws_id)?;
    // Where the line leaves the shell, read back once it is over: every line
    // is a `sh` of its own, so a `cd` would otherwise die with it. An EXIT
    // trap writes it on every way out, `exit N` and a failing line included,
    // and keeps the line's exit status.
    let cwd_file = metafolder_core::auth::runtime_dir()
        .ok()
        .map(|dir| dir.join(format!("shell-cwd-{}-{task_id}", std::process::id())));
    let line = match &cwd_file {
        Some(file) => format!("trap 'pwd > {}' EXIT; {command_line}", shell_quote(file)),
        None => command_line.clone(),
    };
    let mut command = match rc {
        None => {
            let mut command = Command::new("sh");
            command.arg("-c").arg(&line);
            // Its own process group, so `script:stop` can signal the script
            // *and* every child it spawned (the `mf gui input` blocked on a
            // question above all) with one killpg — signalling the shell alone
            // would leave those behind.
            #[cfg(unix)]
            command.process_group(0);
            command
        }
        Some(rc) => interactive_bash(rc, &line),
    };
    if let Some(dir) = working_dir(&place, root.as_deref()) {
        // `PWD` too, or `pwd` names the GUI's directory's resolution of it.
        command.current_dir(&dir).env("PWD", &dir);
    }
    command
        .env("METAFOLDER_GUI_TASK", &task_id)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    match context {
        Some(context) => command.env(metafolder_core::context::LABEL_ENV, context.to_string()),
        // The GUI's own context, if it was started in one, is not the run's.
        None => command.env_remove(metafolder_core::context::LABEL_ENV),
    };
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
    if let Some(file) = cwd_file {
        let left = std::fs::read_to_string(&file).unwrap_or_default();
        let _ = std::fs::remove_file(&file);
        let left = left.strip_suffix('\n').unwrap_or(&left);
        if !left.is_empty() {
            gui.set_shell_cd(&ws_id, PathBuf::from(left), place.selection)?;
        }
    }

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

/// The shell rc file a config `shell-rc` value names: `~/` is the home
/// directory, and an empty value is none (the line runs in a plain `sh`).
pub fn rc_path(setting: &str) -> Option<PathBuf> {
    if setting.is_empty() {
        return None;
    }
    match (setting.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => Some(PathBuf::from(home).join(rest)),
        _ => Some(PathBuf::from(setting)),
    }
}

/// `bash` running `line` with the user's configuration (doc "shell panel").
///
/// The shell is interactive (`-i`), for a `.bashrc` stops at once in any other
/// — the guard every distribution ships — and aliases expand only there; it
/// loads `rc` itself (`--norc`, then `.`), so that the rest follows:
///
/// * an interactive bash with no terminal complains about it before reading
///   anything, so it starts with its stderr on `/dev/null` and the real one
///   on fd 3, given back first thing — the rc file's own errors still show;
/// * the line runs in a subshell, which is not interactive: an `exit` there
///   prints no `exit`, and dies of `SIGTERM`, which the interactive shell
///   ignores — its exit status is then the shell's;
/// * a new session with no controlling terminal (`setsid`, a process group of
///   its own for `script:stop` too): with the terminal the GUI was started
///   from, an interactive bash in a background group would stop itself trying
///   to take it over.
///
/// Each step is on its own line: an alias the rc file defines applies only
/// to the lines read after it.
fn interactive_bash(rc: &Path, line: &str) -> Command {
    let rc = shell_quote(rc);
    let script = format!("exec 2>&3 3>&-\n[ -r {rc} ] && . {rc}\n(\n{line}\n)");
    let mut command = Command::new("bash");
    command.arg("--norc").arg("-i").arg("-c").arg(script);
    #[cfg(unix)]
    // SAFETY: only async-signal-safe calls between fork and exec, on a
    // C string that needs no allocation.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 || libc::dup2(2, 3) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            let null = libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY);
            if null == -1 || libc::dup2(null, 2) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            libc::close(null);
            Ok(())
        });
    }
    command
}

/// Where a shell line runs (doc "shell panel"): where the previous line's
/// `cd` left the shell, if the selection has not changed since; else the
/// selected folder, or the folder of the selected file — the nearest one still
/// there for a path that is gone; else the repository root. `None` (the GUI's
/// own directory) with none of them.
pub fn working_dir(place: &ShellPlace, root: Option<&Path>) -> Option<PathBuf> {
    if let Some(cd) = &place.cd {
        return Some(cd.clone());
    }
    if let Some(path) = &place.selected_path {
        let found = Path::new(path).ancestors().find(|dir| dir.is_dir());
        if let Some(dir) = found.filter(|dir| dir.is_absolute()) {
            return Some(dir.to_path_buf());
        }
    }
    root.map(Path::to_path_buf)
}

/// The workspace repository's root, as the daemon reports it; `None` with no
/// repository or no answer.
pub async fn repo_root(
    gui: &GuiState,
    daemon: &crate::daemon_proxy::DaemonProxy,
    ws_id: &str,
) -> Option<PathBuf> {
    let repo = gui.active_repo(ws_id).ok()?;
    let info = daemon.request("GET", &format!("/repos/{repo}"), None).await.ok()?;
    (info.status == 200).then_some(())?;
    info.body["root"].as_str().map(PathBuf::from)
}

/// Where the workspace's next shell line would run ([`working_dir`]): what
/// the shell panel shows after `$`, and where `Tab` completes.
pub async fn cwd_of(
    gui: &GuiState,
    daemon: &crate::daemon_proxy::DaemonProxy,
    ws_id: &str,
) -> Option<PathBuf> {
    let place = gui.shell_place(ws_id).ok()?;
    match working_dir(&place, None) {
        Some(dir) => Some(dir),
        None => repo_root(gui, daemon, ws_id).await,
    }
}

/// [`cwd_of`], for the shell panel's prompt; `None` is the GUI's directory.
#[tauri::command]
pub async fn shell_cwd(
    app: tauri::State<'_, Arc<crate::commands::App>>,
    ws_id: String,
) -> Result<Option<String>, String> {
    Ok(cwd_of(&app.gui, &app.daemon, &ws_id).await.map(|dir| dir.to_string_lossy().into_owned()))
}

/// `path` as one single-quoted `sh` word.
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
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

/// `label`: run it in a label of its own, so what a script writes is one
/// piece of work in the log — opened in the workspace's active repository,
/// inside the work context in force there (the proxy sends it), and passed on
/// as `METAFOLDER_LABEL`. A plain `!` line is passed the work context alone:
/// each `mf` it runs is a command with a label of its own anyway.
#[tauri::command]
pub fn run_shell(
    app: tauri::State<'_, Arc<crate::commands::App>>,
    ws_id: String,
    command_line: String,
    label: Option<String>,
) -> Result<(), String> {
    let gui = app.gui.clone();
    let daemon = app.daemon.clone();
    let message_ms = app.status_timeouts().message_ms;
    let rc = rc_path(&app.settings.shell_rc);
    tauri::async_runtime::spawn(async move {
        let context = match label {
            Some(name) => open_context(&gui, &daemon, &ws_id, &name).await,
            None => plain_line_context(&gui, &ws_id),
        };
        let root = repo_root(&gui, &daemon, &ws_id).await;
        let rc = rc.as_deref();
        if let Err(error) =
            run_in(gui.clone(), ws_id.clone(), command_line, context, root, rc).await
        {
            let _ = gui.post_status(&ws_id, &error, "error", Some(message_ms));
        }
    });
    Ok(())
}

/// The context a plain `!` line runs in: the work context in force on the
/// workspace's repository (doc "The GUI's work context"), if any — each `mf`
/// it runs then opens its command's label inside it.
fn plain_line_context(gui: &GuiState, ws_id: &str) -> Option<metafolder_core::context::Context> {
    crate::work_context::context_of(&gui.active_repo(ws_id).ok()?)
}

/// Opens a label named `name` in the workspace's active repository: the
/// context a script runs in. `None` — and the script runs as it would have —
/// when there is no repository or the daemon refuses.
async fn open_context(
    gui: &GuiState,
    daemon: &crate::daemon_proxy::DaemonProxy,
    ws_id: &str,
    name: &str,
) -> Option<metafolder_core::context::Context> {
    let repo = gui.active_repo(ws_id).ok()?;
    let body = serde_json::json!({"name": name});
    let response =
        daemon.request("POST", &format!("/repos/{repo}/labels"), Some(body)).await.ok()?;
    let label = response.body["id"].as_i64().filter(|_| response.status < 300)?;
    Some(metafolder_core::context::Context { repo: uuid::Uuid::parse_str(&repo).ok()?, label })
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

    /// A script runs in the context it was given, and a plain line in none —
    /// not even one the GUI itself was started in (doc "Labels and contexts").
    #[tokio::test]
    async fn test_a_run_is_given_its_context_and_only_it() {
        let gui = gui();
        let context = metafolder_core::context::Context { repo: uuid::Uuid::nil(), label: 7 };
        let line = "echo \"in:${METAFOLDER_LABEL:-none}\"";
        run_in(gui.clone(), "ws-1".into(), line.into(), Some(context), None, None).await.unwrap();
        run_in(gui.clone(), "ws-1".into(), line.into(), None, None, None).await.unwrap();
        let log = gui.shell_log("ws-1").unwrap();
        let out: Vec<&str> =
            log.iter().filter(|e| e.kind == ShellLine::Stdout).map(|e| e.text.as_str()).collect();
        assert_eq!(out, [format!("in:{context}").as_str(), "in:none"]);
    }

    /// A plain `!` line runs in the work context in force on the workspace's
    /// repository, if any (doc "The GUI's work context").
    #[test]
    fn test_a_plain_line_runs_in_the_work_context() {
        let gui = gui();
        let repo = uuid::Uuid::new_v4();
        gui.adopt_repo("ws-1", &repo.simple().to_string()).unwrap();
        assert_eq!(plain_line_context(&gui, "ws-1"), None);
        let context =
            crate::work_context::WorkContext { label: 9, name: "w".into(), description: None };
        crate::work_context::activate(&repo.simple().to_string(), context, "/nowhere".into());
        assert_eq!(
            plain_line_context(&gui, "ws-1"),
            Some(metafolder_core::context::Context { repo, label: 9 })
        );
        crate::work_context::forget(&repo.simple().to_string());
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

    /// A disposable directory under `$TMPDIR/metafolder-tests/`, removed by
    /// the caller.
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join("metafolder-tests")
            .join(format!("mf-shell-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/file.txt"), "x").unwrap();
        dir
    }

    fn stdout(gui: &GuiState) -> Vec<String> {
        let log = gui.shell_log("ws-1").unwrap();
        log.into_iter().filter(|e| e.kind == ShellLine::Stdout).map(|e| e.text).collect()
    }

    /// A selected folder is where the shell goes; a selected file, its
    /// folder; a selected path that is gone, its nearest folder still there;
    /// nothing selected, the repository root (doc "shell panel").
    #[test]
    fn test_the_working_directory_follows_the_selection() {
        let dir = scratch("follow");
        let place = |path: Option<std::path::PathBuf>| crate::state::workspace::ShellPlace {
            selection: serde_json::Value::Null,
            cd: None,
            selected_path: path.map(|p| p.to_string_lossy().into_owned()),
        };
        let root = Some(dir.as_path());
        assert_eq!(working_dir(&place(Some(dir.join("sub"))), root), Some(dir.join("sub")));
        assert_eq!(
            working_dir(&place(Some(dir.join("sub/file.txt"))), root),
            Some(dir.join("sub"))
        );
        assert_eq!(working_dir(&place(Some(dir.join("sub/gone/x"))), root), Some(dir.join("sub")));
        assert_eq!(working_dir(&place(None), root), Some(dir.clone()));
        assert_eq!(working_dir(&place(None), None), None);
        let mut cd = place(Some(dir.join("sub")));
        cd.cd = Some(dir.clone());
        assert_eq!(working_dir(&cd, None), Some(dir.clone()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A line runs where the selection is, and a `cd` carries over to the
    /// next line — until the selection changes.
    #[tokio::test]
    async fn test_a_cd_carries_over_to_the_next_line() {
        let dir = scratch("cd");
        let gui = gui();
        gui.set_var("ws-1", "selected_paths", json!([dir.join("sub/file.txt")])).unwrap();
        run_in(gui.clone(), "ws-1".into(), "pwd; cd ..".into(), None, None, None).await.unwrap();
        run_in(gui.clone(), "ws-1".into(), "pwd".into(), None, None, None).await.unwrap();
        gui.set_var("ws-1", "selected_paths", json!([dir.join("sub")])).unwrap();
        run_in(gui.clone(), "ws-1".into(), "pwd".into(), None, None, None).await.unwrap();
        let sub = dir.join("sub").to_string_lossy().into_owned();
        assert_eq!(stdout(&gui), [sub.clone(), dir.to_string_lossy().into_owned(), sub]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The `cd` takes nothing from the line's exit status.
    #[tokio::test]
    async fn test_a_failing_line_still_records_its_cd() {
        let dir = scratch("fail");
        let gui = gui();
        let line = format!("cd '{}' && exit 4", dir.display());
        run_in(gui.clone(), "ws-1".into(), line, None, None, None).await.unwrap();
        let log = gui.shell_log("ws-1").unwrap();
        assert_eq!(log.last().unwrap().text, "exit 4");
        assert_eq!(gui.shell_place("ws-1").unwrap().cd, Some(dir.clone()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A shell rc file in a scratch directory, guarded the way a distribution's
    /// `.bashrc` is: it loads only in an interactive shell.
    fn rc_file(dir: &Path, body: &str) -> PathBuf {
        let rc = dir.join("bashrc");
        std::fs::write(&rc, format!("[[ $- != *i* ]] && return\n{body}\n")).unwrap();
        rc
    }

    async fn run_rc(gui: &Arc<GuiState>, line: &str, rc: &Path) {
        run_in(gui.clone(), "ws-1".into(), line.into(), None, None, Some(rc)).await.unwrap();
    }

    /// With a shell rc file, a line has the user's configuration — an alias, a
    /// function, a variable — even behind the interactive guard, and bash's
    /// complaints about having no terminal are not shown (doc "shell panel").
    #[tokio::test]
    async fn test_a_line_has_the_rc_files_configuration() {
        let dir = scratch("rc");
        let rc = rc_file(&dir, "alias hi='echo aliased'\nf() { echo func; }\nexport MF_RC=var");
        let gui = gui();
        run_rc(&gui, "hi; f; echo $MF_RC", &rc).await;
        let log = gui.shell_log("ws-1").unwrap();
        let lines: Vec<(ShellLine, &str)> =
            log.iter().skip(1).map(|e| (e.kind, e.text.as_str())).collect();
        assert_eq!(
            lines,
            [
                (ShellLine::Stdout, "aliased"),
                (ShellLine::Stdout, "func"),
                (ShellLine::Stdout, "var")
            ]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The rc file's own errors are shown; an `exit` keeps its status and adds
    /// no `exit` line; a `cd` still carries over.
    #[tokio::test]
    async fn test_a_line_with_an_rc_file_keeps_errors_status_and_cd() {
        let dir = scratch("rc-exit");
        let rc = rc_file(&dir, "echo rc-says >&2");
        let gui = gui();
        run_rc(&gui, &format!("cd '{}' && exit 4", dir.display()), &rc).await;
        let log = gui.shell_log("ws-1").unwrap();
        let lines: Vec<(ShellLine, &str)> =
            log.iter().skip(1).map(|e| (e.kind, e.text.as_str())).collect();
        assert_eq!(lines, [(ShellLine::Stderr, "rc-says"), (ShellLine::Status, "exit 4")]);
        assert_eq!(gui.shell_place("ws-1").unwrap().cd, Some(dir.clone()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A missing rc file is skipped, as bash skips a missing `.bashrc`.
    #[tokio::test]
    async fn test_a_missing_rc_file_is_skipped() {
        let dir = scratch("rc-missing");
        let gui = gui();
        run_rc(&gui, "echo ran", &dir.join("nowhere")).await;
        let log = gui.shell_log("ws-1").unwrap();
        let lines: Vec<&str> = log.iter().skip(1).map(|e| e.text.as_str()).collect();
        assert_eq!(lines, ["ran"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `~/` names the home directory, as in bash.
    #[test]
    fn test_the_rc_path_expands_the_home_directory() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(rc_path("~/.bashrc"), Some(PathBuf::from(home).join(".bashrc")));
        assert_eq!(rc_path("/etc/rc"), Some(PathBuf::from("/etc/rc")));
        assert_eq!(rc_path(""), None);
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
        started_with(gui, notifier, command_line, None).await
    }

    /// [`started`], the line run with the shell rc file `rc`.
    async fn started_with(
        gui: &Arc<GuiState>,
        notifier: &Arc<RecordingNotifier>,
        command_line: &str,
        rc: Option<PathBuf>,
    ) -> (String, tokio::task::JoinHandle<Result<(), String>>) {
        let running = tokio::spawn({
            let gui = gui.clone();
            let command_line = command_line.to_string();
            async move { run_in(gui, "ws-1".into(), command_line, None, None, rc.as_deref()).await }
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

    /// The interactive shell that loads the rc file ignores `SIGTERM`, as
    /// every interactive bash does: the stop must still end the line.
    #[tokio::test]
    async fn test_a_line_with_an_rc_file_is_stopped() {
        let dir = scratch("rc-stop");
        let rc = rc_file(&dir, "");
        let notifier = Arc::new(RecordingNotifier::new());
        let gui = Arc::new(GuiState::new(notifier.clone()));
        let (task_id, running) =
            started_with(&gui, &notifier, "sleep 30 & sleep 30", Some(rc)).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(stop_script(&gui, &task_id, Stop::Terminate));
        let stopped = tokio::time::timeout(Duration::from_secs(5), running).await;
        assert!(stopped.is_ok(), "the stopped line must not outlive its stop");
        std::fs::remove_dir_all(&dir).unwrap();
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
