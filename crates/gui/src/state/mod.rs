//! `GuiState`: single source of truth for workspaces, layout, variables and
//! message logs, shared by the Tauri commands and the GUI HTTP API
//! (spec-gui "Concepts"). Every mutation pushes an event to the frontend
//! through the [`FrontendNotifier`].

pub mod layout;
pub mod workspace;

use metafolder_core::sync::MutexExt;
use crate::events;
use crate::notifier::FrontendNotifier;
use layout::{LayoutView, Slot, SlotId, SlotPayload};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use workspace::{MessageEntry, Workspace, WorkspaceInfo};

/// Default panel type shown when a workspace is first displayed: `repos`
/// when no repository is active (entry point), `metarecord-list` otherwise.
/// (Decision; the spec leaves the initial panel type unspecified.)
fn default_panel_type(active_repo: Option<&str>) -> &'static str {
    match active_repo {
        Some(_) => "metarecord-list",
        None => "repos",
    }
}

pub struct GuiState {
    inner: Mutex<Inner>,
    notifier: Arc<dyn FrontendNotifier>,
}

struct Inner {
    /// Tab order.
    workspaces: Vec<Workspace>,
    left: Slot,
    right: Slot,
    focused: SlotId,
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

    fn new_workspace(&mut self, active_repo: Option<String>) -> String {
        let n = self.next_workspace_number();
        let id = format!("ws-{n}");
        self.workspaces.push(Workspace {
            id: id.clone(),
            name: format!("Workspace {n}"),
            active_repo,
            vars: Default::default(),
            messages: Vec::new(),
            last_panel: Default::default(),
            ready_panels: Default::default(),
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

    /// Assigns a workspace to a slot, showing the slot and restoring the
    /// workspace's last panel type for it. When that panel type is already
    /// shown for the same workspace in the other visible slot (one iframe
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

    /// Assigns a workspace to both slots at once, preserving each slot's
    /// visibility (a hidden slot stays hidden, just remembers the new
    /// workspace). Keyboard navigation uses this so the two panels never end
    /// up on different workspaces without an explicit action (a tab click or
    /// a dedicated command). The focused slot is assigned first, so it keeps
    /// its preferred panel type and the other slot pairs with it.
    fn assign_both(&mut self, ws_id: &str) -> Result<(), String> {
        for slot_id in [self.focused, self.focused.other()] {
            let was_visible = self.slot(slot_id).visible;
            self.assign(ws_id, slot_id)?;
            self.slot_mut(slot_id).visible = was_visible;
        }
        Ok(())
    }

    fn layout_view(&self) -> LayoutView {
        let payload = |slot: &Slot| SlotPayload {
            visible: slot.visible,
            workspace_id: slot.workspace.clone(),
            panel_type: slot.panel_type.clone(),
        };
        LayoutView {
            left: payload(&self.left),
            right: payload(&self.right),
            focused: self.focused,
        }
    }

    fn workspace_infos(&self) -> Vec<WorkspaceInfo> {
        self.workspaces
            .iter()
            .map(|w| WorkspaceInfo {
                id: w.id.clone(),
                name: w.name.clone(),
                active_repo: w.active_repo.clone(),
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

/// One slot of a value picker's initial layout (spec-gui "Value picker"):
/// a panel type plus the workspace variables to seed it with.
#[derive(Debug, Clone, Deserialize)]
pub struct PickPanel {
    #[serde(rename = "type")]
    pub panel_type: String,
    #[serde(default)]
    pub vars: Map<String, Value>,
}

/// Everything the calling form decides about its picker. The caller owns the
/// whole initial state — only it knows what the picker is for (spec-gui
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
        };
        let id = inner.new_workspace(None);
        inner
            .assign(&id, SlotId::Left)
            .expect("assigning the initial workspace cannot fail");
        GuiState { inner: Mutex::new(inner), notifier }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // Recover rather than cascade panics if a previous holder panicked.
        // The GUI state is its own source of truth (nothing to repopulate it
        // from), so the guard is reclaimed as-is; a panic mid-mutation may
        // leave a minor inconsistency, far preferable to a permanently dead
        // GUI. See `docs/review-followups.md` (#5).
        self.inner.lock_recover()
    }

    /// Emits an arbitrary event through the frontend notifier (used by
    /// engine helpers outside this module, e.g. keybinding pushes).
    pub fn notify(&self, event: &str, payload: Value) {
        self.notifier.emit(event, payload);
    }

    fn emit_workspaces(&self, inner: &Inner) {
        self.notifier.emit(
            events::WORKSPACES_CHANGED,
            json!({ "workspaces": inner.workspace_infos() }),
        );
    }

    fn emit_layout(&self, inner: &Inner) {
        let view = inner.layout_view();
        self.notifier.emit(
            events::LAYOUT_CHANGED,
            serde_json::to_value(view).expect("layout serializes"),
        );
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
        self.dispatch(&inner, emit);
        Ok(value)
    }

    /// Infallible counterpart of [`mutate`](Self::mutate).
    fn update<T>(&self, f: impl FnOnce(&mut Inner) -> (T, Emit)) -> T {
        let mut inner = self.lock();
        let (value, emit) = f(&mut inner);
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

    pub fn get_var(&self, ws_id: &str, key: &str) -> Result<Value, String> {
        let inner = self.lock();
        let ws = inner.workspace(ws_id)?;
        // `active_repo` is a standard variable (spec-gui) but lives as a
        // workspace field: set at creation, never changed.
        if key == "active_repo" {
            return Ok(ws
                .active_repo
                .as_deref()
                .map(Value::from)
                .unwrap_or(Value::Null));
        }
        Ok(ws.vars.get(key).cloned().unwrap_or(Value::Null))
    }

    pub fn vars(&self, ws_id: &str) -> Result<Vec<(String, Value)>, String> {
        let inner = self.lock();
        let ws = inner.workspace(ws_id)?;
        let mut vars: Vec<(String, Value)> =
            ws.vars.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        vars.push((
            "active_repo".to_string(),
            ws.active_repo.as_deref().map(Value::from).unwrap_or(Value::Null),
        ));
        vars.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(vars)
    }

    // ── Workspace / tab commands ─────────────────────────────────────────

    /// Creates a workspace without assigning it to a slot (GUI HTTP API).
    pub fn create_workspace(&self, active_repo: Option<String>) -> String {
        self.update(|inner| (inner.new_workspace(active_repo), Emit::WORKSPACES))
    }

    /// `tab:new` — creates a workspace and assigns it to the focused slot.
    /// Without an explicit repo, the focused workspace's repo is inherited:
    /// staying on the same repo is the expected default, and switching
    /// costs the same single action either way.
    pub fn tab_new(&self, active_repo: Option<String>) -> String {
        self.update(|inner| {
            let active_repo = active_repo.or_else(|| {
                inner
                    .slot(inner.focused)
                    .workspace
                    .as_deref()
                    .and_then(|id| inner.workspace(id).ok())
                    .and_then(|w| w.active_repo.clone())
            });
            let id = inner.new_workspace(active_repo);
            // Both panels follow the new workspace (a hidden slot just
            // remembers it), so creating a tab never leaves the two panels
            // on different workspaces.
            inner.assign_both(&id).expect("freshly created workspace");
            (id, Emit::BOTH)
        })
    }

    /// Closes a workspace: removes its tab; every slot showing it switches
    /// to the previous workspace in tab order (the next one when the first
    /// tab was closed), or becomes unassigned (but stays visible) when no
    /// workspace remains.
    pub fn close_workspace(&self, ws_id: &str) -> Result<(), String> {
        self.mutate(|inner| {
            let index = inner
                .workspaces
                .iter()
                .position(|w| w.id == ws_id)
                .ok_or_else(|| format!("unknown workspace: {ws_id}"))?;
            inner.workspaces.remove(index);
            let replacement = (!inner.workspaces.is_empty())
                .then(|| inner.workspaces[index.saturating_sub(1)].id.clone());
            for slot_id in [SlotId::Left, SlotId::Right] {
                if inner.slot(slot_id).workspace.as_deref() != Some(ws_id) {
                    continue;
                }
                match &replacement {
                    Some(id) => {
                        // `assign` shows the slot; a slot hidden by
                        // panel:unsplit must stay hidden.
                        let was_visible = inner.slot(slot_id).visible;
                        inner.assign(id, slot_id)?;
                        inner.slot_mut(slot_id).visible = was_visible;
                    }
                    None => {
                        let slot = inner.slot_mut(slot_id);
                        slot.workspace = None;
                        slot.panel_type = None;
                    }
                }
            }
            Ok(((), Emit::BOTH))
        })
    }

    /// `tab:close` — closes the focused slot's workspace.
    pub fn tab_close(&self) -> Result<(), String> {
        let ws_id = self
            .focused_workspace_id()
            .ok_or("no workspace in the focused slot")?;
        self.close_workspace(&ws_id)
    }

    pub fn rename_workspace(&self, ws_id: &str, name: &str) -> Result<(), String> {
        self.mutate(|inner| {
            inner.workspace_mut(ws_id)?.name = name.to_string();
            Ok(((), Emit::WORKSPACES))
        })
    }

    /// Assigns an existing workspace to a slot (tab click), showing the
    /// slot if it was hidden, and restoring the workspace's last panel
    /// type for that slot.
    pub fn tab_assign(&self, ws_id: &str, slot: SlotId) -> Result<(), String> {
        self.mutate(|inner| {
            inner.assign(ws_id, slot)?;
            Ok(((), Emit::LAYOUT))
        })
    }

    /// `tab:next` — assigns the next workspace (tab order, wrapping) to the
    /// focused slot.
    pub fn tab_next(&self) -> Result<(), String> {
        self.tab_step(1)
    }

    /// `tab:prev` — assigns the previous workspace (wrapping).
    pub fn tab_prev(&self) -> Result<(), String> {
        self.tab_step(-1)
    }

    /// `workspace:next` — moves BOTH panels to the next workspace (wrapping).
    pub fn workspace_next(&self) -> Result<(), String> {
        self.workspace_step(1)
    }

    /// `workspace:prev` — moves BOTH panels to the previous workspace.
    pub fn workspace_prev(&self) -> Result<(), String> {
        self.workspace_step(-1)
    }

    fn tab_step(&self, direction: isize) -> Result<(), String> {
        self.step(direction, false)
    }

    fn workspace_step(&self, direction: isize) -> Result<(), String> {
        self.step(direction, true)
    }

    /// Steps the focused-slot workspace by `direction` (wrapping); `both`
    /// moves the two panels together (keyboard navigation), otherwise only
    /// the focused slot (the explicit `tab:next`/`tab:prev` commands).
    fn step(&self, direction: isize, both: bool) -> Result<(), String> {
        self.mutate(|inner| {
            if inner.workspaces.is_empty() {
                return Err("no workspaces".into());
            }
            let len = inner.workspaces.len() as isize;
            let current = inner.slot(inner.focused).workspace.clone();
            let target = match current
                .and_then(|id| inner.workspaces.iter().position(|w| w.id == id))
            {
                Some(pos) => (pos as isize + direction).rem_euclid(len) as usize,
                None => 0,
            };
            let ws_id = inner.workspaces[target].id.clone();
            if both {
                inner.assign_both(&ws_id)?;
            } else {
                let focused = inner.focused;
                inner.assign(&ws_id, focused)?;
            }
            Ok(((), Emit::LAYOUT))
        })
    }

    /// `tab:goto` — moves BOTH panels to workspace N (1-based tab position).
    pub fn tab_goto(&self, n: usize) -> Result<(), String> {
        self.mutate(|inner| {
            if n == 0 || n > inner.workspaces.len() {
                return Err(format!("no workspace at position {n}"));
            }
            let ws_id = inner.workspaces[n - 1].id.clone();
            inner.assign_both(&ws_id)?;
            Ok(((), Emit::LAYOUT))
        })
    }

    // ── Slot commands ────────────────────────────────────────────────────

    /// `panel:split` — shows the hidden slot; if it has no remembered
    /// workspace, shows the focused workspace in both slots (the
    /// collision rule leaves the new slot typeless — same panel type
    /// twice is impossible for one workspace).
    pub fn panel_split(&self) -> Result<(), String> {
        self.mutate(|inner| {
            let target = if !inner.right.visible {
                SlotId::Right
            } else if !inner.left.visible {
                SlotId::Left
            } else {
                return Ok(((), Emit::NONE)); // both slots already visible
            };

            let mut created = false;
            match inner.slot(target).workspace.clone() {
                Some(_) => inner.slot_mut(target).visible = true,
                None => match inner.slot(inner.focused).workspace.clone() {
                    Some(ws_id) => inner.assign(&ws_id, target)?,
                    // Empty layout (no focused workspace at all): a fresh
                    // workspace is the only meaningful split.
                    None => {
                        let id = inner.new_workspace(None);
                        inner.assign(&id, target)?;
                        created = true;
                    }
                },
            }
            Ok(((), Emit { workspaces: created, layout: true }))
        })
    }

    /// `panel:unsplit` — hides the non-focused slot (workspace preserved).
    pub fn panel_unsplit(&self) -> Result<(), String> {
        self.mutate(|inner| {
            let other = inner.focused.other();
            inner.slot_mut(other).visible = false;
            Ok(((), Emit::LAYOUT))
        })
    }

    /// `panel:split-toggle` — splits when one slot is visible, unsplits
    /// when both are.
    pub fn panel_split_toggle(&self) -> Result<(), String> {
        let both_visible = {
            let inner = self.lock();
            inner.left.visible && inner.right.visible
        };
        if both_visible {
            self.panel_unsplit()
        } else {
            self.panel_split()
        }
    }

    /// Hides a slot (GUI API `PUT /gui/layout` with null); the workspace
    /// assignment is remembered. Focus falls back to the other slot.
    pub fn hide_slot(&self, slot_id: SlotId) {
        self.update(|inner| {
            inner.slot_mut(slot_id).visible = false;
            if inner.focused == slot_id && inner.slot(slot_id.other()).visible {
                inner.focused = slot_id.other();
            }
            ((), Emit::LAYOUT)
        })
    }

    /// Marks a (workspace, panel type) instance as ready (iframe loaded
    /// and initialized); reported by the frontend.
    pub fn set_panel_ready(&self, ws_id: &str, panel_type: &str) -> Result<(), String> {
        let mut inner = self.lock();
        inner
            .workspace_mut(ws_id)?
            .ready_panels
            .insert(panel_type.to_string());
        Ok(())
    }

    pub fn panel_ready(&self, ws_id: &str, panel_type: &str) -> bool {
        self.lock()
            .workspace(ws_id)
            .map(|ws| ws.ready_panels.contains(panel_type))
            .unwrap_or(false)
    }

    /// `panel:swap` — exchanges the panel types of the two visible slots
    /// (workspace assignments stay put). Swapping cannot create a
    /// (workspace, panel type) duplicate: when both slots show the same
    /// workspace their types already differ.
    pub fn panel_swap(&self) -> Result<(), String> {
        self.mutate(|inner| {
            if !(inner.left.visible && inner.right.visible) {
                return Err("both panel slots must be visible to swap".into());
            }
            let left_type = inner.left.panel_type.take();
            inner.left.panel_type = inner.right.panel_type.take();
            inner.right.panel_type = left_type;
            for slot_id in [SlotId::Left, SlotId::Right] {
                let slot = inner.slot(slot_id);
                if let (Some(ws_id), Some(panel_type)) =
                    (slot.workspace.clone(), slot.panel_type.clone())
                {
                    inner.workspace_mut(&ws_id)?.last_panel.insert(slot_id, panel_type);
                }
            }
            Ok(((), Emit::LAYOUT))
        })
    }

    /// `panel:focus-next` — moves focus to the other slot if visible.
    pub fn focus_next(&self) {
        self.update(|inner| {
            let other = inner.focused.other();
            let changed = inner.slot(other).visible;
            if changed {
                inner.focused = other;
            }
            ((), Emit { workspaces: false, layout: changed })
        })
    }

    /// `panel:set-type` — switches the panel type displayed in a slot.
    /// Rejected when the other slot already shows the same panel type of
    /// the same workspace (one iframe per (workspace, panel type)).
    pub fn set_panel_type(&self, slot_id: SlotId, panel_type: &str) -> Result<(), String> {
        self.mutate(|inner| {
            let ws_id = inner
                .slot(slot_id)
                .workspace
                .clone()
                .ok_or("no workspace assigned to this slot")?;

            let other_id = slot_id.other();
            let other = inner.slot(other_id);
            let collides = other.workspace.as_deref() == Some(ws_id.as_str())
                && other.panel_type.as_deref() == Some(panel_type);
            if collides {
                if other.visible {
                    return Err(format!(
                        "'{panel_type}' is already displayed for this workspace in the other panel"
                    ));
                }
                // The other slot is hidden but remembers this same (workspace,
                // panel type): drop its assignment so revealing it later cannot
                // duplicate the panel (one iframe per (workspace, panel type)).
                inner.slot_mut(other_id).panel_type = None;
                inner.workspace_mut(&ws_id)?.last_panel.remove(&other_id);
            }

            inner.slot_mut(slot_id).panel_type = Some(panel_type.to_string());
            inner
                .workspace_mut(&ws_id)?
                .last_panel
                .insert(slot_id, panel_type.to_string());
            Ok(((), Emit::LAYOUT))
        })
    }

    /// Sets `active_repo` on a workspace that does not have one yet
    /// (spec-gui "Repo indicator": in-place selection at startup). Once
    /// set, the repo never changes — open another workspace instead.
    pub fn adopt_repo(&self, ws_id: &str, repo: &str) -> Result<(), String> {
        let mut inner = self.lock();
        let ws = inner.workspace_mut(ws_id)?;
        if ws.active_repo.is_some() {
            return Err("this workspace already has a repository; open a new tab".into());
        }
        ws.active_repo = Some(repo.to_string());
        self.emit_workspaces(&inner);
        self.notifier.emit(
            events::WORKSPACE_VAR_CHANGED,
            json!({ "workspace_id": ws_id, "key": "active_repo", "value": repo }),
        );
        Ok(())
    }

    // ── Workspace variables ──────────────────────────────────────────────

    pub fn set_var(&self, ws_id: &str, key: &str, value: Value) -> Result<(), String> {
        if key == "active_repo" {
            return Err("active_repo is set at workspace creation and cannot change".into());
        }
        let mut inner = self.lock();
        inner
            .workspace_mut(ws_id)?
            .vars
            .insert(key.to_string(), value.clone());
        self.notifier.emit(
            events::WORKSPACE_VAR_CHANGED,
            json!({ "workspace_id": ws_id, "key": key, "value": value }),
        );
        Ok(())
    }

    // ── Value picker (spec-gui "Value picker") ───────────────────────────

    /// `metafolder.pick.start` — opens a picker in the slot *other* than the
    /// caller's, so the calling form stays visible in the focused slot. Creates
    /// the picker workspace, marks it with `pick_request` (carrying the other
    /// slot's prior state for restoration), focuses it and posts the prompt.
    /// Returns the picker workspace id.
    pub fn pick_start(&self, spec: PickSpec) -> Result<String, String> {
        let prompt = spec.prompt.clone();
        let ws_id = self.mutate(|inner| {
            // The caller must exist: the result has nowhere to go otherwise.
            inner.workspace(&spec.caller_ws)?;
            // The caller is in the focused slot; the picker takes the other one.
            let caller_slot = inner.focused;
            let picker_slot = caller_slot.other();
            // Remember the picker slot's prior state so finish_pick restores it.
            let prior = inner.slot(picker_slot);
            let restore = json!({
                "workspace": prior.workspace,
                "panel_type": prior.panel_type,
                "visible": prior.visible,
            });

            let ws_id = inner.new_workspace(spec.repo.clone());
            {
                let ws = inner.workspace_mut(&ws_id)?;
                if let Some(name) = &spec.name {
                    ws.name = name.clone();
                }
                ws.vars.insert(
                    "pick_request".into(),
                    json!({
                        "caller_ws": spec.caller_ws,
                        "token": spec.token,
                        "picker_slot": slot_name(picker_slot),
                        "restore": restore,
                    }),
                );
                ws.last_panel.insert(picker_slot, spec.panel.panel_type.clone());
                for (key, value) in &spec.panel.vars {
                    ws.vars.insert(key.clone(), value.clone());
                }
            }
            // Show the picker in the other slot and focus it (the user navigates
            // there and confirms); the caller slot is left untouched.
            inner.assign(&ws_id, picker_slot)?;
            inner.slot_mut(picker_slot).visible = true;
            inner.focused = picker_slot;
            Ok((ws_id, Emit::BOTH))
        })?;

        if let Some(prompt) = prompt {
            // Best-effort: a missing prompt must not fail the pick.
            let _ = self.post_status(&ws_id, &prompt, "info", None);
        }
        Ok(ws_id)
    }

    /// `pick:confirm` — hands the focused picker's `selected_metarecord` uuid
    /// back to the calling workspace as `pick_result`, then closes the picker.
    pub fn pick_confirm(&self) -> Result<(), String> {
        self.finish_pick(true)
    }

    /// `pick:cancel` — closes the focused picker, returning to the caller with
    /// `pick_result = {token, cancelled: true}` and no value.
    pub fn pick_cancel(&self) -> Result<(), String> {
        self.finish_pick(false)
    }

    /// Shared body of [`pick_confirm`](Self::pick_confirm) /
    /// [`pick_cancel`](Self::pick_cancel): reads the focused picker's request
    /// and selection, builds the result, closes the picker, refocuses the
    /// caller, then delivers `pick_result`.
    fn finish_pick(&self, confirm: bool) -> Result<(), String> {
        // Read the picker's link + slot-restore info + current selection.
        let (picker_ws, caller_ws, token, picker_slot, restore, selected) = {
            let inner = self.lock();
            let picker_ws = inner
                .slot(inner.focused)
                .workspace
                .clone()
                .ok_or("no focused workspace")?;
            let ws = inner.workspace(&picker_ws)?;
            let request = ws
                .vars
                .get("pick_request")
                .filter(|v| !v.is_null())
                .ok_or("the focused workspace is not a value picker")?;
            let caller_ws = request["caller_ws"]
                .as_str()
                .ok_or("malformed pick_request")?
                .to_string();
            let token = request["token"].clone();
            let picker_slot = request["picker_slot"]
                .as_str()
                .and_then(slot_from_name)
                .ok_or("malformed pick_request")?;
            let restore = request["restore"].clone();
            let selected = ws.vars.get("selected_metarecord").cloned().unwrap_or(Value::Null);
            (picker_ws, caller_ws, token, picker_slot, restore, selected)
        };

        // Build the result (a confirm needs a selected uuid).
        let result = if confirm {
            let uuid = selected
                .get("uuid")
                .and_then(Value::as_str)
                .ok_or("no metarecord selected in the picker")?;
            json!({ "token": token, "uuid": uuid })
        } else {
            json!({ "token": token, "cancelled": true })
        };

        // Close the picker, restore the slot it occupied and refocus the caller.
        self.mutate(|inner| {
            // The caller may have been closed while picking: nowhere to deliver.
            inner.workspace(&caller_ws)?;
            if let Some(index) = inner.workspaces.iter().position(|w| w.id == picker_ws) {
                inner.workspaces.remove(index);
            }
            // Restore the picker slot to exactly what it showed before the pick.
            let slot = inner.slot_mut(picker_slot);
            slot.workspace =
                restore["workspace"].as_str().map(str::to_string);
            slot.panel_type =
                restore["panel_type"].as_str().map(str::to_string);
            slot.visible = restore["visible"].as_bool().unwrap_or(false);
            // Focus returns to the caller's slot.
            inner.focused = picker_slot.other();
            Ok(((), Emit::BOTH))
        })?;

        // Deliver the result (emits WORKSPACE_VAR_CHANGED for the caller).
        self.set_var(&caller_ws, "pick_result", result)
    }

    // ── Status bar / message log ─────────────────────────────────────────

    /// Posts a status bar message (also appended to the message log).
    pub fn post_status(
        &self,
        ws_id: &str,
        text: &str,
        kind: &str,
        timeout_ms: Option<u64>,
    ) -> Result<(), String> {
        self.push_message(ws_id, text)?;
        self.notifier.emit(
            events::STATUS_MESSAGE,
            json!({
                "workspace_id": ws_id,
                "text": text,
                "kind": kind,
                "timeout_ms": timeout_ms,
            }),
        );
        Ok(())
    }

    /// Appends to the message log only (shell output, reconcile results).
    pub fn append_message(&self, ws_id: &str, text: &str) -> Result<(), String> {
        self.push_message(ws_id, text)
    }

    fn push_message(&self, ws_id: &str, text: &str) -> Result<(), String> {
        let entry = MessageEntry { ts_ms: now_ms(), text: text.to_string() };
        let mut inner = self.lock();
        inner.workspace_mut(ws_id)?.messages.push(entry.clone());
        self.notifier.emit(
            events::MESSAGE_APPENDED,
            json!({ "workspace_id": ws_id, "entry": entry }),
        );
        Ok(())
    }

    pub fn clear_messages(&self, ws_id: &str) -> Result<(), String> {
        let mut inner = self.lock();
        inner.workspace_mut(ws_id)?.messages.clear();
        // A null metarecord tells message panels the log was cleared.
        self.notifier.emit(
            events::MESSAGE_APPENDED,
            json!({ "workspace_id": ws_id, "entry": Value::Null }),
        );
        Ok(())
    }
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
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notifier::RecordingNotifier;

    fn state() -> (Arc<RecordingNotifier>, GuiState) {
        let notifier = Arc::new(RecordingNotifier::new());
        let state = GuiState::new(notifier.clone());
        (notifier, state)
    }

    // ── Initial state ────────────────────────────────────────────────────

    #[test]
    fn test_initial_state() {
        let (_, state) = state();
        let workspaces = state.workspaces();
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].id, "ws-1");
        assert_eq!(workspaces[0].name, "Workspace 1");
        assert_eq!(workspaces[0].active_repo, None);

        let layout = state.layout();
        assert!(layout.left.visible);
        assert_eq!(layout.left.workspace_id.as_deref(), Some("ws-1"));
        assert_eq!(layout.left.panel_type.as_deref(), Some("repos"));
        assert!(!layout.right.visible);
        assert_eq!(layout.focused, SlotId::Left);
        assert_eq!(state.focused_workspace_id().as_deref(), Some("ws-1"));
    }

    // ── Tabs ─────────────────────────────────────────────────────────────

    #[test]
    fn test_tab_new_assigns_focused_slot_and_notifies() {
        let (notifier, state) = state();
        notifier.clear();
        let id = state.tab_new(Some("repo-1".into()));
        assert_eq!(id, "ws-2");

        let workspaces = state.workspaces();
        assert_eq!(workspaces.len(), 2);
        assert_eq!(workspaces[1].name, "Workspace 2");
        assert_eq!(workspaces[1].active_repo.as_deref(), Some("repo-1"));

        let layout = state.layout();
        assert_eq!(layout.left.workspace_id.as_deref(), Some("ws-2"));
        // A repo is active: default panel type is metarecord-list.
        assert_eq!(layout.left.panel_type.as_deref(), Some("metarecord-list"));

        assert!(!notifier.payloads(events::WORKSPACES_CHANGED).is_empty());
        assert!(!notifier.payloads(events::LAYOUT_CHANGED).is_empty());
    }

    #[test]
    fn test_tab_new_inherits_the_focused_repo() {
        let (_, state) = state();
        state.tab_new(Some("repo-9".into())); // focused now shows a repo-9 workspace
        let id = state.tab_new(None);
        let info = state.workspaces().into_iter().find(|w| w.id == id).unwrap();
        // Staying on the same repo is the expected default; picking
        // another one costs the same single action either way.
        assert_eq!(info.active_repo.as_deref(), Some("repo-9"));
    }

    #[test]
    fn test_workspace_numbering_reuses_freed_number_after_close() {
        let (_, state) = state();
        let id2 = state.tab_new(None);
        assert_eq!(id2, "ws-2");
        state.close_workspace(&id2).unwrap();
        // The freed number 2 is the lowest available, so it is reused.
        let reused = state.tab_new(None);
        assert_eq!(reused, "ws-2");
        assert_eq!(state.workspaces().last().unwrap().name, "Workspace 2");
    }

    #[test]
    fn test_workspace_numbering_fills_lowest_gap() {
        let (_, state) = state();
        let id2 = state.tab_new(None);
        let _id3 = state.tab_new(None);
        // Close the middle one: the gap at 2 is the lowest available number.
        state.close_workspace(&id2).unwrap();
        let filled = state.tab_new(None);
        assert_eq!(filled, "ws-2");
        // A further workspace takes the next free number above the others.
        let next = state.tab_new(None);
        assert_eq!(next, "ws-4");
    }

    #[test]
    fn test_close_last_workspace_unassigns_every_slot_showing_it() {
        let (_, state) = state();
        // Show ws-1 (the only workspace) in both slots.
        state.panel_split().unwrap();
        state.tab_assign("ws-1", SlotId::Right).unwrap();
        state.close_workspace("ws-1").unwrap();

        let layout = state.layout();
        assert_eq!(layout.left.workspace_id, None);
        assert_eq!(layout.right.workspace_id, None);
        // Slots stay visible, just unassigned.
        assert!(layout.left.visible);
        assert!(layout.right.visible);
        assert!(state.workspaces().iter().all(|w| w.id != "ws-1"));
    }

    #[test]
    fn test_close_workspace_switches_its_slots_to_the_previous_one() {
        let (_, state) = state();
        state.tab_new(None); // ws-2
        state.tab_new(None); // ws-3, shown in the focused (left) slot
        state.close_workspace("ws-3").unwrap();
        assert_eq!(state.layout().left.workspace_id.as_deref(), Some("ws-2"));

        // Closing the first tab falls forward to the next one instead.
        state.tab_assign("ws-1", SlotId::Left).unwrap();
        state.close_workspace("ws-1").unwrap();
        assert_eq!(state.layout().left.workspace_id.as_deref(), Some("ws-2"));
    }

    #[test]
    fn test_close_workspace_keeps_a_hidden_slot_hidden() {
        let (_, state) = state();
        let id2 = state.tab_new(None); // ws-2 in the focused (left) slot
        state.panel_split().unwrap(); // right shows ws-2 too
        state.panel_unsplit().unwrap(); // right hidden, still on ws-2
        state.close_workspace(&id2).unwrap();

        let layout = state.layout();
        assert_eq!(layout.left.workspace_id.as_deref(), Some("ws-1"));
        // The hidden slot moves off the closed workspace but stays hidden.
        assert_eq!(layout.right.workspace_id.as_deref(), Some("ws-1"));
        assert!(!layout.right.visible);
    }

    #[test]
    fn test_tab_close_closes_focused_workspace() {
        let (_, state) = state();
        state.tab_new(None);
        state.tab_close().unwrap();
        assert_eq!(state.workspaces().len(), 1);
        // The slot switches to the remaining workspace, so closing keeps
        // working until none remains.
        assert_eq!(state.layout().left.workspace_id.as_deref(), Some("ws-1"));
        state.tab_close().unwrap();
        assert_eq!(state.layout().left.workspace_id, None);
        // Focused slot now unassigned: tab:close errors.
        assert!(state.tab_close().is_err());
    }

    #[test]
    fn test_rename_workspace() {
        let (_, state) = state();
        state.rename_workspace("ws-1", "Music").unwrap();
        assert_eq!(state.workspaces()[0].name, "Music");
        assert!(state.rename_workspace("ws-99", "x").is_err());
    }

    #[test]
    fn test_tab_next_and_prev_wrap() {
        let (_, state) = state();
        state.tab_new(None); // ws-2, focused slot shows it
        state.tab_new(None); // ws-3
        state.tab_assign("ws-1", SlotId::Left).unwrap();

        state.tab_next().unwrap();
        assert_eq!(state.focused_workspace_id().as_deref(), Some("ws-2"));
        state.tab_next().unwrap();
        assert_eq!(state.focused_workspace_id().as_deref(), Some("ws-3"));
        state.tab_next().unwrap(); // wraps
        assert_eq!(state.focused_workspace_id().as_deref(), Some("ws-1"));
        state.tab_prev().unwrap(); // wraps back
        assert_eq!(state.focused_workspace_id().as_deref(), Some("ws-3"));
    }

    #[test]
    fn test_tab_goto_is_one_based() {
        let (_, state) = state();
        state.tab_new(None); // ws-2
        state.tab_goto(1).unwrap();
        assert_eq!(state.focused_workspace_id().as_deref(), Some("ws-1"));
        state.tab_goto(2).unwrap();
        assert_eq!(state.focused_workspace_id().as_deref(), Some("ws-2"));
        assert!(state.tab_goto(0).is_err());
        assert!(state.tab_goto(3).is_err());
    }

    #[test]
    fn test_tab_goto_moves_both_panels() {
        // Keyboard navigation keeps the two panels on the same workspace so
        // the user never ends up split across workspaces unknowingly.
        let (_, state) = state();
        state.tab_new(None); // ws-2, focused
        state.panel_split().unwrap(); // both slots on ws-2
        state.tab_goto(1).unwrap();
        let layout = state.layout();
        assert_eq!(layout.left.workspace_id.as_deref(), Some("ws-1"));
        assert_eq!(layout.right.workspace_id.as_deref(), Some("ws-1"));
    }

    #[test]
    fn test_tab_new_moves_both_panels() {
        let (_, state) = state();
        state.panel_split().unwrap(); // both slots on ws-1
        let id = state.tab_new(Some("repo-1".into()));
        let layout = state.layout();
        assert_eq!(layout.left.workspace_id.as_deref(), Some(id.as_str()));
        assert_eq!(layout.right.workspace_id.as_deref(), Some(id.as_str()));
        assert!(layout.left.visible);
        assert!(layout.right.visible);
    }

    #[test]
    fn test_workspace_next_and_prev_move_both_panels() {
        let (_, state) = state();
        state.tab_new(None); // ws-2
        state.tab_new(None); // ws-3, focused
        state.panel_split().unwrap(); // both on ws-3
        state.tab_goto(1).unwrap(); // both on ws-1
        state.workspace_next().unwrap();
        let layout = state.layout();
        assert_eq!(layout.left.workspace_id.as_deref(), Some("ws-2"));
        assert_eq!(layout.right.workspace_id.as_deref(), Some("ws-2"));
        state.workspace_prev().unwrap(); // ws-1
        let layout = state.layout();
        assert_eq!(layout.left.workspace_id.as_deref(), Some("ws-1"));
        assert_eq!(layout.right.workspace_id.as_deref(), Some("ws-1"));
    }

    // ── Slots ────────────────────────────────────────────────────────────

    #[test]
    fn test_panel_split_shows_focused_workspace_without_creating_one() {
        let (_, state) = state();
        state.tab_new(Some("repo-9".into())); // focused shows ws-2 (repo-9)
        let count = state.workspaces().len();
        state.panel_split().unwrap();

        let layout = state.layout();
        assert!(layout.right.visible);
        // Same workspace in both slots; no silently created workspace.
        assert_eq!(layout.right.workspace_id, layout.left.workspace_id);
        assert_eq!(state.workspaces().len(), count);
        // The focused slot's panel type cannot be duplicated for the same
        // workspace: the new slot pairs the list with the detail view.
        assert_eq!(layout.right.panel_type.as_deref(), Some("metarecord-detail"));
    }

    #[test]
    fn test_panel_split_without_repo_leaves_the_new_slot_typeless() {
        let (_, state) = state();
        // ws-1 has no repo: left shows "repos", and metarecord-detail is
        // meaningless without a repository — the type picker is shown.
        state.panel_split().unwrap();
        assert_eq!(state.layout().right.panel_type, None);
    }

    #[test]
    fn test_assign_collision_falls_back_to_typeless_when_record_detail_taken() {
        let (_, state) = state();
        let ws1 = state.tab_new(Some("repo-1".into()));
        state.panel_split().unwrap(); // right: ws1 metarecord-detail
        // Remember metarecord-detail as ws1's right-slot panel type.
        state.set_panel_type(SlotId::Right, "metarecord-detail").unwrap();
        // Park another workspace in the right slot, then move the left
        // slot to metarecord-detail (no collision: different workspaces).
        let ws2 = state.create_workspace(Some("repo-1".into()));
        state.tab_assign(&ws2, SlotId::Right).unwrap();
        state.set_panel_type(SlotId::Left, "metarecord-detail").unwrap();
        // ws1 comes back to the right slot wanting metarecord-detail, which
        // the left slot already shows: no fallback left, typeless.
        state.tab_assign(&ws1, SlotId::Right).unwrap();
        assert_eq!(state.layout().right.panel_type, None);
    }

    #[test]
    fn test_panel_swap_exchanges_the_two_panel_types() {
        let (notifier, state) = state();
        state.tab_new(Some("repo-1".into())); // left: metarecord-list
        state.panel_split().unwrap(); // right: metarecord-detail
        notifier.clear();
        state.panel_swap().unwrap();

        let layout = state.layout();
        assert_eq!(layout.left.panel_type.as_deref(), Some("metarecord-detail"));
        assert_eq!(layout.right.panel_type.as_deref(), Some("metarecord-list"));
        assert!(!notifier.payloads(events::LAYOUT_CHANGED).is_empty());
    }

    #[test]
    fn test_panel_swap_requires_both_slots_visible() {
        let (_, state) = state();
        assert!(state.panel_swap().is_err());
    }

    #[test]
    fn test_panel_swap_updates_the_last_panel_memory() {
        let (_, state) = state();
        let ws = state.tab_new(Some("repo-1".into()));
        state.panel_split().unwrap(); // left: metarecord-list, right: metarecord-detail
        state.panel_swap().unwrap(); // left: metarecord-detail, right: metarecord-list

        // Switching away and back restores the swapped types.
        state.tab_new(None);
        state.tab_assign(&ws, SlotId::Left).unwrap();
        assert_eq!(state.layout().left.panel_type.as_deref(), Some("metarecord-detail"));
    }

    #[test]
    fn test_panel_split_restores_remembered_workspace() {
        let (_, state) = state();
        state.panel_split().unwrap();
        let right_ws = state.layout().right.workspace_id.clone().unwrap();
        state.panel_unsplit().unwrap();
        assert!(!state.layout().right.visible);

        let count = state.workspaces().len();
        state.panel_split().unwrap();
        assert_eq!(state.workspaces().len(), count); // no new workspace
        assert_eq!(state.layout().right.workspace_id.as_deref(), Some(right_ws.as_str()));
    }

    #[test]
    fn test_panel_unsplit_hides_non_focused_and_keeps_tab() {
        let (_, state) = state();
        state.panel_split().unwrap();
        let right_ws = state.layout().right.workspace_id.clone().unwrap();
        assert_eq!(state.layout().focused, SlotId::Left);
        state.panel_unsplit().unwrap();

        let layout = state.layout();
        assert!(!layout.right.visible);
        assert!(state.workspaces().iter().any(|w| w.id == right_ws));
    }

    #[test]
    fn test_panel_split_toggle_splits_then_unsplits() {
        let (_, state) = state();
        state.panel_split_toggle().unwrap(); // one visible slot: splits
        assert!(state.layout().right.visible);
        state.panel_split_toggle().unwrap(); // both visible: unsplits
        assert!(!state.layout().right.visible);
        // Toggling again re-splits with the remembered workspace.
        let count = state.workspaces().len();
        state.panel_split_toggle().unwrap();
        assert!(state.layout().right.visible);
        assert_eq!(state.workspaces().len(), count);
    }

    #[test]
    fn test_focus_next_only_targets_visible_slots() {
        let (_, state) = state();
        state.focus_next(); // right hidden: no-op
        assert_eq!(state.layout().focused, SlotId::Left);
        state.panel_split().unwrap();
        state.focus_next();
        assert_eq!(state.layout().focused, SlotId::Right);
        state.focus_next();
        assert_eq!(state.layout().focused, SlotId::Left);
    }

    #[test]
    fn test_set_panel_type_switches_and_remembers() {
        let (_, state) = state();
        state.set_panel_type(SlotId::Left, "log").unwrap();
        assert_eq!(state.layout().left.panel_type.as_deref(), Some("log"));

        // Assign another workspace, then come back: panel type restored.
        state.tab_new(None);
        assert_eq!(state.layout().left.panel_type.as_deref(), Some("repos"));
        state.tab_assign("ws-1", SlotId::Left).unwrap();
        assert_eq!(state.layout().left.panel_type.as_deref(), Some("log"));
    }

    #[test]
    fn test_set_panel_type_rejects_same_type_for_same_workspace_in_both_slots() {
        let (_, state) = state();
        state.panel_split().unwrap();
        state.tab_assign("ws-1", SlotId::Right).unwrap();
        // Left already shows ws-1 as "repos".
        assert!(state.set_panel_type(SlotId::Right, "repos").is_err());
        state.set_panel_type(SlotId::Right, "log").unwrap();
    }

    #[test]
    fn test_set_panel_type_errors_on_unassigned_slot() {
        let (_, state) = state();
        assert!(state.set_panel_type(SlotId::Right, "log").is_err());
    }

    #[test]
    fn test_set_panel_type_evicts_same_type_from_a_hidden_other_slot() {
        let (_, state) = state();
        state.tab_new(Some("repo-1".into())); // ws-2, left: metarecord-list
        state.panel_split().unwrap(); // right: metarecord-detail
        state.panel_unsplit().unwrap(); // right hidden, remembers metarecord-detail
        // Switching the visible slot to the type the hidden slot remembers must
        // not let a later split show two identical panels.
        state.set_panel_type(SlotId::Left, "metarecord-detail").unwrap();
        state.panel_split().unwrap(); // reveal the other slot
        let layout = state.layout();
        assert_eq!(layout.left.panel_type.as_deref(), Some("metarecord-detail"));
        assert_ne!(layout.right.panel_type.as_deref(), Some("metarecord-detail"));
    }

    #[test]
    fn test_hide_slot_moves_focus_to_the_visible_slot() {
        let (_, state) = state();
        state.panel_split().unwrap();
        state.focus_next();
        assert_eq!(state.layout().focused, SlotId::Right);

        // Hiding the focused slot gives focus back to the other one.
        state.hide_slot(SlotId::Right);
        let layout = state.layout();
        assert!(!layout.right.visible);
        assert_eq!(layout.focused, SlotId::Left);
        // Workspace assignment is remembered while hidden.
        assert!(layout.right.workspace_id.is_some());
    }

    #[test]
    fn test_panel_readiness_is_tracked_per_workspace_and_type() {
        let (_, state) = state();
        assert!(!state.panel_ready("ws-1", "repos"));
        state.set_panel_ready("ws-1", "repos").unwrap();
        assert!(state.panel_ready("ws-1", "repos"));
        assert!(!state.panel_ready("ws-1", "metarecord-list"));
        assert!(state.set_panel_ready("ws-99", "repos").is_err());
    }

    // ── Workspace variables ──────────────────────────────────────────────

    #[test]
    fn test_vars_set_get_and_notify() {
        let (notifier, state) = state();
        notifier.clear();
        state
            .set_var("ws-1", "selected_paths", json!(["/tmp/a"]))
            .unwrap();
        assert_eq!(state.get_var("ws-1", "selected_paths").unwrap(), json!(["/tmp/a"]));
        // Unset variable reads as Null ("unknown").
        assert_eq!(state.get_var("ws-1", "selected_metarecord").unwrap(), Value::Null);

        let payloads = notifier.payloads(events::WORKSPACE_VAR_CHANGED);
        assert_eq!(
            payloads,
            vec![json!({
                "workspace_id": "ws-1",
                "key": "selected_paths",
                "value": ["/tmp/a"],
            })]
        );
    }

    #[test]
    fn test_active_repo_is_a_readonly_standard_variable() {
        let (_, state) = state();
        let id = state.tab_new(Some("repo-7".into()));
        // Readable through the variable store (spec-gui standard vars)...
        assert_eq!(state.get_var(&id, "active_repo").unwrap(), json!("repo-7"));
        assert_eq!(state.get_var("ws-1", "active_repo").unwrap(), Value::Null);
        assert!(state
            .vars(&id)
            .unwrap()
            .iter()
            .any(|(k, v)| k == "active_repo" && *v == json!("repo-7")));
        // ...but immutable: set at creation, never changed.
        assert!(state.set_var(&id, "active_repo", json!("other")).is_err());
    }

    #[test]
    fn test_adopt_repo_sets_active_repo_once() {
        let (notifier, state) = state();
        notifier.clear();
        // ws-1 starts with no repo: adoption allowed (spec-gui "Repo
        // indicator": selection sets it in place when null).
        state.adopt_repo("ws-1", "repo-1").unwrap();
        assert_eq!(state.get_var("ws-1", "active_repo").unwrap(), json!("repo-1"));
        assert_eq!(state.workspaces()[0].active_repo.as_deref(), Some("repo-1"));
        // Indicator + panels must hear about it.
        assert!(!notifier.payloads(events::WORKSPACES_CHANGED).is_empty());
        let vars = notifier.payloads(events::WORKSPACE_VAR_CHANGED);
        assert!(vars
            .iter()
            .any(|p| p["key"] == "active_repo" && p["value"] == json!("repo-1")));
        // Second adoption: refused (immutable once set).
        assert!(state.adopt_repo("ws-1", "repo-2").is_err());
    }

    #[test]
    fn test_vars_unknown_workspace_errors() {
        let (_, state) = state();
        assert!(state.set_var("ws-99", "k", json!(1)).is_err());
        assert!(state.get_var("ws-99", "k").is_err());
    }

    // ── Value picker ─────────────────────────────────────────────────────

    fn pick_spec(caller_ws: &str) -> PickSpec {
        let mut vars = Map::new();
        vars.insert("metarecord-list:query".into(), json!("type = \"tag\""));
        PickSpec {
            caller_ws: caller_ws.to_string(),
            token: json!(7),
            repo: Some("repo-1".into()),
            name: Some("Pick: tag".into()),
            prompt: Some("Select a value".into()),
            panel: PickPanel { panel_type: "metarecord-list".into(), vars },
        }
    }

    #[test]
    fn test_pick_start_opens_in_the_other_slot_keeping_the_caller_visible() {
        let (_, state) = state();
        // ws-1 is in the focused (left) slot; the picker must take the right.
        let picker = state.pick_start(pick_spec("ws-1")).unwrap();
        assert_eq!(picker, "ws-2");

        // Marked, linked, and seeded.
        let request = state.get_var(&picker, "pick_request").unwrap();
        assert_eq!(request["caller_ws"], "ws-1");
        assert_eq!(request["token"], 7);
        assert_eq!(request["picker_slot"], "right");
        assert_eq!(
            state.get_var(&picker, "metarecord-list:query").unwrap(),
            json!("type = \"tag\"")
        );

        let layout = state.layout();
        // Caller stays put in the left slot; the picker opens in the right and
        // takes focus.
        assert_eq!(layout.left.workspace_id.as_deref(), Some("ws-1"));
        assert_eq!(layout.right.workspace_id.as_deref(), Some(picker.as_str()));
        assert_eq!(layout.right.panel_type.as_deref(), Some("metarecord-list"));
        assert!(layout.left.visible && layout.right.visible);
        assert_eq!(layout.focused, SlotId::Right);
    }

    #[test]
    fn test_pick_start_errors_when_caller_is_unknown() {
        let (_, state) = state();
        assert!(state.pick_start(pick_spec("ws-99")).is_err());
        // No stray workspace created.
        assert_eq!(state.workspaces().len(), 1);
    }

    #[test]
    fn test_pick_confirm_delivers_uuid_and_restores_the_slot() {
        let (notifier, state) = state();
        let picker = state.pick_start(pick_spec("ws-1")).unwrap();
        state
            .set_var(&picker, "selected_metarecord", json!({ "uuid": "abc", "repo": "repo-1" }))
            .unwrap();
        notifier.clear();

        state.pick_confirm().unwrap();

        // The picker is gone and the caller is focused again.
        assert!(state.workspaces().iter().all(|w| w.id != picker));
        assert_eq!(state.focused_workspace_id().as_deref(), Some("ws-1"));
        // The right slot is restored to its pre-pick state (was hidden/empty).
        let layout = state.layout();
        assert!(!layout.right.visible);
        assert_eq!(layout.right.workspace_id, None);
        // The uuid is delivered to the caller, tagged with the token.
        assert_eq!(
            state.get_var("ws-1", "pick_result").unwrap(),
            json!({ "token": 7, "uuid": "abc" })
        );
        let vars = notifier.payloads(events::WORKSPACE_VAR_CHANGED);
        assert!(vars.iter().any(|p| p["workspace_id"] == "ws-1" && p["key"] == "pick_result"));
    }

    #[test]
    fn test_pick_confirm_restores_a_previously_split_other_slot() {
        let (_, state) = state();
        let other = state.tab_new(Some("repo-1".into())); // ws-2, focused left
        state.panel_split().unwrap(); // right shows ws-2 too (metarecord-detail)
        // Picker opens in the right slot, displacing ws-2's detail view.
        let picker = state.pick_start(pick_spec(&other)).unwrap();
        state
            .set_var(&picker, "selected_metarecord", json!({ "uuid": "abc", "repo": "repo-1" }))
            .unwrap();
        state.pick_confirm().unwrap();

        // The right slot is restored to ws-2's detail view, not left dangling.
        let layout = state.layout();
        assert_eq!(layout.right.workspace_id.as_deref(), Some(other.as_str()));
        assert_eq!(layout.right.panel_type.as_deref(), Some("metarecord-detail"));
        assert!(layout.right.visible);
        assert_eq!(layout.focused, SlotId::Left);
    }

    #[test]
    fn test_pick_confirm_errors_without_a_selection() {
        let (_, state) = state();
        let picker = state.pick_start(pick_spec("ws-1")).unwrap();
        assert!(state.pick_confirm().is_err());
        // The picker is preserved so the user can still choose.
        assert!(state.workspaces().iter().any(|w| w.id == picker));
    }

    #[test]
    fn test_pick_confirm_errors_when_focused_is_not_a_picker() {
        let (_, state) = state();
        // ws-1 has no pick_request.
        assert!(state.pick_confirm().is_err());
    }

    #[test]
    fn test_pick_cancel_delivers_cancelled_and_closes_picker() {
        let (_, state) = state();
        let picker = state.pick_start(pick_spec("ws-1")).unwrap();
        state
            .set_var(&picker, "selected_metarecord", json!({ "uuid": "abc", "repo": "repo-1" }))
            .unwrap();

        state.pick_cancel().unwrap();

        assert!(state.workspaces().iter().all(|w| w.id != picker));
        assert_eq!(state.focused_workspace_id().as_deref(), Some("ws-1"));
        assert_eq!(
            state.get_var("ws-1", "pick_result").unwrap(),
            json!({ "token": 7, "cancelled": true })
        );
    }

    // ── Status bar and message log ───────────────────────────────────────

    #[test]
    fn test_post_status_emits_and_appends_to_log() {
        let (notifier, state) = state();
        notifier.clear();
        state
            .post_status("ws-1", "Entry deleted.", "info", Some(5000))
            .unwrap();

        let statuses = notifier.payloads(events::STATUS_MESSAGE);
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0]["workspace_id"], "ws-1");
        assert_eq!(statuses[0]["text"], "Entry deleted.");
        assert_eq!(statuses[0]["kind"], "info");
        assert_eq!(statuses[0]["timeout_ms"], 5000);

        let log = state.messages("ws-1").unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].text, "Entry deleted.");
        assert_eq!(notifier.payloads(events::MESSAGE_APPENDED).len(), 1);
    }

    #[test]
    fn test_append_and_clear_messages() {
        let (notifier, state) = state();
        notifier.clear();
        state.append_message("ws-1", "stdout: hello").unwrap();
        assert_eq!(state.messages("ws-1").unwrap().len(), 1);
        // append_message goes to the log only, not to the status bar.
        assert!(notifier.payloads(events::STATUS_MESSAGE).is_empty());
        assert_eq!(notifier.payloads(events::MESSAGE_APPENDED).len(), 1);

        state.clear_messages("ws-1").unwrap();
        assert!(state.messages("ws-1").unwrap().is_empty());
        // Clearing notifies panels with a null metarecord.
        let appended = notifier.payloads(events::MESSAGE_APPENDED);
        assert_eq!(appended.len(), 2);
        assert_eq!(appended[1]["entry"], Value::Null);
    }
}
