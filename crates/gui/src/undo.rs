//! `log:undo` / `log:redo` (doc "Revision commands in the GUI", doc "Undo
//! and redo by label"): undo a label of the newest change the *user* made to
//! the active repository, and take a label of the newest undo back.
//!
//! The selection is [`metafolder_core::undo`]'s, shared with `mf log
//! undo`/`redo`: the labels the newest change still standing (the newest undo)
//! was written in, innermost first. The shell asks which when there are several
//! ([`log_candidates`]); this module carries the revert out and reports it —
//! the watcher's operations are left where they are.
//!
//! Both carry the files with them: a revert that moves a file, or brings one
//! back from the trash-bin, or sends one back there, is performed by the shared
//! coordinated revert ([`metafolder_core::navigation`]), with the GUI's
//! no-question policies ([`GUI_POLICIES`]).

use crate::blocking_client::BlockingClient;
use crate::daemon_proxy::DaemonProxy;
use crate::state::GuiState;
use metafolder_core::navigation::{
    self, MovePolicies, NavError, NavigationUi, Policy, Repo, RevertRequest,
};
use metafolder_core::undo::{self, Selection};
use serde_json::{json, Value};
use std::sync::Arc;

/// What undo (redo) offers — two bounded reads rather than one unbounded one
/// (doc "Undo and redo").
async fn selection(
    daemon: &DaemonProxy,
    repo: &str,
    redo: bool,
) -> Result<Option<Selection>, String> {
    for limit in [undo::WINDOW, undo::WIDE_WINDOW] {
        let path = format!("/repos/{repo}/log?limit={limit}");
        let log = daemon.request("GET", &path, None).await?.body;
        let found = if redo { undo::redo_from_log(&log) } else { undo::undo_from_log(&log) };
        if found.is_some() || !undo::window_exhausted(&log, limit) {
            return Ok(found);
        }
    }
    Ok(None)
}

/// Undoes (redoes) `label`, or — none named — the one label the newest change
/// (undo) was written in. When it was written in several, nothing is done and
/// the status says so: the shell asks which before calling this
/// ([`log_candidates`]).
pub async fn navigate(
    gui: Arc<GuiState>,
    daemon: Arc<DaemonProxy>,
    ws_id: String,
    redo: bool,
    label: Option<i64>,
    timeouts: crate::commands::StatusTimeouts,
) -> Result<(), String> {
    let repo = gui.active_repo(&ws_id)?;
    let verb = if redo { "redo" } else { "undo" };
    let label = match label {
        Some(label) => label,
        None => match selection(&daemon, &repo, redo).await? {
            None => {
                let text = format!("Nothing to {verb}.");
                gui.post_status(&ws_id, &text, "info", Some(timeouts.message_ms))?;
                return Ok(());
            }
            Some(selection) if selection.candidates.len() == 1 => selection.candidates[0].label_id,
            Some(selection) => {
                let text = format!(
                    "Several labels to {verb}: {} — name one (log:{verb} <label>).",
                    selection.describe().join(" · ")
                );
                gui.post_status(&ws_id, &text, "info", Some(timeouts.message_ms))?;
                return Ok(());
            }
        },
    };
    revert(&gui, &daemon, &ws_id, &repo, label, redo, &timeouts).await
}

/// The GUI's move policies. It asks nothing while the lock is held — a window
/// left open would freeze the repository's writes — so a move applies whether
/// or not the file is where the log says: when it is not, the metadata follows
/// the navigation and keeps the recorded path (doc "Reverting a revision", the CLI's own default when the
/// file is there).
pub const GUI_POLICIES: MovePolicies =
    MovePolicies { on_available: Policy::Apply, on_unavailable: Policy::Apply };

/// The GUI's side of a navigation: it never asks, and keeps the notes for the
/// status bar.
#[derive(Default)]
struct GuiUi {
    notes: std::sync::Mutex<Vec<String>>,
}

impl NavigationUi for GuiUi {
    fn ask_move(&self, _from: &str, _to: &str, _available: bool) -> Result<Policy, NavError> {
        Err(NavError("the GUI's move policies never ask".into()))
    }
    fn note(&self, message: &str) {
        self.notes.lock().unwrap_or_else(|p| p.into_inner()).push(message.to_string());
    }
}

/// Runs `work` against the repository on a blocking thread — the shared
/// navigation (`core::navigation`) is synchronous, like the trash-bin's — and
/// returns its result with the notes it left.
pub(crate) async fn blocking<T: Send + 'static>(
    daemon: &DaemonProxy,
    repo: &str,
    work: impl FnOnce(&Repo<'_>, &dyn NavigationUi) -> Result<T, NavError> + Send + 'static,
) -> Result<(T, Vec<String>), String> {
    let base = daemon.base_url();
    let repo = repo.to_string();
    tokio::task::spawn_blocking(move || {
        let client = BlockingClient::new(base);
        let repo = Repo::open(&client, &repo).map_err(|e| e.0)?;
        let ui = GuiUi::default();
        let out = work(&repo, &ui).map_err(|e| e.0)?;
        Ok((out, ui.notes.into_inner().unwrap_or_else(|p| p.into_inner())))
    })
    .await
    .map_err(|e| format!("the navigation task panicked: {e}"))?
}

/// A summary, followed by what the file actions had to say.
fn with_notes(summary: String, notes: &[String]) -> String {
    if notes.is_empty() {
        summary
    } else {
        format!("{summary} {}", notes.join(" · "))
    }
}

/// The revert half of an undo: write the inverse at HEAD onto the current
/// state, files included (`core::navigation::revert`). Nothing blocks it: a
/// later write on the same cell is overwritten (doc "Reverting onto the
/// current state"), which undo — taking the newest change — almost never meets.
async fn revert(
    gui: &Arc<GuiState>,
    daemon: &Arc<DaemonProxy>,
    ws_id: &str,
    repo: &str,
    label_id: i64,
    redo: bool,
    timeouts: &crate::commands::StatusTimeouts,
) -> Result<(), String> {
    let target = if redo { undo::redo_target(label_id) } else { undo::undo_target(label_id) };
    let result = blocking(daemon, repo, move |repo, ui| {
        let plan = navigation::revert_plan(repo, &target)?;
        let request = RevertRequest { target: &target, metadata_only: false };
        navigation::revert(repo, &request, &plan, &GUI_POLICIES, ui)
    })
    .await;
    let (response, notes) = match result {
        Ok(done) => done,
        Err(message) => return report(gui, ws_id, &message, timeouts),
    };
    let count = response["reverted_operations"].as_array().map(|a| a.len()).unwrap_or(0);
    let summary = match response["label"].as_i64() {
        None => "Nothing was reverted.".to_string(),
        Some(_) => format!(
            "{} label {label_id} ({count} operations) — the daemon's own operations were \
             left in place.",
            if redo { "Redid" } else { "Undid" }
        ),
    };
    finish(gui, ws_id, &with_notes(summary, &notes), timeouts)
}

/// Says why nothing happened, without failing the command: the user is being
/// told what to do next, not handed an error.
fn report(
    gui: &Arc<GuiState>,
    ws_id: &str,
    message: &str,
    timeouts: &crate::commands::StatusTimeouts,
) -> Result<(), String> {
    gui.post_status(ws_id, message, "error", Some(timeouts.error_ms))
}

/// Reports what was done and asks the panels to refresh.
fn finish(
    gui: &Arc<GuiState>,
    ws_id: &str,
    summary: &str,
    timeouts: &crate::commands::StatusTimeouts,
) -> Result<(), String> {
    gui.post_status(ws_id, summary, "info", Some(timeouts.message_ms))?;
    gui.mark_metarecords_dirty(ws_id)?;
    Ok(())
}

#[tauri::command]
pub async fn log_navigate(
    app: tauri::State<'_, Arc<crate::commands::App>>,
    ws_id: String,
    redo: bool,
    label: Option<i64>,
) -> Result<(), String> {
    navigate(app.gui.clone(), app.daemon.clone(), ws_id, redo, label, app.status_timeouts()).await
}

/// The labels `log:undo` (`log:redo`) would take back, innermost first:
/// `[{"id", "name"}]`, empty when there is nothing to undo.
#[tauri::command]
pub async fn log_candidates(
    app: tauri::State<'_, Arc<crate::commands::App>>,
    ws_id: String,
    redo: bool,
) -> Result<Vec<Value>, String> {
    let repo = app.gui.active_repo(&ws_id)?;
    Ok(selection(&app.daemon, &repo, redo)
        .await?
        .map(|s| s.candidates)
        .unwrap_or_default()
        .into_iter()
        .map(|c| json!({"id": c.label_id, "name": c.name}))
        .collect())
}

/// The log panel's `log:revert`: writes the inverse of `target` at HEAD, files
/// included (`core::navigation::revert`). Answers the daemon's revert response
/// (`label`, `reverted_operations`, `skipped_operations`) with the `notes`
/// the file actions left. The panel reads the plan and confirms an overwrite
/// of a cell changed since before it gets here.
#[tauri::command]
pub async fn log_revert(
    app: tauri::State<'_, Arc<crate::commands::App>>,
    repo: String,
    target: Value,
) -> Result<Value, String> {
    let (mut response, notes) = blocking(&app.daemon, &repo, move |repo, ui| {
        let plan = navigation::revert_plan(repo, &target)?;
        let request = RevertRequest { target: &target, metadata_only: false };
        navigation::revert(repo, &request, &plan, &GUI_POLICIES, ui)
    })
    .await?;
    response["notes"] = json!(notes);
    Ok(response)
}
