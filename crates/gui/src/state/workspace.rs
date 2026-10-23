//! Workspace: self-contained state container (doc "Workspaces").

use super::layout::SlotId;
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

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
    pub vars: Vars,
    /// Append-only log shown by the `message` panel type.
    pub messages: Vec<MessageEntry>,
    /// The last status-bar message posted, whether or not the bar still shows
    /// it (a timed one clears itself in the shell). Read by scripts and tests
    /// through `GET /gui/workspaces/:id/messages` (doc "mf gui").
    pub last_status: Option<StatusEntry>,
    /// What the shell lines run in this workspace printed, shown by the
    /// `shell` panel type (doc "shell panel").
    pub shell: Vec<ShellEntry>,
    /// Where the last shell line left the shell (its final `pwd`), with the
    /// selection it ran under: it holds until the selection changes (doc
    /// "shell panel").
    pub shell_cd: Option<(PathBuf, Value)>,
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

/// A workspace's variables, shared: a clone is a pointer copy, so a command
/// transaction's snapshot costs nothing, and a write made while one is held
/// copies the table of pointers, never the values (doc "Command
/// transactions").
#[derive(Clone, Debug, Default)]
pub struct Vars(Arc<HashMap<String, Arc<Value>>>);

impl Vars {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key).map(|value| &**value)
    }

    pub fn insert(&mut self, key: String, value: Value) {
        Arc::make_mut(&mut self.0).insert(key, Arc::new(value));
    }

    pub fn extend(&mut self, vars: impl IntoIterator<Item = (String, Value)>) {
        let map = Arc::make_mut(&mut self.0);
        for (key, value) in vars {
            map.insert(key, Arc::new(value));
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.0.iter().map(|(key, value)| (key, &**value))
    }
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct MessageEntry {
    /// Milliseconds since the Unix epoch (formatted by the frontend).
    pub ts_ms: u64,
    pub text: String,
}

/// A status-bar message as posted: what it said and how (`info`, `error`,
/// `busy`…).
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct StatusEntry {
    /// Milliseconds since the Unix epoch.
    pub ts_ms: u64,
    pub text: String,
    pub kind: String,
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

/// What decides the directory a shell line runs in (doc "shell panel").
#[derive(Clone, Debug, PartialEq)]
pub struct ShellPlace {
    /// The selection now (`[selected_metarecord, selected_paths]`), to record
    /// a `cd` under.
    pub selection: Value,
    /// Where a `cd` left the shell, if made under this very selection.
    pub cd: Option<PathBuf>,
    /// The first of `selected_paths`.
    pub selected_path: Option<String>,
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
    /// The work context in force on the repository (doc "The GUI's work
    /// context") — what the slot header's context indicator shows.
    pub work_context: Option<crate::work_context::WorkContext>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_vars_snapshot_does_not_move_with_later_writes() {
        let mut vars = Vars::default();
        vars.insert("a".into(), json!(1));
        let snapshot = vars.clone();
        vars.insert("a".into(), json!(2));
        vars.extend([("b".to_string(), json!(3))]);
        assert_eq!(snapshot.get("a"), Some(&json!(1)));
        assert_eq!(snapshot.get("b"), None);
        assert_eq!(vars.get("a"), Some(&json!(2)));
        assert_eq!(vars.get("b"), Some(&json!(3)));
    }

    #[test]
    fn test_vars_write_after_snapshot_shares_the_untouched_values() {
        let mut vars = Vars::default();
        vars.insert("big".into(), json!(["a", "b", "c"]));
        let snapshot = vars.clone();
        vars.insert("small".into(), json!(1));
        // The table was copied, the value it points to was not.
        assert!(std::ptr::eq(snapshot.get("big").unwrap(), vars.get("big").unwrap()));
    }
}
