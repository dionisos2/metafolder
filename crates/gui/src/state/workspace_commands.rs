use super::*;

impl GuiState {
    // ── Workspace commands ───────────────────────────────────────────────

    /// Creates a workspace without assigning it to a slot (GUI HTTP API).
    pub fn create_workspace(&self, active_repo: Option<String>) -> String {
        self.create_workspace_named(active_repo, None)
    }

    /// [`create_workspace`](Self::create_workspace) carrying the repository's
    /// human name (resolved by the caller from the daemon) so the workspace is
    /// auto-named after it (doc "Workspaces").
    pub fn create_workspace_named(
        &self,
        active_repo: Option<String>,
        repo_name: Option<String>,
    ) -> String {
        self.create_workspace_with_panels(active_repo, repo_name, &[])
    }

    /// [`create_workspace_named`](Self::create_workspace_named) with the panel
    /// type each slot shows the first time the workspace is assigned to it,
    /// in place of the default one. For a script's scratch workspace: shown
    /// with its default panel first, it mounts the metarecord list, which
    /// loads the repository and publishes its first row as the selection —
    /// later, over what the script had shown meanwhile (doc "Script sessions").
    pub fn create_workspace_with_panels(
        &self,
        active_repo: Option<String>,
        repo_name: Option<String>,
        panels: &[(SlotId, String)],
    ) -> String {
        self.update(|inner| {
            let id = inner.new_workspace(active_repo, repo_name);
            let ws = inner.workspace_mut(&id).expect("freshly created workspace");
            for (slot, panel_type) in panels {
                ws.last_panel.insert(*slot, panel_type.clone());
            }
            (id, Emit::WORKSPACES)
        })
    }

    /// `workspace:new` — creates a workspace and shows it in both slots, on
    /// the repository its argument names (doc "workspace:new"): `self-repo`
    /// inherits the focused workspace's, `none` takes none.
    pub fn workspace_new(&self, repo: NewWorkspaceRepo) -> String {
        self.workspace_new_named(repo, None)
    }

    /// [`workspace_new`](Self::workspace_new) carrying the repository's human
    /// name (resolved by the caller from the daemon) so the workspace is
    /// auto-named after it. With `self-repo` the focused workspace's repo
    /// *and* its name are both inherited, so an inherited tab is numbered
    /// under the same base.
    pub fn workspace_new_named(&self, repo: NewWorkspaceRepo, repo_name: Option<String>) -> String {
        self.update(|inner| {
            let (active_repo, repo_name) = match repo {
                NewWorkspaceRepo::Repo(repo) => (Some(repo), repo_name),
                NewWorkspaceRepo::None => (None, None),
                NewWorkspaceRepo::SelfRepo => inner
                    .slot(inner.focused)
                    .workspace
                    .as_deref()
                    .and_then(|id| inner.workspace(id).ok())
                    .map(|w| (w.active_repo.clone(), w.repo_name.clone()))
                    .unwrap_or((None, None)),
            };
            let id = inner.new_workspace(active_repo, repo_name);
            // Both panels follow the new workspace (a hidden slot just
            // remembers it), so creating a tab never leaves the two panels
            // on different workspaces.
            inner.assign_both(&id).expect("freshly created workspace");
            (id, Emit::BOTH)
        })
    }

    /// `mf.workspace.create` — a workspace with nothing in it but its
    /// repository, named by the caller (none is inherited: a fork is
    /// [`fork`](Self::fork)). Shown in `show_in`, which takes the focus, or —
    /// without one — like a new tab, in both slots. Returns its id.
    pub fn workspace_create(
        &self,
        active_repo: Option<String>,
        repo_name: Option<String>,
        show_in: Option<SlotId>,
    ) -> Result<String, String> {
        self.mutate(|inner| {
            let id = inner.new_workspace(active_repo, repo_name);
            inner.show_new_workspace(&id, show_in)?;
            Ok((id, Emit::BOTH))
        })
    }

    /// `workspace:send` — writes variables into a workspace, any workspace:
    /// the one channel between workspaces (doc "Workspaces"). All or nothing:
    /// `active_repo`, fixed at creation, refuses the whole send.
    pub fn send_vars(&self, ws_id: &str, vars: Map<String, Value>) -> Result<(), String> {
        refuse_fixed(&vars)?;
        let mut inner = self.lock();
        inner.workspace(ws_id)?;
        self.write(&mut inner, vec![(ws_id.to_string(), vars.into_iter().collect())], Vec::new());
        Ok(())
    }

    /// `workspace:fork` — a new workspace identical to `source`: its
    /// repository, every variable, the panels it showed and its panel count;
    /// shown like a new tab. A value picker is not forked: the copy would be a
    /// second picker answering the same form. Returns the fork's id.
    pub fn fork(&self, source: &str) -> Result<String, String> {
        self.mutate(|inner| {
            let ws = inner.workspace(source)?;
            if ws.vars.get("pick_request").is_some_and(|v| !v.is_null()) {
                return Err("a value picker cannot be forked: confirm or cancel it first".into());
            }
            let (repo, repo_name) = (ws.active_repo.clone(), ws.repo_name.clone());
            let (vars, last_panel, split) = (ws.vars.clone(), ws.last_panel.clone(), ws.split);
            let id = inner.new_workspace(repo, repo_name);
            {
                let fork = inner.workspace_mut(&id)?;
                fork.vars = vars;
                fork.last_panel = last_panel;
                fork.split = split;
            }
            inner.show_new_workspace(&id, None)?;
            Ok((id, Emit::BOTH))
        })
    }

    /// Closes a workspace: removes its tab, and the window moves to the
    /// previous workspace in tab order (the next one when the first tab was
    /// closed). When the closed workspace had the window to itself, both slots
    /// move, as [`assign_both`](Inner::assign_both) does, so the replacement
    /// shows the panel count it was left with; in a mixed layout only the
    /// slot showing it switches. With no workspace left, every slot showing it
    /// becomes unassigned (but stays visible).
    pub fn close_workspace(&self, ws_id: &str) -> Result<(), String> {
        // A value picker closed by hand is a cancel (doc "Value picker"): the
        // form that opened it is waiting for an answer, and the slot it took
        // is to be given back. With the caller gone there is nobody to answer,
        // and it closes like any workspace.
        let is_picker = self
            .lock()
            .workspace(ws_id)
            .is_ok_and(|ws| ws.vars.get("pick_request").is_some_and(|v| !v.is_null()));
        if is_picker && self.finish_pick(ws_id, false).is_ok() {
            return Ok(());
        }
        // As closing a terminal ends its shell.
        self.shells.close(ws_id);
        self.mutate(|inner| {
            let index = inner
                .workspaces
                .iter()
                .position(|w| w.id == ws_id)
                .ok_or_else(|| format!("unknown workspace: {ws_id}"))?;
            // Whether the closed workspace had the window to itself (as
            // `record_split` defines it), read before it goes.
            let owned = {
                let (focused, other) =
                    (inner.slot(inner.focused), inner.slot(inner.focused.other()));
                focused.visible
                    && focused.workspace.as_deref() == Some(ws_id)
                    && (!other.visible || other.workspace.as_deref() == Some(ws_id))
            };
            inner.workspaces.remove(index);
            let replacement = (!inner.workspaces.is_empty())
                .then(|| inner.workspaces[index.saturating_sub(1)].id.clone());
            if let (true, Some(id)) = (owned, &replacement) {
                // Like the navigation commands: both panels move, and the
                // workspace comes back with as many panels as it was left with.
                inner.assign_both(id)?;
                return Ok(((), Emit::BOTH));
            }
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

    /// `workspace:close` — closes the focused slot's workspace.
    pub fn workspace_close(&self) -> Result<(), String> {
        let ws_id = self.focused_workspace_id().ok_or("no workspace in the focused slot")?;
        self.close_workspace(&ws_id)
    }

    pub fn rename_workspace(&self, ws_id: &str, name: &str) -> Result<(), String> {
        self.mutate(|inner| {
            let ws = inner.workspace_mut(ws_id)?;
            ws.name = name.to_string();
            // A user-chosen name is frozen and leaves its base's auto-numbering.
            ws.auto_index = None;
            Ok(((), Emit::WORKSPACES))
        })
    }

    /// Assigns an existing workspace to a slot (tab click), showing the
    /// slot if it was hidden, and restoring the workspace's last panel
    /// type for that slot.
    pub fn tab_assign(&self, ws_id: &str, slot: SlotId) -> Result<(), String> {
        self.mutate(|inner| {
            inner.assign(ws_id, slot)?;
            inner.record_split();
            Ok(((), Emit::LAYOUT))
        })
    }

    /// `workspace:next slot` — assigns the next workspace (tab order,
    /// wrapping) to the focused slot only.
    pub fn workspace_next_in_slot(&self) -> Result<(), String> {
        self.workspace_step_in_slot(1)
    }

    /// `workspace:prev slot` — assigns the previous workspace (wrapping) to
    /// the focused slot only.
    pub fn workspace_prev_in_slot(&self) -> Result<(), String> {
        self.workspace_step_in_slot(-1)
    }

    /// `workspace:next` — moves BOTH panels to the next workspace (wrapping).
    pub fn workspace_next(&self) -> Result<(), String> {
        self.workspace_step(1)
    }

    /// `workspace:prev` — moves BOTH panels to the previous workspace.
    pub fn workspace_prev(&self) -> Result<(), String> {
        self.workspace_step(-1)
    }

    pub(super) fn workspace_step_in_slot(&self, direction: isize) -> Result<(), String> {
        self.step(direction, false)
    }

    pub(super) fn workspace_step(&self, direction: isize) -> Result<(), String> {
        self.step(direction, true)
    }

    /// Steps the focused-slot workspace by `direction` (wrapping); `both`
    /// moves the two panels together (keyboard navigation), otherwise only
    /// the focused slot (the explicit `workspace:next slot` /
    /// `workspace:prev slot` commands).
    pub(super) fn step(&self, direction: isize, both: bool) -> Result<(), String> {
        self.mutate(|inner| {
            if inner.workspaces.is_empty() {
                return Err("no workspaces".into());
            }
            let len = inner.workspaces.len() as isize;
            let current = inner.slot(inner.focused).workspace.clone();
            let target =
                match current.and_then(|id| inner.workspaces.iter().position(|w| w.id == id)) {
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

    /// `workspace:goto` — moves BOTH panels to workspace N (1-based tab position).
    pub fn workspace_goto(&self, n: usize) -> Result<(), String> {
        self.mutate(|inner| {
            if n == 0 || n > inner.workspaces.len() {
                return Err(format!("no workspace at position {n}"));
            }
            let ws_id = inner.workspaces[n - 1].id.clone();
            inner.assign_both(&ws_id)?;
            Ok(((), Emit::LAYOUT))
        })
    }

    // ── Workspace variables ──────────────────────────────────────────────

    /// The workspace's active repository, or the error a command that needs
    /// one reports.
    pub fn active_repo(&self, ws_id: &str) -> Result<String, String> {
        match self.get_var(ws_id, "active_repo")? {
            Value::String(repo) => Ok(repo),
            _ => Err("no active repository in this workspace".into()),
        }
    }

    /// Tells the panels showing metarecords (list, detail, file manager, log)
    /// that something they display may have changed: `metarecords:dirty` set to
    /// a fresh nonce, the current time in milliseconds.
    pub fn mark_metarecords_dirty(&self, ws_id: &str) -> Result<(), String> {
        self.set_var(ws_id, "metarecords:dirty", Value::from(now_ms()))
    }

    pub fn set_var(&self, ws_id: &str, key: &str, value: Value) -> Result<(), String> {
        self.send_vars(ws_id, Map::from_iter([(key.to_string(), value)]))
    }
}
