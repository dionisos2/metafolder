//! `log:undo` / `log:redo` (spec-gui "Event log"): undo the last change the
//! *user* made to the active repository, and re-apply what a rollback undid.
//!
//! Undo is not "step HEAD back one revision": the watcher writes revisions of
//! its own, and rewinding HEAD past them would unrecord what the filesystem
//! really did. The choice — roll HEAD back, or write the inverse at HEAD — is
//! [`metafolder_core::undo`]'s, shared with `mf log undo`; this module only
//! carries it out and reports it.
//!
//! Both carry the files with them: a navigation that moves a file, or brings
//! one back from the trash-bin, or sends one back there, is performed by the
//! shared coordinated navigation ([`metafolder_core::navigation`]), with the
//! GUI's no-question policies ([`GUI_POLICIES`]).
//!
//! Redo is its mirror: it takes back the newest *undo*, by moving HEAD forward
//! onto what a rollback unapplied, by rolling back over the revert an undo
//! wrote, or — when the watcher has written since — by reverting that revert.
//! The choice is [`metafolder_core::undo::plan_redo`]'s, shared with
//! `mf log redo`; this module only carries it out and reports it.

use crate::blocking_client::BlockingClient;
use crate::daemon_proxy::DaemonProxy;
use crate::state::GuiState;
use metafolder_core::navigation::{
    self, MovePolicies, NavError, Navigated, NavigationUi, Policy, Repo, RevertRequest,
};
use metafolder_core::undo::{self, RedoPlan, UndoPlan};
use serde_json::{json, Value};
use std::sync::Arc;

/// Reads enough of the log to decide what undo should undo — two bounded reads
/// rather than one unbounded one (doc "Undo and redo").
async fn undo_plan(daemon: &DaemonProxy, repo: &str) -> Result<UndoPlan, String> {
    for limit in [undo::WINDOW, undo::WIDE_WINDOW] {
        let log = read_log(daemon, repo, "linear", limit).await?;
        let plan = undo::plan_from_log(&log);
        if plan != UndoPlan::Nothing || !undo::window_exhausted(&log, limit) {
            return Ok(plan);
        }
    }
    Ok(UndoPlan::Nothing)
}

/// The same for redo, over the *active* line: it carries HEAD's forward
/// continuation, which is what a redo re-applies when a rollback left one.
async fn redo_plan(daemon: &DaemonProxy, repo: &str) -> Result<RedoPlan, String> {
    for limit in [undo::WINDOW, undo::WIDE_WINDOW] {
        let log = read_log(daemon, repo, "active", limit).await?;
        let plan = undo::plan_redo_from_log(&log);
        if plan != RedoPlan::Nothing || !undo::window_exhausted(&log, limit) {
            return Ok(plan);
        }
    }
    Ok(RedoPlan::Nothing)
}

async fn read_log(
    daemon: &DaemonProxy,
    repo: &str,
    mode: &str,
    limit: usize,
) -> Result<Value, String> {
    let path = format!("/repos/{repo}/log?mode={mode}&limit={limit}");
    Ok(daemon.request("GET", &path, None).await?.body)
}

pub async fn navigate(
    gui: Arc<GuiState>,
    daemon: Arc<DaemonProxy>,
    ws_id: String,
    redo: bool,
    timeouts: crate::commands::StatusTimeouts,
) -> Result<(), String> {
    let repo = gui.active_repo(&ws_id)?;

    if !redo {
        return undo(gui, daemon, ws_id, repo, timeouts).await;
    }

    match redo_plan(&daemon, &repo).await? {
        RedoPlan::Nothing => {
            gui.post_status(&ws_id, "Nothing to redo.", "info", Some(timeouts.message_ms))?;
            Ok(())
        }
        RedoPlan::Forward { op_id, rev_id } => {
            let (done, notes) =
                rollback(&gui, &daemon, &ws_id, &repo, json!({ "id": op_id }), &timeouts).await?;
            let summary =
                format!("Redo: revision {rev_id} re-applied ({} operations).", done.processed);
            finish(&gui, &ws_id, &with_notes(summary, &notes), &timeouts)
        }
        RedoPlan::Rollback { rev_id } => {
            let (done, notes) =
                rollback(&gui, &daemon, &ws_id, &repo, json!({"prev_revision": true}), &timeouts)
                    .await?;
            let summary = format!(
                "Redo: the undo in revision {rev_id} rolled back ({} operations).",
                done.processed
            );
            finish(&gui, &ws_id, &with_notes(summary, &notes), &timeouts)
        }
        // The watcher has written since the undo, so HEAD cannot be rewound
        // over it: the undo is undone in place, exactly as undo itself does.
        RedoPlan::Revert { rev_id, ops } => {
            revert(&gui, &daemon, &ws_id, &repo, rev_id, ops, &timeouts).await
        }
    }
}

/// Undo: whichever of the two mechanisms the selection picked.
async fn undo(
    gui: Arc<GuiState>,
    daemon: Arc<DaemonProxy>,
    ws_id: String,
    repo: String,
    timeouts: crate::commands::StatusTimeouts,
) -> Result<(), String> {
    match undo_plan(&daemon, &repo).await? {
        UndoPlan::Nothing => {
            gui.post_status(&ws_id, "Nothing to undo.", "info", Some(timeouts.message_ms))?;
            Ok(())
        }
        UndoPlan::Rollback { rev_id } => {
            let (done, notes) =
                rollback(&gui, &daemon, &ws_id, &repo, json!({"prev_revision": true}), &timeouts)
                    .await?;
            let summary =
                format!("Undo: revision {rev_id} rolled back ({} operations).", done.processed);
            finish(&gui, &ws_id, &with_notes(summary, &notes), &timeouts)
        }
        UndoPlan::Revert { rev_id, ops } => {
            revert(&gui, &daemon, &ws_id, &repo, rev_id, ops, &timeouts).await
        }
    }
}

/// The GUI's move policies. It asks nothing while the lock is held — a window
/// left open would freeze the repository's writes — so a move applies whether
/// or not the file is where the log says: when it is not, the metadata follows
/// the navigation and keeps the recorded path (spec-event-log "Policies for
/// move_file", the CLI's own default when the file is there).
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

/// Navigates HEAD to `target`, files included (`core::navigation::rollback`),
/// reporting a failure on the status bar.
async fn rollback(
    gui: &Arc<GuiState>,
    daemon: &Arc<DaemonProxy>,
    ws_id: &str,
    repo: &str,
    target: Value,
    timeouts: &crate::commands::StatusTimeouts,
) -> Result<(Navigated, Vec<String>), String> {
    let result = blocking(daemon, repo, move |repo, ui| {
        navigation::rollback(repo, &target, &GUI_POLICIES, ui)
    })
    .await;
    if let Err(message) = &result {
        gui.post_status(ws_id, message, "error", Some(timeouts.error_ms))?;
    }
    result
}

/// What a revert came to: the daemon's answer, or the plan's refusal.
enum Reverted {
    Blocked(Value),
    Done(Value),
}

/// The revert half of an undo: check the plan, then write the inverse at HEAD,
/// files included (`core::navigation::revert`). A *blocked* revert is reported,
/// not forced — the log panel's `log:revert with-dependents` is the way
/// through, and pulling in someone else's changes is not something to do
/// behind the user's back.
async fn revert(
    gui: &Arc<GuiState>,
    daemon: &Arc<DaemonProxy>,
    ws_id: &str,
    repo: &str,
    rev_id: i64,
    ops: Option<Vec<i64>>,
    timeouts: &crate::commands::StatusTimeouts,
) -> Result<(), String> {
    let target = match &ops {
        Some(ops) => json!({ "op_ids": ops }),
        None => json!({ "rev_id": rev_id }),
    };
    let result = blocking(daemon, repo, move |repo, ui| {
        let plan = navigation::revert_plan(repo, &target, false)?;
        if plan["revertable"] == json!(false) {
            return Ok(Reverted::Blocked(plan));
        }
        let request = RevertRequest {
            target: &target,
            with_dependents: false,
            metadata_only: false,
            label: None,
        };
        navigation::revert(repo, &request, &plan, &GUI_POLICIES, ui).map(Reverted::Done)
    })
    .await;
    let (reverted, notes) = match result {
        Ok(done) => done,
        Err(message) => return report(gui, ws_id, &message, timeouts),
    };
    match reverted {
        Reverted::Blocked(plan) => {
            let blocker = plan["blocked"].as_array().and_then(|b| b.first()).cloned();
            let where_ = blocker
                .as_ref()
                .and_then(|b| b["rev_id"].as_i64())
                .map(|r| format!(" by revision {r}"))
                .unwrap_or_default();
            report(
                gui,
                ws_id,
                &format!(
                    "Cannot take revision {rev_id} back: a later change{where_} overwrote what it \
                     wrote. Open the log panel and use log:revert with-dependents to undo both."
                ),
                timeouts,
            )
        }
        Reverted::Done(response) => {
            let count = response["reverted_operations"].as_array().map(|a| a.len()).unwrap_or(0);
            let summary = match response["revision"].as_i64() {
                None => "Nothing was reverted.".to_string(),
                Some(new_rev) => format!(
                    "Revision {rev_id} reverted as revision {new_rev} ({count} operations) — \
                     the daemon's own revisions were left in place."
                ),
            };
            finish(gui, ws_id, &with_notes(summary, &notes), timeouts)
        }
    }
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
) -> Result<(), String> {
    navigate(app.gui.clone(), app.daemon.clone(), ws_id, redo, app.status_timeouts()).await
}

/// What `log_rollback` answers the log panel.
#[derive(serde::Serialize)]
pub struct RollbackResult {
    pub total: usize,
    pub processed: usize,
    /// What the file actions had to say (a file moved, one brought back…).
    pub notes: Vec<String>,
}

/// The log panel's `log:rollback`: navigates HEAD to `target`, files included
/// (`core::navigation::rollback`). The panel confirms first, so no dialog is
/// open while the lock is held.
#[tauri::command]
pub async fn log_rollback(
    app: tauri::State<'_, Arc<crate::commands::App>>,
    repo: String,
    target: Value,
) -> Result<RollbackResult, String> {
    let (done, notes) = blocking(&app.daemon, &repo, move |repo, ui| {
        navigation::rollback(repo, &target, &GUI_POLICIES, ui)
    })
    .await?;
    Ok(RollbackResult { total: done.total, processed: done.processed, notes })
}

/// The log panel's `log:revert`: writes the inverse of `target` at HEAD, files
/// included (`core::navigation::revert`). Answers the daemon's revert response
/// (`revision`, `reverted_operations`, `skipped_operations`) with the `notes`
/// the file actions left. A blocked plan is refused: the panel reads the plan
/// and offers `with dependents` before it gets here.
#[tauri::command]
pub async fn log_revert(
    app: tauri::State<'_, Arc<crate::commands::App>>,
    repo: String,
    target: Value,
    with_dependents: bool,
) -> Result<Value, String> {
    let (mut response, notes) = blocking(&app.daemon, &repo, move |repo, ui| {
        let plan = navigation::revert_plan(repo, &target, with_dependents)?;
        if plan["revertable"] == json!(false) {
            return Err(NavError(
                "the revert is blocked by later changes; revert with dependents".into(),
            ));
        }
        let request =
            RevertRequest { target: &target, with_dependents, metadata_only: false, label: None };
        navigation::revert(repo, &request, &plan, &GUI_POLICIES, ui)
    })
    .await?;
    response["notes"] = json!(notes);
    Ok(response)
}
