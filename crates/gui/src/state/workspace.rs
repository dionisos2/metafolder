//! Workspace: self-contained state container (doc "Workspaces").

use super::layout::SlotId;
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

pub struct Workspace {
    pub id: String,
    pub name: String,
    /// Set at creation, never changed afterwards (doc "Workspaces").
    pub active_repo: Option<String>,
    /// Human repository name captured when the workspace adopts a repo (the
    /// daemon owns the canonical name; `active_repo` is only its uuid). `None`
    /// with no repo or when the name could not be resolved. Frozen at load:
    /// a later daemon-side repo rename does not propagate here.
    pub repo_name: Option<String>,
    /// The per-base counter that produced the auto-generated `name`
    /// ("<base> <auto_index>", base = `repo_name` or "Workspace"). `None`
    /// once the user (or a picker) sets a custom name, which also excludes
    /// the workspace from the auto-numbering of its base.
    pub auto_index: Option<u64>,
    /// Reactive per-workspace key-value store shared by panel types.
    pub vars: HashMap<String, Value>,
    /// Append-only log shown by the `message` panel type.
    pub messages: Vec<MessageEntry>,
    /// What the shell lines run in this workspace printed, shown by the
    /// `shell` panel type (doc "shell panel").
    pub shell: Vec<ShellEntry>,
    /// Last panel type displayed per slot, restored on re-assignment.
    pub last_panel: HashMap<SlotId, String>,
    /// Panel count remembered while this workspace owned the window: `true`
    /// = two panels, `false` = one. Restored by keyboard navigation
    /// (`assign_both`); recorded by split/unsplit/close while the workspace
    /// owns the window (doc "Panel slots and layout").
    pub split: bool,
    /// Panel types whose instance finished mounting in this workspace
    /// (GET /gui/panels/:slot/view "loading"/"ready").
    pub ready_panels: HashSet<String>,
}

impl Workspace {
    /// The auto-naming base: the repository name, or "Workspace" with none.
    /// Workspaces sharing a base share one auto-numbering sequence.
    pub fn base(&self) -> &str {
        self.repo_name.as_deref().unwrap_or("Workspace")
    }
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct MessageEntry {
    /// Milliseconds since the Unix epoch (formatted by the frontend).
    pub ts_ms: u64,
    pub text: String,
}

/// One line of the shell log (doc "shell panel").
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct ShellEntry {
    /// Milliseconds since the Unix epoch (formatted by the frontend).
    pub ts_ms: u64,
    /// The run it belongs to (`script-N`, the run's task id): two runs at once
    /// interleave their lines, and the panel groups them back by run.
    pub run: String,
    pub kind: ShellLine,
    pub text: String,
}

/// What a shell-log line is.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ShellLine {
    /// The command line run, first line of its run.
    Command,
    Stdout,
    Stderr,
    /// How the run ended, when worth saying: `exit N`, `stopped`, `killed`.
    Status,
}

/// Public descriptor used by `workspaces-changed` and the GUI HTTP API.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct WorkspaceInfo {
    pub id: String,
    pub name: String,
    pub active_repo: Option<String>,
    /// The repository's name as captured when the workspace adopted it (see
    /// [`Workspace::repo_name`]) — what the slot header's repo indicator shows.
    pub repo_name: Option<String>,
}
