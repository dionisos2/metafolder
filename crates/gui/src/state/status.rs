use super::*;

impl GuiState {
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
        self.lock().workspace_mut(ws_id)?.last_status =
            Some(StatusEntry { ts_ms: now_ms(), text: text.to_string(), kind: kind.to_string() });
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

    pub(super) fn push_message(&self, ws_id: &str, text: &str) -> Result<(), String> {
        let entry = MessageEntry { ts_ms: now_ms(), text: text.to_string() };
        let mut inner = self.lock();
        inner.workspace_mut(ws_id)?.messages.push(entry.clone());
        self.notifier
            .emit(events::MESSAGE_APPENDED, json!({ "workspace_id": ws_id, "entry": entry }));
        Ok(())
    }

    /// Appends one line of run `run` to the shell log (doc "shell panel").
    pub fn append_shell(
        &self,
        ws_id: &str,
        run: &str,
        kind: ShellLine,
        text: &str,
    ) -> Result<(), String> {
        let entry =
            ShellEntry { ts_ms: now_ms(), run: run.to_string(), kind, text: text.to_string() };
        let mut inner = self.lock();
        inner.workspace_mut(ws_id)?.shell.push(entry.clone());
        self.notifier
            .emit(events::SHELL_APPENDED, json!({ "workspace_id": ws_id, "entry": entry }));
        Ok(())
    }

    /// What decides where the workspace's next shell line runs (doc "shell
    /// panel"): a `cd` made under the current selection, else the selection.
    pub fn shell_place(&self, ws_id: &str) -> Result<ShellPlace, String> {
        let inner = self.lock();
        let ws = inner.workspace(ws_id)?;
        let selection = shell_selection(ws);
        let cd = ws
            .shell_cd
            .as_ref()
            .filter(|(_, under)| *under == selection)
            .map(|(dir, _)| dir.clone());
        // A string, or `{text, bytes}` for a path that is not text: its
        // bytes, exactly (doc "Tree names").
        let selected_path = ws
            .vars
            .get("selected_paths")
            .and_then(|paths| paths.get(0))
            .and_then(|path| {
                serde_json::from_value::<metafolder_core::metarecord::TreePath>(path.clone()).ok()
            })
            .filter(|path| !path.is_empty())
            .map(|path| path.to_path_buf());
        Ok(ShellPlace { selection, cd, selected_path })
    }

    /// Records where a shell line left the shell, under the selection it ran
    /// with ([`ShellPlace::selection`]).
    pub fn set_shell_cd(&self, ws_id: &str, dir: PathBuf, selection: Value) -> Result<(), String> {
        let mut inner = self.lock();
        let ws = inner.workspace_mut(ws_id)?;
        let cd = Some((dir, selection));
        if ws.shell_cd != cd {
            ws.shell_cd = cd;
            self.notifier.emit(events::SHELL_CWD_CHANGED, json!({ "workspace_id": ws_id }));
        }
        Ok(())
    }

    pub fn clear_shell(&self, ws_id: &str) -> Result<(), String> {
        let mut inner = self.lock();
        inner.workspace_mut(ws_id)?.shell.clear();
        // A null entry tells shell panels the log was cleared.
        self.notifier
            .emit(events::SHELL_APPENDED, json!({ "workspace_id": ws_id, "entry": Value::Null }));
        Ok(())
    }

    pub fn clear_messages(&self, ws_id: &str) -> Result<(), String> {
        let mut inner = self.lock();
        inner.workspace_mut(ws_id)?.messages.clear();
        // A null metarecord tells message panels the log was cleared.
        self.notifier
            .emit(events::MESSAGE_APPENDED, json!({ "workspace_id": ws_id, "entry": Value::Null }));
        Ok(())
    }
}
