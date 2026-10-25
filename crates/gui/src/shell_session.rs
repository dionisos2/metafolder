//! The workspace's persistent shell (doc "shell panel"): one `bash` per
//! workspace, started by its first typed line and kept for the next ones, as a
//! console keeps its shell — a `cd`, a variable, a function or an alias of one
//! line is there for the next, and the user's rc file is loaded once.
//!
//! The shell reads its commands from its standard input, a pipe only the GUI
//! writes to, one line at a time; the GUI writes a line only once the previous
//! one is over, so lines run in turn. Each line is followed by `__mf_done`,
//! which marks its end in-band on stdout and stderr (so that every line of
//! output it printed is attributed before its end is reported) and writes its
//! exit status and the shell's directory on fd 3, a pipe of its own.
//!
//! A line reads its standard input from a FIFO of its own, which the GUI
//! writes what the user types into while it runs, and closes for an end of
//! file — never from the shell's command stream, which it would swallow.

use crate::shell_exec::{report_end, script_label, shell_quote, working_dir, RunningGuard};
use crate::state::workspace::ShellLine;
use crate::state::GuiState;
use std::collections::HashMap;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::mpsc;

/// What ends a line's output on stdout and stderr, at the end of a line of
/// its own or of the line it printed last without a newline.
const DONE: &str = "\u{1e}mf-done";

/// The workspaces' persistent shells, by workspace id.
#[derive(Default)]
pub struct ShellSessions {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}

/// What a session's readers and its exit report to the line running.
#[derive(Debug)]
enum Event {
    /// stdout (`false`) or stderr (`true`) reached the line's end mark.
    Done(bool),
    /// The shell opened the line's input: closing it now is an end of file.
    InputOpen,
    /// The line's exit status and the directory the shell is left in — or,
    /// from its exit trap, the shell's.
    Status(i32, PathBuf),
    /// The shell itself ended — an `exit`, a kill: its exit code, if any.
    Exited(Option<i32>),
}

struct Session {
    /// The shell's pid, which is also its process group's.
    pid: u32,
    stdin: tokio::sync::Mutex<ChildStdin>,
    /// Taken for the whole of a line: the next waits its turn.
    events: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<Event>>>,
    /// What the shell is to read before the first line (see [`start`]).
    boot: Mutex<Option<String>>,
    /// The run output is attributed to: the line running, or the last one
    /// (a background job printing later).
    run: Arc<Mutex<String>>,
    /// The line running and the writing end of its standard input.
    current: Mutex<Option<Current>>,
    alive: Arc<AtomicBool>,
}

struct Current {
    run: String,
    /// `None` once the user ended its input (end of file).
    input: Option<std::fs::File>,
    /// Whether the shell has opened the FIFO: closing it before would leave
    /// the shell's `open` waiting for a writer for ever.
    opened: bool,
    /// An end of file asked for before it was, carried out once it is.
    eof: bool,
}

impl ShellSessions {
    /// Runs `command_line`, typed by the user, in workspace `ws_id`'s shell —
    /// started first if it has none, loading `rc` — once the line before it
    /// is over; returns when it is. `context` is its `METAFOLDER_LABEL`;
    /// `root` the repository root, where it runs with nothing selected.
    pub async fn run(
        &self,
        gui: &Arc<GuiState>,
        ws_id: &str,
        command_line: &str,
        context: Option<metafolder_core::context::Context>,
        root: Option<PathBuf>,
        rc: Option<PathBuf>,
    ) -> Result<(), String> {
        let task_id = crate::shell_exec::next_task_id();
        gui.append_message(ws_id, &format!("$ {command_line}"))?;
        gui.append_shell(ws_id, &task_id, ShellLine::Command, command_line)?;

        // The shell, once it is this line's turn: one that ended while the
        // line waited (an `exit`, a kill) gives way to a new one.
        let (session, mut events) = loop {
            let place = gui.shell_place(ws_id)?;
            let session =
                self.session(gui, ws_id, &task_id, &place, root.as_deref(), rc.as_deref())?;
            let events = session.events.clone().lock_owned().await;
            if session.alive.load(Ordering::SeqCst) {
                break (session, events);
            }
            self.forget(ws_id, &session);
        };
        // Read again now that it is this line's turn: the line before may have
        // moved the shell, and the selection may have changed meanwhile.
        let place = gui.shell_place(ws_id)?;
        self.run_turn(
            gui,
            ws_id,
            &task_id,
            command_line,
            context,
            root,
            &session,
            &mut events,
            place,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_turn(
        &self,
        gui: &Arc<GuiState>,
        ws_id: &str,
        task_id: &str,
        command_line: &str,
        context: Option<metafolder_core::context::Context>,
        root: Option<PathBuf>,
        session: &Arc<Session>,
        events: &mut mpsc::UnboundedReceiver<Event>,
        place: crate::state::workspace::ShellPlace,
    ) -> Result<(), String> {
        *session.run.lock().unwrap_or_else(|e| e.into_inner()) = task_id.to_string();
        gui.script_begin(task_id, ws_id, &script_label(command_line));
        let _running = RunningGuard { gui: gui.clone(), task_id: task_id.to_string() };
        gui.script_set_pid(task_id, Some(session.pid));

        let fifo = input_fifo(task_id)?;
        let input = std::fs::OpenOptions::new()
            .read(true) // never blocks, and keeps the FIFO open until the line ends
            .write(true)
            .open(&fifo)
            .map_err(|e| format!("cannot open the line's input: {e}"))?;
        *session.current.lock().unwrap_or_else(|e| e.into_inner()) = Some(Current {
            run: task_id.to_string(),
            input: Some(input),
            opened: false,
            eof: false,
        });

        // Where the line runs, when the selection moved the shell since the
        // last line (a `cd` of its own otherwise left it where it is).
        let cd = match place.cd {
            Some(_) => String::new(),
            None => working_dir(&place, root.as_deref())
                .map(|dir| format!("; cd -- {}", shell_quote(&dir)))
                .unwrap_or_default(),
        };
        let label = match context {
            Some(context) => format!(
                "export {}={}",
                metafolder_core::context::LABEL_ENV,
                quote(&context.to_string())
            ),
            None => format!("unset {}", metafolder_core::context::LABEL_ENV),
        };
        // The line itself is `eval`ed, so that one that does not parse fails
        // at once rather than waiting for the rest of a construct. Its input
        // is its FIFO, opened (and said so on fd 3) before it runs.
        let boot = session.boot.lock().unwrap_or_else(|e| e.into_inner()).take();
        let script = format!(
            "{boot}export METAFOLDER_GUI_TASK={task}; {label}{cd}; exec {{__mf_in}}<{fifo}; \
             printf 'in\\0\\0' >&3\neval {line} <&$__mf_in\n__mf_done\n",
            boot = boot.unwrap_or_default(),
            task = quote(task_id),
            line = quote(command_line),
            fifo = shell_quote(&fifo),
        );
        let sent = session.stdin.lock().await.write_all(script.as_bytes()).await;

        let (mut out, mut err, mut status) = (false, false, None);
        let mut exited = None;
        if sent.is_ok() {
            while !(out && err && status.is_some()) {
                match events.recv().await {
                    Some(Event::Done(false)) => out = true,
                    Some(Event::Done(true)) => err = true,
                    Some(Event::InputOpen) => {
                        let mut current = session.current.lock().unwrap_or_else(|e| e.into_inner());
                        if let Some(current) = current.as_mut() {
                            current.opened = true;
                            if current.eof {
                                current.input = None;
                            }
                        }
                    }
                    Some(Event::Status(code, dir)) => status = Some((code, dir)),
                    Some(Event::Exited(code)) => {
                        exited = Some(code);
                        break;
                    }
                    None => {
                        exited = Some(None);
                        break;
                    }
                }
            }
        } else {
            exited = Some(None);
        }
        *session.current.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let _ = std::fs::remove_file(&fifo);
        // Over: the shell's group must stop naming this run, for a stop now
        // would interrupt the next line.
        gui.script_set_pid(task_id, None);

        let failure = match (exited, status) {
            (None, Some((code, dir))) => {
                gui.set_shell_cd(ws_id, dir, place.selection)?;
                (code != 0).then(|| code.to_string())
            }
            (Some(code), status) => {
                // The shell is gone with the line: the next one starts another,
                // where this one was left (its exit trap said where).
                self.forget(ws_id, session);
                if let Some((_, dir)) = status {
                    gui.set_shell_cd(ws_id, dir, place.selection)?;
                }
                code.filter(|code| *code != 0).map(|code| code.to_string())
            }
            (None, None) => unreachable!("the loop ends with a status or an exit"),
        };
        report_end(gui, ws_id, task_id, command_line, failure)
    }

    /// Workspace `ws_id`'s shell, started (in the directory the line runs in)
    /// if it has none or it has ended.
    fn session(
        &self,
        gui: &Arc<GuiState>,
        ws_id: &str,
        task_id: &str,
        place: &crate::state::workspace::ShellPlace,
        root: Option<&Path>,
        rc: Option<&Path>,
    ) -> Result<Arc<Session>, String> {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(session) = sessions.get(ws_id).filter(|s| s.alive.load(Ordering::SeqCst)) {
            return Ok(session.clone());
        }
        let session = Arc::new(start(gui, ws_id, task_id, working_dir(place, root), rc)?);
        sessions.insert(ws_id.to_string(), session.clone());
        Ok(session)
    }

    fn forget(&self, ws_id: &str, session: &Arc<Session>) {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        if sessions.get(ws_id).is_some_and(|s| Arc::ptr_eq(s, session)) {
            sessions.remove(ws_id);
        }
    }

    /// Hands `text` (a line; `None`: the end of the input) to the line running
    /// in workspace `ws_id`, and echoes it in its run. False when no line runs
    /// there.
    pub fn input(&self, gui: &GuiState, ws_id: &str, text: Option<&str>) -> Result<bool, String> {
        let Some(session) = self.live(ws_id) else { return Ok(false) };
        let mut current = session.current.lock().unwrap_or_else(|e| e.into_inner());
        let Some(current) = current.as_mut() else { return Ok(false) };
        match text {
            Some(text) => {
                let Some(input) = current.input.as_mut() else {
                    return Err("the line's input is closed".into());
                };
                input
                    .write_all(format!("{text}\n").as_bytes())
                    .map_err(|e| format!("cannot write to the line: {e}"))?;
                gui.append_shell(ws_id, &current.run, ShellLine::Input, text)?;
            }
            // Closed once the shell has opened it.
            None if current.opened => current.input = None,
            None => current.eof = true,
        }
        Ok(true)
    }

    /// The run id of the line running in workspace `ws_id`, if any.
    pub fn running(&self, ws_id: &str) -> Option<String> {
        let session = self.live(ws_id)?;
        let current = session.current.lock().unwrap_or_else(|e| e.into_inner());
        current.as_ref().map(|current| current.run.clone())
    }

    /// Whether `task_id` is a line of a persistent shell: stopping it is an
    /// interrupt, which ends the line and keeps the shell.
    pub fn holds(&self, task_id: &str) -> bool {
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions.values().any(|session| {
            let current = session.current.lock().unwrap_or_else(|e| e.into_inner());
            current.as_ref().is_some_and(|current| current.run == task_id)
        })
    }

    /// Ends workspace `ws_id`'s shell, as closing a terminal does (`SIGHUP` to
    /// its group).
    pub fn close(&self, ws_id: &str) {
        let session = self.sessions.lock().unwrap_or_else(|e| e.into_inner()).remove(ws_id);
        if let Some(session) = session {
            signal_group(session.pid, libc::SIGHUP);
        }
    }

    fn live(&self, ws_id: &str) -> Option<Arc<Session>> {
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions.get(ws_id).filter(|s| s.alive.load(Ordering::SeqCst)).cloned()
    }
}

/// `text` as one single-quoted shell word.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// A new FIFO for the standard input of run `task_id`.
fn input_fifo(task_id: &str) -> Result<PathBuf, String> {
    let dir = metafolder_core::auth::runtime_dir().map_err(|e| e.to_string())?;
    let fifo = dir.join(format!("shell-in-{}-{task_id}", std::process::id()));
    let _ = std::fs::remove_file(&fifo);
    let path =
        std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).map_err(|e| e.to_string())?;
    // SAFETY: a plain syscall on a NUL-terminated path.
    if unsafe { libc::mkfifo(path.as_ptr(), 0o600) } == -1 {
        return Err(format!("cannot create the line's input: {}", std::io::Error::last_os_error()));
    }
    Ok(fifo)
}

#[cfg(unix)]
fn signal_group(pid: u32, signal: i32) -> bool {
    // SAFETY: `kill` is a plain syscall; a negative pid addresses the group.
    unsafe { libc::kill(-(pid as i32), signal) == 0 }
}

/// Starts a workspace's shell in `dir`, loading `rc`.
///
/// The shell is interactive (`-i`): a `.bashrc` stops at once in any other —
/// the guard every distribution ships — and aliases expand only there. Without
/// line editing (`--noediting`), it does not echo what it reads; it loads `rc`
/// itself (`--norc`, then `.`), then drops the prompts, the prompt command and
/// the history, which are a terminal's. And:
///
/// * an interactive bash with no terminal complains about it before reading
///   anything, so it starts with its stderr on `/dev/null` and the real one
///   on fd 4, given back first thing — the rc file's own errors still show;
/// * it runs in a new session with no controlling terminal (`setsid`, a
///   process group of its own for `script:stop` too): with the terminal the
///   GUI was started from, an interactive bash in a background group would
///   stop itself trying to take it over;
/// * the signals the GUI may have been started with ignored (a background
///   job of a non-interactive shell ignores `SIGINT`) are reset: an ignored
///   signal is inherited, and bash cannot reset it for its children — a
///   line could not be interrupted.
///
/// What the rc file prints is attributed to the line that started the shell,
/// `task_id`.
fn start(
    gui: &Arc<GuiState>,
    ws_id: &str,
    task_id: &str,
    dir: Option<PathBuf>,
    rc: Option<&Path>,
) -> Result<Session, String> {
    let (status_tx, status_rx) =
        tokio::net::unix::pipe::pipe().map_err(|e| format!("cannot start the shell: {e}"))?;
    let status_fd = status_tx.into_blocking_fd().map_err(|e| e.to_string())?;
    let mut command = Command::new("bash");
    command
        .args(["--norc", "--noediting", "-i"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env_remove(metafolder_core::context::LABEL_ENV);
    if let Some(dir) = &dir {
        // `PWD` too, or `pwd` names the GUI's directory's resolution of it.
        command.current_dir(dir).env("PWD", dir);
    }
    {
        use std::os::fd::AsRawFd;
        let status = status_fd.as_raw_fd();
        // SAFETY: only async-signal-safe calls between fork and exec, on a C
        // string that needs no allocation.
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() == -1 || libc::dup2(status, 3) == -1 || libc::dup2(2, 4) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                let null = libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY);
                if null == -1 || libc::dup2(null, 2) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::close(null);
                for signal in [libc::SIGINT, libc::SIGQUIT, libc::SIGTERM, libc::SIGHUP] {
                    libc::signal(signal, libc::SIG_DFL);
                }
                Ok(())
            });
        }
    }
    let mut child = command.spawn().map_err(|e| format!("cannot start the shell: {e}"))?;
    drop(status_fd);
    let pid = child.id().ok_or("the shell ended at once")?;
    let stdin = child.stdin.take().expect("stdin piped");
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    let rc = match rc {
        Some(rc) => {
            let rc = shell_quote(rc);
            format!("[ -r {rc} ] && . {rc}; ")
        }
        None => String::new(),
    };
    // One line, prompts dropped on it: the rc file sets its own, which the
    // shell would print before reading the next line. Then `__mf_done`: the end
    // marks (`\036` is the `\x1e` of [`DONE`]), then the status record.
    let boot = format!(
        "exec 2>&4 4>&-; {rc}PS1= PS2= PS0=; unset PROMPT_COMMAND HISTFILE; set +H +o history\n\
         __mf_done() {{ local s=$?; exec {{__mf_in}}<&-; printf '\\036mf-done\\n'; \
         printf '\\036mf-done\\n' >&2; printf '%s\\0%s\\0' \"$s\" \"$PWD\" >&3; }}\n\
         trap 'printf \"%s\\0%s\\0\" \"$?\" \"$PWD\" >&3' EXIT\n"
    );
    let (tx, rx) = mpsc::unbounded_channel();
    let run = Arc::new(Mutex::new(task_id.to_string()));
    let alive = Arc::new(AtomicBool::new(true));
    tokio::spawn(forward(gui.clone(), ws_id.to_string(), run.clone(), stdout, false, tx.clone()));
    tokio::spawn(forward(gui.clone(), ws_id.to_string(), run.clone(), stderr, true, tx.clone()));
    tokio::spawn(statuses(status_rx, tx.clone()));
    tokio::spawn({
        let alive = alive.clone();
        async move {
            let status = child.wait().await;
            alive.store(false, Ordering::SeqCst);
            let _ = tx.send(Event::Exited(status.ok().and_then(|s| s.code())));
        }
    });
    Ok(Session {
        pid,
        stdin: tokio::sync::Mutex::new(stdin),
        events: Arc::new(tokio::sync::Mutex::new(rx)),
        boot: Mutex::new(Some(boot)),
        run,
        current: Mutex::new(None),
        alive,
    })
}

/// Streams one output pipe of the shell into the shell log, under the run
/// running, echoing to the terminal that launched the GUI; reports each end
/// mark it meets.
async fn forward(
    gui: Arc<GuiState>,
    ws_id: String,
    run: Arc<Mutex<String>>,
    reader: impl AsyncRead + Unpin,
    to_stderr: bool,
    events: mpsc::UnboundedSender<Event>,
) {
    let mut reader = BufReader::new(reader);
    let mut bytes = Vec::new();
    loop {
        bytes.clear();
        match reader.read_until(b'\n', &mut bytes).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let text = String::from_utf8_lossy(&bytes);
        let text = text.strip_suffix('\n').unwrap_or(&text);
        let (text, done) = match text.strip_suffix(DONE) {
            Some(before) => (before, true),
            None => (text, false),
        };
        if !(done && text.is_empty()) {
            let kind = if to_stderr {
                eprintln!("{text}");
                ShellLine::Stderr
            } else {
                println!("{text}");
                ShellLine::Stdout
            };
            let run = run.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let _ = gui.append_shell(&ws_id, &run, kind, text);
        }
        if done {
            let _ = events.send(Event::Done(to_stderr));
        }
    }
}

/// Reads the `status\0dir\0` records `__mf_done` writes on fd 3.
async fn statuses(
    mut reader: tokio::net::unix::pipe::Receiver,
    events: mpsc::UnboundedSender<Event>,
) {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
        }
        // A record is two NUL-terminated fields.
        while let Some(end) =
            buffer.iter().enumerate().filter(|(_, b)| **b == 0).nth(1).map(|(i, _)| i)
        {
            let record: Vec<u8> = buffer.drain(..=end).collect();
            let mut fields = record.split(|b| *b == 0);
            let code = String::from_utf8_lossy(fields.next().unwrap_or_default());
            let dir = std::ffi::OsStr::from_bytes(fields.next().unwrap_or_default());
            let event = match &*code {
                "in" => Event::InputOpen,
                code => Event::Status(code.parse().unwrap_or(-1), PathBuf::from(dir)),
            };
            let _ = events.send(event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notifier::RecordingNotifier;
    use crate::shell_exec::{stop_script, Stop};
    use serde_json::json;
    use std::time::Duration;

    fn gui() -> Arc<GuiState> {
        Arc::new(GuiState::new(Arc::new(RecordingNotifier::new())))
    }

    /// A disposable directory under `$TMPDIR/metafolder-tests/`, removed on
    /// drop.
    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn scratch(name: &str) -> Scratch {
        let dir = std::env::temp_dir()
            .join("metafolder-tests")
            .join(format!("mf-session-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        Scratch(dir)
    }

    /// An rc file guarded the way a distribution's `.bashrc` is: it loads only
    /// in an interactive shell.
    fn rc_file(dir: &Path, body: &str) -> PathBuf {
        let rc = dir.join("bashrc");
        std::fs::write(&rc, format!("[[ $- != *i* ]] && return\n{body}\n")).unwrap();
        rc
    }

    async fn run(gui: &Arc<GuiState>, line: &str, rc: Option<&Path>) {
        gui.shells().run(gui, "ws-1", line, None, None, rc.map(Path::to_path_buf)).await.unwrap();
    }

    /// Each run's lines after its command, as (kind, text).
    fn runs(gui: &GuiState) -> Vec<Vec<(ShellLine, String)>> {
        let mut runs: Vec<(String, Vec<(ShellLine, String)>)> = Vec::new();
        for entry in gui.shell_log("ws-1").unwrap() {
            if entry.kind == ShellLine::Command {
                runs.push((entry.run.clone(), Vec::new()));
            } else {
                let run = runs.iter_mut().find(|(id, _)| *id == entry.run).unwrap();
                run.1.push((entry.kind, entry.text));
            }
        }
        runs.into_iter().map(|(_, lines)| lines).collect()
    }

    fn out(text: &str) -> (ShellLine, String) {
        (ShellLine::Stdout, text.to_string())
    }

    /// A variable, a function, an alias and a `cd` of one line are there for
    /// the next: the lines run in one shell.
    #[tokio::test]
    async fn test_the_shell_keeps_its_state_from_line_to_line() {
        let dir = scratch("state");
        let gui = gui();
        let sub = dir.0.join("sub");
        let first = format!("X=1; f() {{ echo func; }}; alias k='echo kk'; cd '{}'", sub.display());
        run(&gui, &first, None).await;
        run(&gui, "echo $X; f; k; pwd", None).await;
        let sub = sub.to_string_lossy().into_owned();
        assert_eq!(runs(&gui), [vec![], vec![out("1"), out("func"), out("kk"), out(&sub)]]);
        assert_eq!(gui.shell_place("ws-1").unwrap().cd, Some(dir.0.join("sub")));
    }

    /// The rc file is loaded once, by the line that starts the shell, through
    /// the interactive guard; what it prints shows in that line's run.
    #[tokio::test]
    async fn test_the_rc_file_is_loaded_once() {
        let dir = scratch("rc");
        let count = dir.0.join("count");
        let rc = rc_file(
            &dir.0,
            &format!("echo x >> '{}'\nalias hi='echo aliased'\necho rc-says >&2", count.display()),
        );
        let gui = gui();
        run(&gui, "hi", Some(&rc)).await;
        run(&gui, "hi", Some(&rc)).await;
        assert_eq!(std::fs::read_to_string(&count).unwrap(), "x\n");
        assert_eq!(
            runs(&gui),
            [
                vec![(ShellLine::Stderr, "rc-says".to_string()), out("aliased")],
                vec![out("aliased")]
            ]
        );
    }

    /// A failing line says so, and the shell goes on; a missing rc file is
    /// skipped.
    #[tokio::test]
    async fn test_a_failing_line_keeps_the_shell() {
        let dir = scratch("fail");
        let gui = gui();
        run(&gui, "X=2; false", Some(&dir.0.join("nowhere"))).await;
        run(&gui, "echo $X", None).await;
        assert_eq!(runs(&gui), [vec![(ShellLine::Status, "exit 1".to_string())], vec![out("2")]]);
    }

    /// `exit` ends the shell, as it closes a terminal: the next line starts a
    /// new one, where the previous left off.
    #[tokio::test]
    async fn test_exit_ends_the_shell_and_the_next_line_starts_another() {
        let dir = scratch("exit");
        let gui = gui();
        run(&gui, &format!("X=3; cd '{}'; exit 3", dir.0.display()), None).await;
        run(&gui, "echo \"[$X]\"; pwd", None).await;
        let here = dir.0.to_string_lossy().into_owned();
        let runs = runs(&gui);
        assert_eq!(runs[0].last(), Some(&(ShellLine::Status, "exit 3".to_string())));
        assert_eq!(runs[1], [out("[]"), out(&here)]);
    }

    /// What is typed while a line runs is its input, echoed in its run; the
    /// end of the input is an end of file.
    #[tokio::test]
    async fn test_typed_input_reaches_the_running_line() {
        let gui = gui();
        let running = tokio::spawn({
            let gui = gui.clone();
            async move { run(&gui, "read a; echo got $a; cat", None).await }
        });
        let shells = gui.shells();
        for _ in 0..200 {
            if shells.running("ws-1").is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(shells.input(&gui, "ws-1", Some("yes")).unwrap());
        assert!(shells.input(&gui, "ws-1", Some("more")).unwrap());
        assert!(shells.input(&gui, "ws-1", None).unwrap());
        tokio::time::timeout(Duration::from_secs(5), running).await.unwrap().unwrap();
        let input = |text: &str| (ShellLine::Input, text.to_string());
        let lines = runs(&gui).remove(0);
        assert_eq!(lines.iter().filter(|l| l.0 == ShellLine::Input).count(), 2);
        assert!(lines.contains(&input("yes")) && lines.contains(&input("more")));
        let printed: Vec<_> = lines.into_iter().filter(|l| l.0 == ShellLine::Stdout).collect();
        assert_eq!(printed, [out("got yes"), out("more")]);
        assert!(!shells.input(&gui, "ws-1", Some("late")).unwrap(), "no line runs any more");
    }

    /// Output without a final newline is still a line of its own.
    #[tokio::test]
    async fn test_output_without_a_final_newline() {
        let gui = gui();
        run(&gui, "printf abc", None).await;
        run(&gui, "printf def >&2", None).await;
        assert_eq!(runs(&gui), [vec![out("abc")], vec![(ShellLine::Stderr, "def".to_string())]]);
    }

    /// A line typed while another runs waits for it, as in a terminal.
    #[tokio::test]
    async fn test_lines_run_in_turn() {
        let gui = gui();
        let first = tokio::spawn({
            let gui = gui.clone();
            async move { run(&gui, "sleep 0.3; echo one", None).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        run(&gui, "echo two", None).await;
        first.await.unwrap();
        assert_eq!(runs(&gui), [vec![out("one")], vec![out("two")]]);
    }

    async fn started(gui: &Arc<GuiState>, line: &str) -> (String, tokio::task::JoinHandle<()>) {
        let running = tokio::spawn({
            let gui = gui.clone();
            let line = line.to_string();
            async move { run(&gui, &line, None).await }
        });
        for _ in 0..200 {
            if let Some(id) = gui.shells().running("ws-1") {
                tokio::time::sleep(Duration::from_millis(200)).await;
                return (id, running);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the line should be running");
    }

    /// Stopping a line interrupts it, as `ctrl+c` does: the shell stays.
    #[tokio::test]
    async fn test_stopping_a_line_keeps_the_shell() {
        let gui = gui();
        run(&gui, "X=4", None).await;
        let (task, running) = started(&gui, "sleep 30").await;
        assert!(stop_script(&gui, &task, Stop::Terminate));
        tokio::time::timeout(Duration::from_secs(5), running).await.unwrap().unwrap();
        run(&gui, "echo $X", None).await;
        let runs = runs(&gui);
        assert_eq!(runs[1].last(), Some(&(ShellLine::Status, "stopped".to_string())));
        assert_eq!(runs[2], [out("4")]);
    }

    /// The shell's lines are interrupted even when the GUI was started with
    /// `SIGINT` ignored (a background job of a non-interactive shell): an
    /// ignored signal is inherited, and bash cannot reset it for its children.
    #[tokio::test]
    async fn test_a_line_is_interrupted_even_if_the_gui_ignores_sigint() {
        // SAFETY: plain syscalls; the disposition is restored below. Only the
        // shell started meanwhile inherits it.
        let before = unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };
        let gui = gui();
        run(&gui, "true", None).await;
        unsafe { libc::signal(libc::SIGINT, before) };
        let (task, running) = started(&gui, "sleep 30").await;
        assert!(stop_script(&gui, &task, Stop::Terminate));
        let stopped = tokio::time::timeout(Duration::from_secs(5), running).await;
        assert!(stopped.is_ok(), "the interrupted line must end");
    }

    /// Killing a line kills its shell; the next line starts another.
    #[tokio::test]
    async fn test_killing_a_line_ends_the_shell() {
        let gui = gui();
        run(&gui, "X=5", None).await;
        let (task, running) = started(&gui, "trap '' INT; sleep 30").await;
        assert!(stop_script(&gui, &task, Stop::Kill));
        tokio::time::timeout(Duration::from_secs(5), running).await.unwrap().unwrap();
        run(&gui, "echo \"[$X]\"", None).await;
        let runs = runs(&gui);
        assert_eq!(runs[1].last(), Some(&(ShellLine::Status, "killed".to_string())));
        assert_eq!(runs[2], [out("[]")]);
    }

    /// A new selection moves the shell there before the next line; a line's
    /// own `cd` holds while the selection stays.
    #[tokio::test]
    async fn test_the_selection_moves_the_shell() {
        let dir = scratch("select");
        let gui = gui();
        gui.set_var("ws-1", "selected_paths", json!([dir.0.join("sub")])).unwrap();
        run(&gui, "pwd; cd ..", None).await;
        run(&gui, "pwd", None).await;
        gui.set_var("ws-1", "selected_paths", json!([dir.0.join("sub")])).unwrap();
        gui.set_var("ws-1", "selected_paths", json!([dir.0.clone()])).unwrap();
        run(&gui, "pwd", None).await;
        let (sub, here) = (
            dir.0.join("sub").to_string_lossy().into_owned(),
            dir.0.to_string_lossy().into_owned(),
        );
        assert_eq!(runs(&gui), [vec![out(&sub)], vec![out(&here)], vec![out(&here)]]);
    }

    /// Each line is passed its own context, and none when it has none.
    #[tokio::test]
    async fn test_each_line_is_given_its_context() {
        let gui = gui();
        let context = metafolder_core::context::Context { repo: uuid::Uuid::nil(), label: 7 };
        let line = "echo \"in:${METAFOLDER_LABEL:-none}\"";
        gui.shells().run(&gui, "ws-1", line, Some(context), None, None).await.unwrap();
        gui.shells().run(&gui, "ws-1", line, None, None, None).await.unwrap();
        assert_eq!(runs(&gui), [vec![out(&format!("in:{context}"))], vec![out("in:none")]]);
    }

    /// Closing the workspace ends its shell.
    #[tokio::test]
    async fn test_closing_the_workspace_ends_its_shell() {
        let gui = gui();
        run(&gui, "true", None).await;
        let pid = gui.shells().live("ws-1").unwrap().pid;
        gui.shells().close("ws-1");
        for _ in 0..200 {
            // SAFETY: signal 0 only checks that the process exists.
            if unsafe { libc::kill(pid as i32, 0) } != 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the shell should have ended");
    }
}
