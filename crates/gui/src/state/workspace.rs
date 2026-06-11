//! Workspace: self-contained state container (spec-gui "Workspace").

use super::layout::SlotId;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;

pub struct Workspace {
    pub id: String,
    pub name: String,
    /// Set at creation, never changed afterwards (spec-gui "Workspace").
    pub active_repo: Option<String>,
    /// Reactive per-workspace key-value store shared by panel types.
    pub vars: HashMap<String, Value>,
    /// Append-only log shown by the `message` panel type.
    pub messages: Vec<MessageEntry>,
    /// Last panel type displayed per slot, restored on re-assignment.
    pub last_panel: HashMap<SlotId, String>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct MessageEntry {
    /// Milliseconds since the Unix epoch (formatted by the frontend).
    pub ts_ms: u64,
    pub text: String,
}

/// Public descriptor used by `workspaces-changed` and the GUI HTTP API.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct WorkspaceInfo {
    pub id: String,
    pub name: String,
    pub active_repo: Option<String>,
}
