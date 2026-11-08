//! `GuiState`: single source of truth for workspaces, layout, variables and
//! message logs, shared by the Tauri commands and the GUI HTTP API
//! (doc "GUI"). Every mutation pushes an event to the frontend
//! through the [`FrontendNotifier`].

mod global;
pub mod layout;
mod picker;
pub mod saved;
mod slot_commands;
mod status;
mod txn;
pub mod workspace;
mod workspace_commands;

use crate::events;
use crate::notifier::FrontendNotifier;
use layout::{LayoutView, Slot, SlotId, SlotPayload};
use metafolder_core::sync::MutexExt;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use workspace::{
    MessageEntry, ShellEntry, ShellLine, ShellPlace, StatusEntry, Workspace, WorkspaceInfo,
};

/// Default panel type shown when a workspace is first displayed: `repos`
/// when no repository is active (entry point), `metarecord-list` otherwise.
/// (Decision; the spec leaves the initial panel type unspecified.)
fn default_panel_type(active_repo: Option<&str>) -> &'static str {
    match active_repo {
        Some(_) => "metarecord-list",
        None => "repos",
    }
}

/// One running shell script, as surfaced in the task bar (doc
/// "Script sessions"). `done`/`total` drive a determinate progress bar when the
/// script reports them (via `mf gui progress`); absent ⇒ a spinner.
#[derive(Clone, Default)]
struct ScriptTask {
    ws_id: String,
    label: String,
    phase: Option<String>,
    done: Option<u64>,
    total: Option<u64>,
    /// The workspaces this script owns: the one it was launched from, plus
    /// every workspace it created through the GUI API. A script's question and
    /// task entry are shown only while one of them is on screen (doc
    /// "Script sessions"), so switching tab puts them away.
    workspaces: Vec<String>,
    /// True while the script blocks on an input/prompt wait: it is *not*
    /// working, so the entry stops spinning and says the answer is awaited.
    waiting: bool,
    /// The run's process-group leader, when the GUI launched it: what
    /// `script:stop` signals to end the script and every child it spawned.
    /// `None` for a run registered without one (tests, a foreign launcher).
    pid: Option<u32>,
    /// The workspaces the script *opened* (a subset of `workspaces`, which
    /// also holds the one it was launched from): what the GUI closes when the
    /// run is stopped.
    opened: Vec<String>,
    /// The visible workspace of the left and of the right slot when the run
    /// began — what a slot is given back when the run is stopped.
    launch_layout: (Option<String>, Option<String>),
    /// Set by `script:stop` ("stopped") and `script:kill` ("killed"): the user
    /// ended the run, so it is reported under that word rather than as a
    /// failure, and the GUI cleans up after it (doc "Script sessions").
    ended_by: Option<&'static str>,
}

pub struct GuiState {
    inner: Mutex<Inner>,
    /// Shell scripts currently running, keyed by a per-run id (the value of
    /// `METAFOLDER_GUI_TASK` injected into the subprocess), so a script can
    /// address its own progress regardless of which workspace is focused.
    /// Kept out of `Inner` so the workspace/message lock is never held across
    /// the notify that broadcasts the running set.
    scripts: Mutex<HashMap<String, ScriptTask>>,
    /// Whether a script's awaited keys currently reach the script (doc
    /// "Script sessions"). The question bar's checkbox flips it; it holds for the
    /// rest of the session, but every new run starts with its keys live.
    script_keys: AtomicBool,
    /// The questions scripts are asking right now — at most one per workspace
    /// (doc "Script sessions") — kept so they can be re-broadcast when the
    /// checkbox flips. Empty when no input wait is up.
    questions: Mutex<Vec<Question>>,
    /// The open command transactions, by id (doc "Command transactions").
    /// Kept out of `Inner`, and never locked together with it.
    txns: Mutex<HashMap<u64, txn::Txn>>,
    next_txn: AtomicU64,
    /// The workspaces' persistent shells (doc "shell panel").
    shells: crate::shell_session::ShellSessions,
    /// What every script and shell the GUI starts is told of where it runs
    /// ([`GuiState::set_script_env`]); empty until set.
    script_env: std::sync::OnceLock<Vec<(&'static str, String)>>,
    notifier: Arc<dyn FrontendNotifier>,
}

/// A live `POST /gui/input` question: what the frontend shows in the question
/// bar while its workspace is on screen.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct Question {
    /// The wait's id, which `answer:send` resolves.
    pub id: u64,
    pub keys: Vec<String>,
    pub prompt: Option<String>,
    /// The workspace the question is asked in; `None` = owned by nobody,
    /// always shown.
    pub workspace: Option<String>,
    /// The asking script's run id, so escape knows which run to stop.
    pub task: Option<String>,
}

struct Inner {
    /// Tab order.
    workspaces: Vec<Workspace>,
    left: Slot,
    right: Slot,
    focused: SlotId,
    /// The global GUI variables (doc "Global GUI variables").
    globals: workspace::Vars,
}

impl Inner {
    fn slot(&self, id: SlotId) -> &Slot {
        match id {
            SlotId::Left => &self.left,
            SlotId::Right => &self.right,
        }
    }

    fn slot_mut(&mut self, id: SlotId) -> &mut Slot {
        match id {
            SlotId::Left => &mut self.left,
            SlotId::Right => &mut self.right,
        }
    }

    fn workspace(&self, id: &str) -> Result<&Workspace, String> {
        self.workspaces
            .iter()
            .find(|w| w.id == id)
            .ok_or_else(|| format!("unknown workspace: {id}"))
    }

    fn workspace_mut(&mut self, id: &str) -> Result<&mut Workspace, String> {
        self.workspaces
            .iter_mut()
            .find(|w| w.id == id)
            .ok_or_else(|| format!("unknown workspace: {id}"))
    }

    fn new_workspace(&mut self, active_repo: Option<String>, repo_name: Option<String>) -> String {
        let n = self.next_workspace_number();
        let id = format!("ws-{n}");
        let base = repo_name.as_deref().unwrap_or("Workspace");
        let index = self.next_index_for_base(base);
        let name = format!("{base} {index}");
        self.workspaces.push(Workspace {
            id: id.clone(),
            name,
            active_repo,
            repo_name,
            auto_index: Some(index),
            vars: Default::default(),
            messages: Vec::new(),
            last_status: None,
            shell: Vec::new(),
            shell_cd: None,
            last_panel: Default::default(),
            ready_panels: Default::default(),
            split: false,
        });
        id
    }

    /// Smallest positive integer `n` such that no existing workspace has the
    /// id `ws-{n}`. Closing a workspace frees its number for reuse, so the
    /// numbering fills gaps rather than growing without bound.
    fn next_workspace_number(&self) -> u64 {
        let used: std::collections::HashSet<&str> =
            self.workspaces.iter().map(|w| w.id.as_str()).collect();
        (1..).find(|n| !used.contains(format!("ws-{n}").as_str())).unwrap()
    }

    /// Smallest positive integer not yet used as the auto-index of an
    /// auto-named workspace sharing the given base (the repository name, or
    /// "Workspace"). Custom-named workspaces (`auto_index == None`) do not
    /// participate, so the counter fills gaps like the id numbering. The
    /// display name is "<base> <index>" (doc "Workspaces").
    fn next_index_for_base(&self, base: &str) -> u64 {
        let used: std::collections::HashSet<u64> = self
            .workspaces
            .iter()
            .filter(|w| w.base() == base)
            .filter_map(|w| w.auto_index)
            .collect();
        (1..).find(|n| !used.contains(n)).unwrap()
    }

    /// Assigns a workspace to a slot, showing the slot and restoring the
    /// workspace's last panel type for it. When that panel type is already
    /// shown for the same workspace in the other visible slot (one instance
    /// exists per (workspace, panel type)), the slot falls back to
    /// `metarecord-detail` — pairing the list with the detail view is the
    /// expected split — and, when that one is taken too or no repo is
    /// active, gets no panel type (the frontend shows the type picker).
    fn assign(&mut self, ws_id: &str, slot_id: SlotId) -> Result<(), String> {
        let ws = self.workspace(ws_id)?;
        let has_repo = ws.active_repo.is_some();
        let wanted = ws
            .last_panel
            .get(&slot_id)
            .cloned()
            .unwrap_or_else(|| default_panel_type(ws.active_repo.as_deref()).to_string());

        let other = self.slot(slot_id.other());
        let other_shows = |panel_type: &str| {
            other.visible
                && other.workspace.as_deref() == Some(ws_id)
                && other.panel_type.as_deref() == Some(panel_type)
        };
        let panel_type = if !other_shows(&wanted) {
            Some(wanted)
        } else if has_repo && !other_shows("metarecord-detail") {
            Some("metarecord-detail".to_string())
        } else {
            None
        };

        let slot = self.slot_mut(slot_id);
        slot.workspace = Some(ws_id.to_string());
        slot.visible = true;
        slot.panel_type = panel_type;
        Ok(())
    }

    /// Shows a workspace just created: in `slot`, focused, or — without one —
    /// like a new tab, in both slots ([`assign_both`](Self::assign_both)).
    fn show_new_workspace(&mut self, ws_id: &str, slot: Option<SlotId>) -> Result<(), String> {
        match slot {
            Some(slot) => {
                self.assign(ws_id, slot)?;
                self.focused = slot;
                Ok(())
            }
            None => self.assign_both(ws_id),
        }
    }

    /// Assigns a workspace to both slots at once. The focused slot is assigned
    /// first (so it keeps its preferred panel type and the other slot pairs
    /// with it) and is always shown; the other slot follows the workspace's
    /// remembered panel count (`split`) — a workspace left split reopens its
    /// second panel, one left single keeps the other slot hidden (it still
    /// remembers its workspace). Keyboard navigation uses this so the two
    /// panels never end up on different workspaces without an explicit action
    /// (a tab click or a dedicated command). See [`record_split`](Self::record_split).
    fn assign_both(&mut self, ws_id: &str) -> Result<(), String> {
        let split = self.workspace(ws_id)?.split;
        let focused = self.focused;
        self.assign(ws_id, focused)?; // `assign` shows the focused slot
        let other = focused.other();
        self.assign(ws_id, other)?;
        self.slot_mut(other).visible = split;
        Ok(())
    }

    /// Records the focused workspace's panel count (`split`) when that
    /// workspace *owns the window*: it fills the focused slot and the other
    /// slot is either hidden or shows the same workspace. A mixed layout (the
    /// two slots on different workspaces) owns nothing and records nothing, so
    /// each workspace keeps the count it last had on its own. Consumed by
    /// [`assign_both`](Self::assign_both) when navigation brings a workspace
    /// back (doc "Panel slots and layout").
    fn record_split(&mut self) {
        let focused = self.focused;
        let Some(ws_id) = self.slot(focused).workspace.clone() else {
            return;
        };
        if !self.slot(focused).visible {
            return;
        }
        let other = self.slot(focused.other());
        let other_same = other.visible && other.workspace.as_deref() == Some(ws_id.as_str());
        // The workspace owns the window when it is alone (other hidden) or
        // paired with itself; a different workspace next to it records nothing.
        if other_same || !other.visible {
            if let Ok(ws) = self.workspace_mut(&ws_id) {
                ws.split = other_same;
            }
        }
    }

    /// Keeps the focus on a shown slot: the keys go to the focused slot, so a
    /// focus left on a hidden one beside a shown one would make every key act
    /// on a panel nobody sees. Run after every mutation, whatever it did; a
    /// focus it moves is a layout change to announce.
    fn settle_focus(&mut self, emit: Emit) -> Emit {
        let focused = self.focused;
        if self.slot(focused).visible || !self.slot(focused.other()).visible {
            return emit;
        }
        self.focused = focused.other();
        Emit { layout: true, ..emit }
    }

    fn layout_view(&self) -> LayoutView {
        let payload = |slot: &Slot| SlotPayload {
            visible: slot.visible,
            workspace_id: slot.workspace.clone(),
            panel_type: slot.panel_type.clone(),
        };
        LayoutView { left: payload(&self.left), right: payload(&self.right), focused: self.focused }
    }

    fn workspace_infos(&self) -> Vec<WorkspaceInfo> {
        self.workspaces
            .iter()
            .map(|w| WorkspaceInfo {
                id: w.id.clone(),
                name: w.name.clone(),
                active_repo: w.active_repo.clone(),
                repo_name: w.repo_name.clone(),
                work_context: w.active_repo.as_deref().and_then(crate::work_context::active),
            })
            .collect()
    }
}

/// The frontend events a mutation asks [`GuiState::mutate`] / [`GuiState::update`]
/// to emit once it returns. Making this the return value of the mutation
/// closure means a state change cannot silently forget to notify the frontend.
#[derive(Clone, Copy)]
struct Emit {
    workspaces: bool,
    layout: bool,
}

impl Emit {
    const NONE: Emit = Emit { workspaces: false, layout: false };
    const WORKSPACES: Emit = Emit { workspaces: true, layout: false };
    const LAYOUT: Emit = Emit { workspaces: false, layout: true };
    const BOTH: Emit = Emit { workspaces: true, layout: true };
}

/// One slot of a value picker's initial layout (doc "Value picker"):
/// a panel type plus the workspace variables to seed it with.
#[derive(Debug, Clone, Deserialize)]
pub struct PickPanel {
    #[serde(rename = "type")]
    pub panel_type: String,
    #[serde(default)]
    pub vars: Map<String, Value>,
}

/// Everything the calling form decides about its picker. The caller owns the
/// whole initial state — only it knows what the picker is for (doc
/// "Value picker"). `caller_ws` and `token` form the return contract. The
/// picker opens in the *other* slot, so a single panel describes it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PickSpec {
    /// Workspace the result is delivered back to (the form's own workspace).
    pub caller_ws: String,
    /// Opaque value echoed back in `pick_result`, matching the result to the
    /// exact widget that requested it.
    pub token: Value,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    /// Status-bar prompt posted in the picker.
    #[serde(default)]
    pub prompt: Option<String>,
    /// What `pick:confirm` returns: `"uuid"` (the selected metarecord, default)
    /// or `"path"` (the first `selected_paths` entry, e.g. a folder chosen in
    /// `file-manager`).
    #[serde(default)]
    pub result: Option<String>,
    pub panel: PickPanel,
}

impl GuiState {
    /// Initial state: one empty workspace assigned to the visible left slot.
    pub fn new(notifier: Arc<dyn FrontendNotifier>) -> Self {
        let mut inner = Inner {
            workspaces: Vec::new(),
            left: Slot::default(),
            right: Slot::default(),
            focused: SlotId::Left,
            globals: Default::default(),
        };
        let id = inner.new_workspace(None, None);
        inner.assign(&id, SlotId::Left).expect("assigning the initial workspace cannot fail");
        GuiState {
            inner: Mutex::new(inner),
            scripts: Mutex::new(HashMap::new()),
            script_keys: AtomicBool::new(true),
            questions: Mutex::new(Vec::new()),
            txns: Mutex::new(HashMap::new()),
            next_txn: AtomicU64::new(1),
            shells: Default::default(),
            script_env: std::sync::OnceLock::new(),
            notifier,
        }
    }

    /// Names this GUI and its daemon to what it starts: `METAFOLDER_GUI_URL`
    /// for `mf gui …`, `METAFOLDER_DAEMON_PORT` for every other `mf`, so a
    /// script reaches the GUI that launched it and that GUI's daemon, even when
    /// they run on ports other than the configuration files' (doc "Scripting
    /// the GUI"). Set once, at startup; a later call is ignored.
    pub fn set_script_env(&self, gui_port: u16, daemon_port: u16) {
        let _ = self.script_env.set(vec![
            ("METAFOLDER_GUI_URL", format!("http://127.0.0.1:{gui_port}")),
            ("METAFOLDER_DAEMON_PORT", daemon_port.to_string()),
        ]);
    }

    /// The variables [`GuiState::set_script_env`] set, to pass to a child.
    pub fn script_env(&self) -> &[(&'static str, String)] {
        self.script_env.get().map_or(&[], Vec::as_slice)
    }

    /// The workspaces' persistent shells, where the lines the user types run.
    pub fn shells(&self) -> &crate::shell_session::ShellSessions {
        &self.shells
    }

    /// Marks a shell script (run id `task_id`, launched from `ws_id`) as running
    /// under a human `label` and broadcasts the running set, so the frontend can
    /// show a loading indicator (doc "Script sessions"). Paired with
    /// [`Self::script_end`].
    pub fn script_begin(&self, task_id: &str, ws_id: &str, label: &str) {
        // Read before the scripts lock is taken: the two locks are never held
        // together.
        let launch_layout = self.shown_workspaces();
        self.scripts.lock_recover().insert(
            task_id.to_string(),
            ScriptTask {
                ws_id: ws_id.to_string(),
                label: label.to_string(),
                workspaces: vec![ws_id.to_string()],
                launch_layout,
                ..Default::default()
            },
        );
        // A new run always starts with its keys live: the checkbox is a choice
        // about the script being answered, not a lasting handicap for the next.
        // The shell mirrors the flag, so turning it back on is announced.
        if !self.script_keys.swap(true, Ordering::Relaxed) {
            self.push_questions();
        }
        self.emit_scripts();
    }

    /// Records (or forgets) the process-group leader of run `task_id` — set
    /// right after the spawn, cleared as soon as the child is reaped: a reaped
    /// pid can be recycled, and a stale one would aim a later signal at
    /// whatever process inherited the number. Unknown run ids are ignored.
    pub fn script_set_pid(&self, task_id: &str, pid: Option<u32>) {
        if let Some(task) = self.scripts.lock_recover().get_mut(task_id) {
            task.pid = pid;
        }
    }

    /// The process group of run `task_id`, when the GUI launched it.
    pub fn script_pid(&self, task_id: &str) -> Option<u32> {
        self.scripts.lock_recover().get(task_id).and_then(|t| t.pid)
    }

    /// Whether a script's awaited keys currently reach the script.
    pub fn script_keys_enabled(&self) -> bool {
        self.script_keys.load(Ordering::Relaxed)
    }

    /// Sets that flag (the question bar's checkbox, `script-keys:toggle`).
    /// Returns the new value.
    pub fn set_script_keys(&self, enabled: bool) -> bool {
        self.script_keys.store(enabled, Ordering::Relaxed);
        enabled
    }

    /// The questions scripts are asking right now.
    pub fn questions(&self) -> Vec<Question> {
        self.questions.lock_recover().clone()
    }

    /// Adds a question (a wait beginning).
    pub fn add_question(&self, question: Question) {
        self.questions.lock_recover().push(question);
    }

    /// Removes question `id` (its wait's guard, on every exit path).
    pub fn remove_question(&self, id: u64) {
        self.questions.lock_recover().retain(|q| q.id != id);
    }

    /// Records that run `task_id` owns workspace `ws_id` — called when a script
    /// creates one through `POST /gui/workspaces` with its run id. Idempotent;
    /// an unknown run id is ignored (a script run outside the GUI owns nothing).
    pub fn script_claim_workspace(&self, task_id: &str, ws_id: &str) {
        {
            let mut scripts = self.scripts.lock_recover();
            let Some(task) = scripts.get_mut(task_id) else { return };
            if task.workspaces.iter().any(|w| w == ws_id) {
                return;
            }
            task.workspaces.push(ws_id.to_string());
            task.opened.push(ws_id.to_string());
        }
        self.emit_scripts();
    }

    /// The workspace run `task_id` asks its questions in: the last one it
    /// opened that is still open, else the one it was launched from (doc
    /// "Script sessions"). `None` for an unknown run id.
    pub fn script_question_workspace(&self, task_id: &str) -> Option<String> {
        let owned = self.script_workspaces(task_id);
        let open: Vec<String> = self.workspaces().into_iter().map(|w| w.id).collect();
        owned.iter().rev().find(|ws| open.contains(ws)).or(owned.first()).cloned()
    }

    /// The workspaces run `task_id` owns; empty for an unknown run id.
    pub fn script_workspaces(&self, task_id: &str) -> Vec<String> {
        self.scripts.lock_recover().get(task_id).map(|t| t.workspaces.clone()).unwrap_or_default()
    }

    /// Marks run `task_id` as blocked on (or released from) a user answer.
    /// Unknown run ids are ignored, and an unchanged flag rebroadcasts nothing.
    pub fn script_waiting(&self, task_id: &str, waiting: bool) {
        {
            let mut scripts = self.scripts.lock_recover();
            let Some(task) = scripts.get_mut(task_id) else { return };
            if task.waiting == waiting {
                return;
            }
            task.waiting = waiting;
        }
        self.emit_scripts();
    }

    /// Updates a running script's determinate progress (`mf gui progress`).
    /// Only the provided fields are overwritten; unknown ids are ignored (a
    /// script run outside the GUI reports to nothing). Rebroadcasts on a hit.
    pub fn script_progress(
        &self,
        task_id: &str,
        done: Option<u64>,
        total: Option<u64>,
        phase: Option<String>,
    ) {
        {
            let mut scripts = self.scripts.lock_recover();
            let Some(task) = scripts.get_mut(task_id) else { return };
            if done.is_some() {
                task.done = done;
            }
            if total.is_some() {
                task.total = total;
            }
            if phase.is_some() {
                task.phase = phase;
            }
        }
        self.emit_scripts();
    }

    /// Records that the user is ending run `task_id` — `how` is "stopped" or
    /// "killed" — so that [`Self::script_end`] cleans up after it. Unknown run
    /// ids are ignored.
    pub fn script_mark_stopped(&self, task_id: &str, how: &'static str) {
        if let Some(task) = self.scripts.lock_recover().get_mut(task_id) {
            task.ended_by = Some(how);
        }
    }

    /// How the user ended run `task_id` ("stopped" or "killed"), if they did.
    pub fn script_stopped(&self, task_id: &str) -> Option<&'static str> {
        self.scripts.lock_recover().get(task_id).and_then(|task| task.ended_by)
    }

    /// Clears the running mark for run id `task_id` and rebroadcasts.
    ///
    /// A run that was *stopped* is cleaned up here (doc "Script sessions"): a
    /// killed script runs nothing more, so the workspaces it opened would stay
    /// open and the slots it took would keep showing them. A run that ended on
    /// its own is left alone — what it leaves behind is its decision, and
    /// `!mf gui workspace new` is a run too.
    pub fn script_end(&self, task_id: &str) {
        let ended = self.scripts.lock_recover().remove(task_id);
        self.emit_scripts();
        if let Some(task) = ended.filter(|task| task.ended_by.is_some()) {
            self.clean_up_after(&task);
        }
    }

    /// Closes the workspaces a stopped script opened, and gives each slot that
    /// was showing one of them what it showed when the script was launched. A
    /// slot the user pointed elsewhere meanwhile is not touched: the slots are
    /// given back, not reset.
    fn clean_up_after(&self, task: &ScriptTask) {
        let layout = self.layout();
        let taken = |slot: &layout::SlotPayload| {
            slot.workspace_id.as_ref().is_some_and(|ws| task.opened.contains(ws))
        };
        let slots = [
            (SlotId::Left, taken(&layout.left), &task.launch_layout.0),
            (SlotId::Right, taken(&layout.right), &task.launch_layout.1),
        ];
        for ws_id in &task.opened {
            // Already closed by the script's own exit trap, or by the user.
            let _ = self.close_workspace(ws_id);
        }
        for (slot_id, taken, before) in slots {
            if !taken {
                continue;
            }
            match before {
                // A workspace closed since the launch cannot come back: the
                // slot keeps what closing the script's workspace left it.
                Some(ws_id) => {
                    let _ = self.tab_assign(ws_id, slot_id);
                }
                None => self.hide_slot(slot_id),
            }
        }
    }

    /// The visible workspace of the left and of the right slot.
    fn shown_workspaces(&self) -> (Option<String>, Option<String>) {
        let layout = self.layout();
        let shown = |slot: &layout::SlotPayload| {
            if slot.visible {
                slot.workspace_id.clone()
            } else {
                None
            }
        };
        (shown(&layout.left), shown(&layout.right))
    }

    /// The `script-task-changed` payload: the scripts running now. What the
    /// shell starts from, as well as what every change announces.
    pub fn script_tasks_payload(&self) -> Value {
        let tasks: Vec<Value> = self
            .scripts
            .lock_recover()
            .iter()
            .map(|(id, t)| {
                json!({
                    "task": id,
                    "workspace_id": t.ws_id,
                    "label": t.label,
                    "phase": t.phase,
                    "done": t.done,
                    "total": t.total,
                    "workspaces": t.workspaces,
                    "waiting": t.waiting,
                })
            })
            .collect();
        json!({ "tasks": tasks })
    }

    fn emit_scripts(&self) {
        self.notifier.emit(events::SCRIPT_TASK_CHANGED, self.script_tasks_payload());
    }

    /// The `input-wait-changed` payload: the questions up and the script keys
    /// flag, so the frontend shows the question bar of the workspace on screen
    /// with the right checkbox state (doc "Script sessions").
    pub fn questions_payload(&self) -> Value {
        json!({ "questions": self.questions(), "script_keys": self.script_keys_enabled() })
    }

    /// Announces [`Self::questions_payload`].
    ///
    /// No keybinding is installed for the awaited keys: the shell gives them to
    /// the question on screen before any binding, and a binding in the table would
    /// also fire for a question put away in another workspace and answer it
    /// unseen.
    pub fn push_questions(&self) {
        self.notifier.emit(events::INPUT_WAIT_CHANGED, self.questions_payload());
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // Recover rather than cascade panics if a previous holder panicked.
        // The GUI state is its own source of truth (nothing to repopulate it
        // from), so the guard is reclaimed as-is; a panic mid-mutation may
        // leave a minor inconsistency, far preferable to a permanently dead
        // GUI.
        self.inner.lock_recover()
    }

    /// Emits an arbitrary event through the frontend notifier (used by
    /// engine helpers outside this module, e.g. keybinding pushes).
    pub fn notify(&self, event: &str, payload: Value) {
        self.notifier.emit(event, payload);
    }

    /// Emits the barrier `id` behind every event already emitted (one
    /// emitter, delivered in order).
    pub fn event_barrier(&self, id: u64) {
        self.notifier.emit(events::EVENT_BARRIER, json!({ "id": id }));
    }

    /// Re-sends the workspaces: what they show changed outside this state —
    /// the work context in force on a repository.
    pub fn refresh_workspaces(&self) {
        let inner = self.lock();
        self.emit_workspaces(&inner);
    }

    fn emit_workspaces(&self, inner: &Inner) {
        self.notifier
            .emit(events::WORKSPACES_CHANGED, json!({ "workspaces": inner.workspace_infos() }));
    }

    fn emit_layout(&self, inner: &Inner) {
        let view = inner.layout_view();
        self.notifier
            .emit(events::LAYOUT_CHANGED, serde_json::to_value(view).expect("layout serializes"));
    }

    fn dispatch(&self, inner: &Inner, emit: Emit) {
        if emit.workspaces {
            self.emit_workspaces(inner);
        }
        if emit.layout {
            self.emit_layout(inner);
        }
    }

    /// Runs a fallible mutation under the lock, then emits exactly the events
    /// it returns — and only on success (an error emits nothing, just as the
    /// hand-written `return Err(..)` paths did). Centralizing the
    /// lock → mutate → emit sequence keeps a mutation from silently skipping
    /// the frontend notification.
    fn mutate<T>(
        &self,
        f: impl FnOnce(&mut Inner) -> Result<(T, Emit), String>,
    ) -> Result<T, String> {
        let mut inner = self.lock();
        let (value, emit) = f(&mut inner)?;
        let emit = inner.settle_focus(emit);
        self.dispatch(&inner, emit);
        Ok(value)
    }

    /// Infallible counterpart of [`mutate`](Self::mutate).
    fn update<T>(&self, f: impl FnOnce(&mut Inner) -> (T, Emit)) -> T {
        let mut inner = self.lock();
        let (value, emit) = f(&mut inner);
        let emit = inner.settle_focus(emit);
        self.dispatch(&inner, emit);
        value
    }

    // ── Read accessors ───────────────────────────────────────────────────

    pub fn workspaces(&self) -> Vec<WorkspaceInfo> {
        self.lock().workspace_infos()
    }

    pub fn layout(&self) -> LayoutView {
        self.lock().layout_view()
    }

    /// Workspace shown in the focused slot, if any.
    pub fn focused_workspace_id(&self) -> Option<String> {
        let inner = self.lock();
        inner.slot(inner.focused).workspace.clone()
    }

    pub fn messages(&self, ws_id: &str) -> Result<Vec<MessageEntry>, String> {
        Ok(self.lock().workspace(ws_id)?.messages.clone())
    }

    /// The last status-bar message posted in `ws_id` ([`Workspace::last_status`]).
    pub fn last_status(&self, ws_id: &str) -> Result<Option<StatusEntry>, String> {
        Ok(self.lock().workspace(ws_id)?.last_status.clone())
    }

    pub fn shell_log(&self, ws_id: &str) -> Result<Vec<ShellEntry>, String> {
        Ok(self.lock().workspace(ws_id)?.shell.clone())
    }

    pub fn get_var(&self, ws_id: &str, key: &str) -> Result<Value, String> {
        let inner = self.lock();
        let ws = inner.workspace(ws_id)?;
        // `active_repo` is a standard variable (doc "Workspace variables") but lives as a
        // workspace field: set at creation, never changed.
        if key == "active_repo" {
            return Ok(ws.active_repo.as_deref().map(Value::from).unwrap_or(Value::Null));
        }
        Ok(ws.vars.get(key).cloned().unwrap_or(Value::Null))
    }

    /// Every variable of a workspace (doc "Workspace variables"), `active_repo`
    /// included — it is one to whoever reads them, though it lives as a
    /// workspace field. What a panel built after some were set starts from: the
    /// change events only tell it what moves afterwards.
    pub fn vars(&self, ws_id: &str) -> Result<Map<String, Value>, String> {
        let inner = self.lock();
        let ws = inner.workspace(ws_id)?;
        let mut vars: Map<String, Value> =
            ws.vars.iter().map(|(key, value)| (key.clone(), value.clone())).collect();
        if let Some(repo) = ws.active_repo.as_deref() {
            vars.insert("active_repo".to_string(), Value::from(repo));
        }
        Ok(vars)
    }
}

/// The selection a `cd` is recorded under: both halves of it, since a panel
/// may set the metarecord alone.
fn shell_selection(ws: &Workspace) -> Value {
    let var = |key| ws.vars.get(key).cloned().unwrap_or(Value::Null);
    json!([var("selected_metarecord"), var("selected_paths")])
}

fn slot_name(slot: SlotId) -> &'static str {
    match slot {
        SlotId::Left => "left",
        SlotId::Right => "right",
    }
}

fn slot_from_name(name: &str) -> Option<SlotId> {
    match name {
        "left" => Some(SlotId::Left),
        "right" => Some(SlotId::Right),
        _ => None,
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests;
