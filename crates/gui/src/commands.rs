//! Tauri command layer: thin `#[tauri::command]` wrappers over the
//! engine modules. No logic here — everything testable lives in
//! `state`, `keybindings`, `command_registry` and `config`.

use crate::command_registry::{CommandDef, CommandRegistry, Scope};
use crate::config::ConfigDir;
use crate::daemon_proxy::{DaemonProxy, ProxyResponse};
use crate::keybindings::{CompiledBinding, KeybindingSet};
use crate::state::layout::{LayoutView, SlotId};
use crate::state::workspace::{MessageEntry, WorkspaceInfo};
use crate::state::GuiState;
use serde::Serialize;
use serde_json::Value;
use std::sync::{Arc, Mutex};

/// Shared application context managed by Tauri.
pub struct App {
    pub gui: Arc<GuiState>,
    pub registry: Arc<CommandRegistry>,
    pub config: Arc<ConfigDir>,
    /// Shared with the GUI HTTP server (temporary /gui/input bindings).
    pub keybindings: Arc<Mutex<KeybindingSet>>,
    pub gui_port: u16,
    pub daemon: Arc<DaemonProxy>,
    /// Shared /gui/input + /gui/prompt wait lock.
    pub input: Arc<crate::server::input_wait::InputWait>,
    /// Keeps the style.css auto-reload watcher alive.
    pub style_watcher: Mutex<Option<crate::style_watcher::StyleWatcher>>,
}

type AppHandle<'a> = tauri::State<'a, Arc<App>>;

#[derive(Serialize)]
pub struct InitialState {
    pub workspaces: Vec<WorkspaceInfo>,
    pub layout: LayoutView,
    pub keybindings: Vec<CompiledBinding>,
    pub commands: Vec<CommandDef>,
    pub panel_types: Vec<String>,
    pub style_css: String,
    pub gui_port: u16,
    pub daemon_url: String,
}

#[tauri::command]
pub fn get_initial_state(app: AppHandle) -> Result<InitialState, String> {
    let layout = app.gui.layout();
    let focused_panel = match layout.focused {
        SlotId::Left => layout.left.panel_type.clone(),
        SlotId::Right => layout.right.panel_type.clone(),
    };
    Ok(InitialState {
        workspaces: app.gui.workspaces(),
        layout: app.gui.layout(),
        keybindings: app.keybindings.lock().unwrap().compiled(),
        commands: app.registry.list(focused_panel.as_deref()),
        panel_types: app.config.list_panel_types()?,
        style_css: app.config.load_style(),
        gui_port: app.gui_port,
        daemon_url: app.daemon.base_url(),
    })
}

// ── Tabs ─────────────────────────────────────────────────────────────────

#[tauri::command]
pub fn tab_new(app: AppHandle, active_repo: Option<String>) -> String {
    app.gui.tab_new(active_repo)
}

#[tauri::command]
pub fn tab_close(app: AppHandle) -> Result<(), String> {
    app.gui.tab_close()
}

/// Mouse path: the tab's close button targets its own workspace, which
/// is not necessarily the focused one (unlike `tab:close`).
#[tauri::command]
pub fn tab_close_ws(app: AppHandle, ws_id: String) -> Result<(), String> {
    app.gui.close_workspace(&ws_id)
}

#[tauri::command]
pub fn tab_rename(app: AppHandle, ws_id: String, name: String) -> Result<(), String> {
    app.gui.rename_workspace(&ws_id, &name)
}

#[tauri::command]
pub fn tab_assign(app: AppHandle, ws_id: String, slot: SlotId) -> Result<(), String> {
    app.gui.tab_assign(&ws_id, slot)
}

#[tauri::command]
pub fn tab_next(app: AppHandle) -> Result<(), String> {
    app.gui.tab_next()
}

#[tauri::command]
pub fn tab_prev(app: AppHandle) -> Result<(), String> {
    app.gui.tab_prev()
}

#[tauri::command]
pub fn tab_goto(app: AppHandle, n: usize) -> Result<(), String> {
    app.gui.tab_goto(n)
}

// ── Slots ────────────────────────────────────────────────────────────────

#[tauri::command]
pub fn panel_split(app: AppHandle) -> Result<(), String> {
    app.gui.panel_split()
}

#[tauri::command]
pub fn panel_close(app: AppHandle) -> Result<(), String> {
    app.gui.panel_close()
}

/// Mouse path: the slot header's close button hides its own slot,
/// which is not necessarily the non-focused one (unlike `panel:close`).
#[tauri::command]
pub fn slot_hide(app: AppHandle, slot: SlotId) {
    app.gui.hide_slot(slot)
}

#[tauri::command]
pub fn panel_focus_next(app: AppHandle) {
    app.gui.focus_next()
}

#[tauri::command]
pub fn panel_set_type(app: AppHandle, slot: SlotId, panel_type: String) -> Result<(), String> {
    app.gui.set_panel_type(slot, &panel_type)
}

// ── Workspace variables ──────────────────────────────────────────────────

#[tauri::command]
pub fn ws_get_var(app: AppHandle, ws_id: String, key: String) -> Result<Value, String> {
    app.gui.get_var(&ws_id, &key)
}

#[tauri::command]
pub fn ws_set_var(app: AppHandle, ws_id: String, key: String, value: Value) -> Result<(), String> {
    app.gui.set_var(&ws_id, &key, value)
}

#[tauri::command]
pub fn ws_vars(app: AppHandle, ws_id: String) -> Result<Vec<(String, Value)>, String> {
    app.gui.vars(&ws_id)
}

#[tauri::command]
pub fn adopt_repo(app: AppHandle, ws_id: String, repo: String) -> Result<(), String> {
    app.gui.adopt_repo(&ws_id, &repo)
}

// ── Commands and keybindings ─────────────────────────────────────────────

#[tauri::command]
pub fn list_commands(app: AppHandle, focused_panel: Option<String>) -> Vec<CommandDef> {
    app.registry.list(focused_panel.as_deref())
}

#[tauri::command]
pub fn register_command(
    app: AppHandle,
    panel_type: String,
    name: String,
    label: String,
    scope: Option<String>,
    reveal: Option<bool>,
) -> Result<(), String> {
    let scope = match scope.as_deref() {
        None => None,
        Some("local") => Some(Scope::Local),
        Some("global") => Some(Scope::Global),
        Some(other) => return Err(format!("unknown command scope: {other}")),
    };
    app.registry
        .register_panel(&panel_type, &name, &label, scope, reveal.unwrap_or(false));
    Ok(())
}

#[tauri::command]
pub fn suggest_keybinding(
    app: AppHandle,
    combo: String,
    invocation: String,
    when: Option<String>,
    text_input: Option<bool>,
) -> Result<Vec<CompiledBinding>, String> {
    let mut keybindings = app.keybindings.lock().unwrap();
    keybindings.add_suggestion(
        &combo,
        &invocation,
        when.as_deref(),
        text_input.unwrap_or(false),
    )?;
    let compiled = keybindings.compiled();
    crate::push_keybindings(&app.gui, &compiled);
    Ok(compiled)
}

#[tauri::command]
pub fn get_compiled_keybindings(app: AppHandle) -> Vec<CompiledBinding> {
    app.keybindings.lock().unwrap().compiled()
}

/// Settings view: writes a user keybinding override to keybindings.toml,
/// swaps in the recompiled set and pushes it to every document.
#[tauri::command]
pub fn set_user_keybinding(
    app: AppHandle,
    combo: String,
    command: String,
    when: Option<String>,
    text_input: Option<bool>,
) -> Result<Vec<CompiledBinding>, String> {
    let set = app.config.set_user_keybinding(
        &combo,
        &command,
        when.as_deref(),
        text_input.unwrap_or(false),
    )?;
    let compiled = set.compiled();
    *app.keybindings.lock().unwrap() = set;
    crate::push_keybindings(&app.gui, &compiled);
    Ok(compiled)
}

/// Settings view: removes a user override (reverting to the default).
#[tauri::command]
pub fn remove_user_keybinding(
    app: AppHandle,
    combo: String,
) -> Result<Vec<CompiledBinding>, String> {
    let set = app.config.remove_user_keybinding(&combo)?;
    let compiled = set.compiled();
    *app.keybindings.lock().unwrap() = set;
    crate::push_keybindings(&app.gui, &compiled);
    Ok(compiled)
}

#[tauri::command]
pub fn list_panel_types(app: AppHandle) -> Result<Vec<String>, String> {
    app.config.list_panel_types()
}

/// Current stylesheet (user file or shipped default), for manual reloads.
#[tauri::command]
pub fn load_style(app: AppHandle) -> String {
    app.config.load_style()
}

/// Config paths shown in the settings view.
#[derive(Serialize)]
pub struct ConfigInfo {
    pub root: String,
    pub style_css: String,
    pub keybindings: String,
    pub panel_types: String,
}

#[tauri::command]
pub fn config_info(app: AppHandle) -> ConfigInfo {
    ConfigInfo {
        root: app.config.root().display().to_string(),
        style_css: app.config.style_css_path().display().to_string(),
        keybindings: app.config.root().join("keybindings.toml").display().to_string(),
        panel_types: app.config.panel_types_dir().display().to_string(),
    }
}

// ── Status bar / messages ────────────────────────────────────────────────

#[tauri::command]
pub fn post_status(
    app: AppHandle,
    ws_id: String,
    text: String,
    kind: Option<String>,
    timeout_ms: Option<u64>,
) -> Result<(), String> {
    app.gui
        .post_status(&ws_id, &text, kind.as_deref().unwrap_or("info"), timeout_ms)
}

#[tauri::command]
pub fn get_messages(app: AppHandle, ws_id: String) -> Result<Vec<MessageEntry>, String> {
    app.gui.messages(&ws_id)
}

#[tauri::command]
pub fn clear_messages(app: AppHandle, ws_id: String) -> Result<(), String> {
    app.gui.clear_messages(&ws_id)
}

// ── Daemon ───────────────────────────────────────────────────────────────

#[tauri::command]
pub async fn daemon_request(
    app: AppHandle<'_>,
    method: String,
    path: String,
    body: Option<Value>,
) -> Result<ProxyResponse, String> {
    let result = app.daemon.request(&method, &path, body).await;
    if result.is_err() {
        // Likely a daemon outage: refresh the health state right away.
        let daemon = app.daemon.clone();
        let gui = app.gui.clone();
        tauri::async_runtime::spawn(async move {
            daemon.check_health(&gui).await;
        });
    }
    result
}

#[tauri::command]
pub async fn daemon_set_url(app: AppHandle<'_>, url: String) -> Result<bool, String> {
    app.daemon.set_url(url);
    Ok(app.daemon.check_health(&app.gui).await)
}

#[tauri::command]
pub async fn daemon_health(app: AppHandle<'_>) -> Result<bool, String> {
    Ok(app.daemon.check_health(&app.gui).await)
}

/// Compiles a query DSL string to the `Query` JSON IR (shared parser in
/// metafolder-core, same syntax as the CLI).
#[tauri::command]
pub fn parse_query(dsl: String) -> Result<Value, String> {
    let query = metafolder_core::dsl::parse_query(&dsl)?;
    serde_json::to_value(query).map_err(|e| format!("cannot serialize the query: {e}"))
}

// ── Scripting waits ──────────────────────────────────────────────────────

/// `answer:send <value>` — resolves the active `POST /gui/input` wait.
#[tauri::command]
pub fn answer_send(app: AppHandle, value: String) -> Result<(), String> {
    if app.input.resolve_answer(&value) {
        Ok(())
    } else {
        Err("no script is waiting for input".into())
    }
}

/// Command-input resolution of a `POST /gui/prompt` wait.
#[tauri::command]
pub fn prompt_resolve(app: AppHandle, confirm: bool, text: Option<String>) -> bool {
    app.input.resolve_prompt(confirm, text)
}

/// Reported by PanelHost once a panel iframe finished initializing.
#[tauri::command]
pub fn panel_ready(app: AppHandle, ws_id: String, panel_type: String) -> Result<(), String> {
    app.gui.set_panel_ready(&ws_id, &panel_type)
}

#[tauri::command]
pub fn quit(window: tauri::Window) {
    let _ = window.close();
}
