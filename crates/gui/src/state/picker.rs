use super::*;

impl GuiState {
    // ── Value picker (doc "Value picker") ───────────────────────────

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

            // The workspace primitives (doc "Workspaces"): an empty workspace —
            // not a fork, so the caller's own list state cannot override the
            // seed — sent the seed and the request that makes it a picker.
            let ws_id = inner.new_workspace(spec.repo.clone(), None);
            let mut vars = spec.panel.vars.clone();
            vars.insert(
                "pick_request".into(),
                json!({
                    "caller_ws": spec.caller_ws,
                    "token": spec.token,
                    "picker_slot": slot_name(picker_slot),
                    "restore": restore,
                    "result": spec.result.clone().unwrap_or_else(|| "uuid".into()),
                }),
            );
            {
                let ws = inner.workspace_mut(&ws_id)?;
                if let Some(name) = &spec.name {
                    // A picker's explicit name is custom: freeze it and drop it
                    // out of its base's auto-numbering.
                    ws.name = name.clone();
                    ws.auto_index = None;
                }
                ws.vars.extend(vars);
                ws.last_panel.insert(picker_slot, spec.panel.panel_type.clone());
            }
            // Shown in the other slot, focused (the user navigates there and
            // confirms); the caller slot is left untouched.
            inner.show_new_workspace(&ws_id, Some(picker_slot))?;
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
        let picker = self.focused_workspace_id().ok_or("no focused workspace")?;
        self.finish_pick(&picker, true)
    }

    /// `pick:cancel` — closes the focused picker, returning to the caller with
    /// `pick_result = {token, cancelled: true}` and no value.
    pub fn pick_cancel(&self) -> Result<(), String> {
        let picker = self.focused_workspace_id().ok_or("no focused workspace")?;
        self.finish_pick(&picker, false)
    }

    /// Shared body of [`pick_confirm`](Self::pick_confirm) /
    /// [`pick_cancel`](Self::pick_cancel): reads the picker's request and
    /// selection, builds the result, closes the picker, refocuses the caller,
    /// then delivers `pick_result`.
    pub(super) fn finish_pick(&self, picker_ws: &str, confirm: bool) -> Result<(), String> {
        // Read the picker's link + slot-restore info + current selection.
        let (caller_ws, token, picker_slot, restore, result_kind, selected, paths) = {
            let inner = self.lock();
            let ws = inner.workspace(picker_ws)?;
            let request = ws
                .vars
                .get("pick_request")
                .filter(|v| !v.is_null())
                .ok_or("the focused workspace is not a value picker")?;
            let caller_ws =
                request["caller_ws"].as_str().ok_or("malformed pick_request")?.to_string();
            let token = request["token"].clone();
            let picker_slot = request["picker_slot"]
                .as_str()
                .and_then(slot_from_name)
                .ok_or("malformed pick_request")?;
            let restore = request["restore"].clone();
            let result_kind = request["result"].as_str().unwrap_or("uuid").to_string();
            let selected = ws.vars.get("selected_metarecord").cloned().unwrap_or(Value::Null);
            let paths = ws.vars.get("selected_paths").cloned().unwrap_or(Value::Null);
            (caller_ws, token, picker_slot, restore, result_kind, selected, paths)
        };

        // Build the result (a confirm needs the kind of selection requested).
        let result = if !confirm {
            json!({ "token": token, "cancelled": true })
        } else if result_kind == "path" {
            // As it is: a string, or `{text, bytes}` for a path that is not
            // text (doc "Tree names").
            let path = paths
                .as_array()
                .and_then(|a| a.first())
                .filter(|p| p.is_string() || p.get("bytes").is_some_and(Value::is_string))
                .ok_or("no path selected in the picker")?;
            json!({ "token": token, "path": path })
        } else {
            let uuid = selected
                .get("uuid")
                .and_then(Value::as_str)
                .ok_or("no metarecord selected in the picker")?;
            json!({ "token": token, "uuid": uuid })
        };

        // Close the picker, restore the slot it occupied and refocus the caller.
        self.mutate(|inner| {
            // The caller may have been closed while picking: nowhere to deliver.
            inner.workspace(&caller_ws)?;
            if let Some(index) = inner.workspaces.iter().position(|w| w.id == picker_ws) {
                inner.workspaces.remove(index);
            }
            // Restore the picker slot to exactly what it showed before the pick
            // — unless that workspace was closed meanwhile: the slot is then
            // left empty and hidden, never naming a workspace that is gone.
            let prior = restore["workspace"].as_str().filter(|ws| inner.workspace(ws).is_ok());
            let slot = SlotPayload {
                workspace_id: prior.map(str::to_string),
                panel_type: prior.and(restore["panel_type"].as_str()).map(str::to_string),
                visible: prior.is_some() && restore["visible"].as_bool().unwrap_or(false),
            };
            let picker_slot_mut = inner.slot_mut(picker_slot);
            picker_slot_mut.workspace = slot.workspace_id;
            picker_slot_mut.panel_type = slot.panel_type;
            picker_slot_mut.visible = slot.visible;
            // Focus returns to the caller's slot — shown again if it was hidden
            // while picking and the restored slot is hidden too: the window
            // would show nothing.
            let caller_slot = picker_slot.other();
            inner.focused = caller_slot;
            // The picker was put in the caller's slot too (a tab click, or the
            // workspace closed there): the caller comes back in its place, to
            // be given the result.
            if inner.slot(caller_slot).workspace.as_deref() == Some(picker_ws) {
                let visible = inner.slot(caller_slot).visible;
                inner.assign(&caller_ws, caller_slot)?;
                inner.slot_mut(caller_slot).visible = visible;
            }
            if !inner.slot(picker_slot).visible && inner.slot(caller_slot).workspace.is_some() {
                inner.slot_mut(caller_slot).visible = true;
            }
            Ok(((), Emit::BOTH))
        })?;
        // As closing a terminal ends its shell, closing the picker ends its own.
        self.shells.close(picker_ws);

        // Deliver the result through the one channel between workspaces
        // (emits WORKSPACE_VAR_CHANGED for the caller).
        self.send_vars(&caller_ws, Map::from_iter([("pick_result".to_string(), result)]))
    }
}
