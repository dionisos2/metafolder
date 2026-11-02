use super::*;

impl GuiState {
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
                        let id = inner.new_workspace(None, None);
                        inner.assign(&id, target)?;
                        created = true;
                    }
                },
            }
            inner.record_split();
            Ok(((), Emit { workspaces: created, layout: true }))
        })
    }

    /// `panel:unsplit` — hides the non-focused slot (workspace preserved).
    pub fn panel_unsplit(&self) -> Result<(), String> {
        self.mutate(|inner| {
            let other = inner.focused.other();
            inner.slot_mut(other).visible = false;
            inner.record_split();
            Ok(((), Emit::LAYOUT))
        })
    }

    /// `panel:toggle split` — splits when one slot is visible, unsplits
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
            inner.record_split();
            ((), Emit::LAYOUT)
        })
    }

    /// Marks a (workspace, panel type) instance as ready (panel mounted
    /// and initialized); reported by the frontend.
    pub fn set_panel_ready(&self, ws_id: &str, panel_type: &str) -> Result<(), String> {
        let mut inner = self.lock();
        inner.workspace_mut(ws_id)?.ready_panels.insert(panel_type.to_string());
        Ok(())
    }

    pub fn panel_ready(&self, ws_id: &str, panel_type: &str) -> bool {
        self.lock().workspace(ws_id).map(|ws| ws.ready_panels.contains(panel_type)).unwrap_or(false)
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

    /// `panel:focus next` — moves focus to the other slot if visible.
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

    /// Focuses a specific slot if it is visible. Idempotent (unlike
    /// `focus_next`'s toggle), so the click-to-focus handler can fire it more
    /// than once for one click — a focusable target raises both `pointerdown`
    /// and `focusin` — without the two cancelling out.
    pub fn focus_slot(&self, slot_id: SlotId) {
        self.update(|inner| {
            let changed = inner.slot(slot_id).visible && inner.focused != slot_id;
            if changed {
                inner.focused = slot_id;
            }
            ((), Emit { workspaces: false, layout: changed })
        })
    }

    /// `panel:open here` — switches the panel type displayed in a slot.
    /// Rejected when the other slot already shows the same panel type of
    /// the same workspace (one instance per (workspace, panel type)).
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
                // duplicate the panel (one instance per (workspace, panel type)).
                inner.slot_mut(other_id).panel_type = None;
                inner.workspace_mut(&ws_id)?.last_panel.remove(&other_id);
            }

            inner.slot_mut(slot_id).panel_type = Some(panel_type.to_string());
            inner.workspace_mut(&ws_id)?.last_panel.insert(slot_id, panel_type.to_string());
            Ok(((), Emit::LAYOUT))
        })
    }

    /// Sets `active_repo` on a workspace that does not have one yet
    /// (doc "Workspaces": in-place selection at startup). Once
    /// set, the repo never changes — open another workspace instead.
    pub fn adopt_repo(&self, ws_id: &str, repo: &str) -> Result<(), String> {
        self.adopt_repo_named(ws_id, repo, None)
    }

    /// [`adopt_repo`](Self::adopt_repo) carrying the repository's human name
    /// (resolved by the caller from the daemon). An auto-named workspace is
    /// re-based to the repo's counter ("<repo> N"); a user-named one keeps its
    /// custom name.
    pub fn adopt_repo_named(
        &self,
        ws_id: &str,
        repo: &str,
        repo_name: Option<String>,
    ) -> Result<(), String> {
        let mut inner = self.lock();
        if inner.workspace(ws_id)?.active_repo.is_some() {
            return Err("this workspace already has a repository; open a new tab".into());
        }
        // Re-number under the new base before mutating, while the workspace
        // still carries its old base (so it is not counted against itself).
        let base = repo_name.as_deref().unwrap_or("Workspace");
        let rename = inner.workspace(ws_id)?.auto_index.is_some().then(|| {
            let index = inner.next_index_for_base(base);
            (index, format!("{base} {index}"))
        });
        let ws = inner.workspace_mut(ws_id)?;
        ws.active_repo = Some(repo.to_string());
        ws.repo_name = repo_name;
        if let Some((index, name)) = rename {
            ws.auto_index = Some(index);
            ws.name = name;
        }
        self.emit_workspaces(&inner);
        self.notifier.emit(
            events::WORKSPACE_VAR_CHANGED,
            json!({ "workspace_id": ws_id, "key": "active_repo", "value": repo }),
        );
        Ok(())
    }
}
