//! The GUI's work context (doc "The GUI's work context"): a label the user
//! opens on a repository (`context:set`), which every later request of the
//! GUI's to that repository writes in (`X-Metafolder-Label`, doc "Labels and
//! contexts") until it is closed (`context:unset`) — naming the work under way,
//! so it can be looked at, or undone, whole.
//!
//! One per repository, at most: a label id means nothing in another
//! repository, and a context is the repository's, not a workspace's. It is
//! remembered in the repository's `.metafolder/gui/context`, so a GUI started
//! again finds it and asks whether to stay in it ([`pending`], [`resume`]).
//! The watcher's operations never carry it: they are written by the daemon,
//! not sent by the GUI.
//!
//! The contexts in force are the process's, like the command under way
//! ([`crate::daemon_proxy`]): one GUI, one set of contexts, read by both the
//! async proxy and the blocking client.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{json, Value};
use uuid::Uuid;

use crate::daemon_proxy::DaemonProxy;
use metafolder_core::sync::MutexExt;

/// A work context: the label its writes go in, and the name it was given.
/// The file's content, as JSON.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkContext {
    pub label: i64,
    pub name: String,
    /// What the work is, when the user said (the label's description).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// What a repository being opened has to offer back ([`pending`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pending {
    /// Nothing: no context remembered, or the one remembered is already in force.
    None,
    /// A context left open, to stay in or to end.
    Open(WorkContext),
    /// A context left open whose label is gone — pruned since. It has been
    /// forgotten; the user is only told.
    Gone(WorkContext),
}

/// A context in force: what it is, and the `.metafolder/` it is remembered in.
struct Active {
    context: WorkContext,
    metafolder: PathBuf,
}

static ACTIVE: Mutex<Option<HashMap<Uuid, Active>>> = Mutex::new(None);

fn with_active<T>(f: impl FnOnce(&mut HashMap<Uuid, Active>) -> T) -> T {
    f(ACTIVE.lock_recover().get_or_insert_with(HashMap::new))
}

/// Who is told that a context was put in force or closed: the GUI, which
/// re-sends the workspaces so the slot headers' indicator follows.
type Listener = Box<dyn Fn() + Send + Sync>;
static LISTENER: Mutex<Option<Listener>> = Mutex::new(None);

/// Calls `listener` after every change of the contexts in force (one listener:
/// one GUI; a second replaces the first).
pub fn on_change(listener: impl Fn() + Send + Sync + 'static) {
    *LISTENER.lock_recover() = Some(Box::new(listener));
}

/// [`with_active`] for a change: the listener is told once the contexts are
/// unlocked, since it reads them back.
fn change<T>(f: impl FnOnce(&mut HashMap<Uuid, Active>) -> T) -> T {
    let out = with_active(f);
    if let Some(listener) = LISTENER.lock_recover().as_ref() {
        listener();
    }
    out
}

fn repo_key(repo: &str) -> Result<Uuid, String> {
    Uuid::parse_str(repo).map_err(|_| format!("not a repository uuid: {repo}"))
}

/// The repository a daemon path is one of (`/repos/<uuid>/…`).
fn repo_of_path(path: &str) -> Option<Uuid> {
    let rest = path.strip_prefix("/repos/")?;
    Uuid::parse_str(rest.split(['/', '?']).next()?).ok()
}

/// The context in force on `repo`.
pub fn active(repo: &str) -> Option<WorkContext> {
    let key = Uuid::parse_str(repo).ok()?;
    with_active(|map| map.get(&key).map(|a| a.context.clone()))
}

/// The context in force on `repo`, as it is passed on to a process
/// (`METAFOLDER_LABEL`).
pub fn context_of(repo: &str) -> Option<metafolder_core::context::Context> {
    let key = Uuid::parse_str(repo).ok()?;
    with_active(|map| {
        map.get(&key)
            .map(|a| metafolder_core::context::Context { repo: key, label: a.context.label })
    })
}

/// The label a request to `path` is sent in: the context in force on its
/// repository.
pub(crate) fn label_for(path: &str) -> Option<i64> {
    let key = repo_of_path(path)?;
    with_active(|map| map.get(&key).map(|a| a.context.label))
}

/// After a request to `path` answered `status` with `body`: when the daemon
/// refused it because the context's label no longer exists (pruned while the
/// context was in force — doc "Log endpoints": nothing was written), the
/// context is closed and forgotten, and `true` says to send the request again,
/// outside it.
pub(crate) fn forget_if_gone(path: &str, status: u16, body: &Value) -> bool {
    if status != 400 {
        return false;
    }
    let Some(key) = repo_of_path(path) else { return false };
    let removed = change(|map| {
        let gone = map.get(&key).is_some_and(|a| {
            body["error"].as_str() == Some(&format!("no label {}", a.context.label))
        });
        if gone {
            map.remove(&key)
        } else {
            None
        }
    });
    match removed {
        Some(active) => {
            eprintln!(
                "work context \"{}\" (label {}) no longer exists: closed",
                active.context.name, active.context.label
            );
            let _ = remove_file(&active.metafolder);
            true
        }
        None => false,
    }
}

/// Puts `context` in force on `repo`, remembered in `metafolder`.
pub(crate) fn activate(repo: &str, context: WorkContext, metafolder: PathBuf) {
    if let Ok(key) = Uuid::parse_str(repo) {
        change(|map| map.insert(key, Active { context, metafolder }));
    }
}

/// Forgets the context in force on `repo` without touching its file — what a
/// GUI that exits does. Tests use it to stand for a restart.
pub fn forget(repo: &str) {
    if let Ok(key) = Uuid::parse_str(repo) {
        change(|map| map.remove(&key));
    }
}

/// Opens a context named `name` on `repo` and puts it in force, closing the
/// one in force there if any: one at a time. The new label is opened at the
/// root, not inside the one it replaces.
pub async fn set(
    daemon: &DaemonProxy,
    repo: &str,
    name: &str,
    description: Option<&str>,
) -> Result<WorkContext, String> {
    let key = repo_key(repo)?;
    let metafolder = crate::history::metafolder_dir_of(daemon, repo).await?;
    let previous = change(|map| map.remove(&key));
    let opened = async {
        let body = json!({"name": name, "description": description});
        let response = daemon.request("POST", &format!("/repos/{repo}/labels"), Some(body)).await?;
        if response.status >= 300 {
            return Err(crate::daemon_proxy::error_message(&response.body, || {
                format!("opening the label failed with status {}", response.status)
            }));
        }
        let label = response.body["id"].as_i64().ok_or("the daemon answered no label id")?;
        let context = WorkContext {
            label,
            name: name.to_string(),
            description: description.map(str::to_string),
        };
        write_file(&metafolder, &context)?;
        Ok(context)
    }
    .await;
    match opened {
        Ok(context) => {
            change(|map| {
                map.insert(key, Active { context: context.clone(), metafolder });
            });
            Ok(context)
        }
        Err(error) => {
            // Nothing replaced it: the previous context stays in force.
            if let Some(previous) = previous {
                change(|map| map.insert(key, previous));
            }
            Err(error)
        }
    }
}

/// Closes the context on `repo` — the one in force, or the one remembered and
/// not resumed — and forgets its file. The context closed, if there was one.
pub async fn unset(daemon: &DaemonProxy, repo: &str) -> Result<Option<WorkContext>, String> {
    let key = repo_key(repo)?;
    if let Some(active) = change(|map| map.remove(&key)) {
        remove_file(&active.metafolder)?;
        return Ok(Some(active.context));
    }
    let metafolder = crate::history::metafolder_dir_of(daemon, repo).await?;
    let remembered = read_file(&metafolder).ok().flatten();
    remove_file(&metafolder)?;
    Ok(remembered)
}

/// What `repo`, being opened, has to offer back: the context remembered in its
/// file, when it is not in force already — checked against the daemon, since
/// the label may have been pruned since; a gone one is forgotten.
pub async fn pending(daemon: &DaemonProxy, repo: &str) -> Result<Pending, String> {
    if active(repo).is_some() {
        return Ok(Pending::None);
    }
    let metafolder = crate::history::metafolder_dir_of(daemon, repo).await?;
    let Some(context) = read_file(&metafolder)? else { return Ok(Pending::None) };
    let path = format!("/repos/{repo}/labels/{}", context.label);
    let response = daemon.request("GET", &path, None).await?;
    match response.status {
        200..=299 => Ok(Pending::Open(context)),
        404 => {
            remove_file(&metafolder)?;
            Ok(Pending::Gone(context))
        }
        status => Err(crate::daemon_proxy::error_message(&response.body, || {
            format!("reading label {} failed with status {status}", context.label)
        })),
    }
}

/// Puts the context remembered on `repo` back in force — the answer "stay" to
/// [`pending`]. The context resumed; `None` when none is remembered.
pub async fn resume(daemon: &DaemonProxy, repo: &str) -> Result<Option<WorkContext>, String> {
    repo_key(repo)?;
    let metafolder = crate::history::metafolder_dir_of(daemon, repo).await?;
    let Some(context) = read_file(&metafolder)? else { return Ok(None) };
    activate(repo, context.clone(), metafolder);
    Ok(Some(context))
}

const FILE: &str = "gui/context";

/// Remembers `context` in `<metafolder>/gui/context`, atomically (a temporary
/// file renamed over it).
pub fn write_file(metafolder: &Path, context: &WorkContext) -> Result<(), String> {
    let file = metafolder.join(FILE);
    let dir = file.parent().expect("the context file has a directory");
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let text = serde_json::to_string(context).map_err(|e| e.to_string())?;
    let tmp = file.with_extension("tmp");
    std::fs::write(&tmp, text + "\n")
        .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &file).map_err(|e| format!("cannot replace {}: {e}", file.display()))
}

/// The context remembered in `<metafolder>/gui/context`; `None` without one. A
/// file that does not hold one is an error, not a context.
pub fn read_file(metafolder: &Path) -> Result<Option<WorkContext>, String> {
    let file = metafolder.join(FILE);
    match std::fs::read_to_string(&file) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| format!("{} does not hold a work context: {e}", file.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read {}: {e}", file.display())),
    }
}

fn remove_file(metafolder: &Path) -> Result<(), String> {
    let file = metafolder.join(FILE);
    match std::fs::remove_file(&file) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("cannot remove {}: {e}", file.display())),
    }
}

// ── Tauri commands (the shell's `context:set`, `context:unset`, and the
// question asked when a repository is opened) ────────────────────────────

/// `context:set <name>` on the workspace's repository.
#[tauri::command]
pub async fn context_set(
    app: tauri::State<'_, std::sync::Arc<crate::commands::App>>,
    ws_id: String,
    name: String,
    description: Option<String>,
) -> Result<WorkContext, String> {
    let repo = app.gui.active_repo(&ws_id)?;
    set(&app.daemon, &repo, &name, description.as_deref()).await
}

/// `context:unset` on the workspace's repository: the context closed, if any.
#[tauri::command]
pub async fn context_unset(
    app: tauri::State<'_, std::sync::Arc<crate::commands::App>>,
    ws_id: String,
) -> Result<Option<WorkContext>, String> {
    let repo = app.gui.active_repo(&ws_id)?;
    unset(&app.daemon, &repo).await
}

/// The context in force on the workspace's repository.
#[tauri::command]
pub fn context_active(
    app: tauri::State<'_, std::sync::Arc<crate::commands::App>>,
    ws_id: String,
) -> Result<Option<WorkContext>, String> {
    Ok(active(&app.gui.active_repo(&ws_id)?))
}

/// What the workspace's repository, just opened, has to offer back:
/// `{"state": "none"}`, or `{"state": "open" | "gone", "label", "name"}`.
#[tauri::command]
pub async fn context_pending(
    app: tauri::State<'_, std::sync::Arc<crate::commands::App>>,
    ws_id: String,
) -> Result<Value, String> {
    let repo = app.gui.active_repo(&ws_id)?;
    Ok(match pending(&app.daemon, &repo).await? {
        Pending::None => json!({"state": "none"}),
        Pending::Open(c) => json!({"state": "open", "label": c.label, "name": c.name,
            "description": c.description}),
        Pending::Gone(c) => json!({"state": "gone", "label": c.label, "name": c.name}),
    })
}

/// Stays in the context remembered on the workspace's repository.
#[tauri::command]
pub async fn context_resume(
    app: tauri::State<'_, std::sync::Arc<crate::commands::App>>,
    ws_id: String,
) -> Result<Option<WorkContext>, String> {
    let repo = app.gui.active_repo(&ws_id)?;
    resume(&app.daemon, &repo).await
}
