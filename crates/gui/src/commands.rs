//! Tauri command layer: thin `#[tauri::command]` wrappers over the
//! engine modules. No logic here — everything testable lives in
//! `state`, `keybindings`, `command_registry` and `config`.

use metafolder_core::sync::MutexExt;
use crate::command_registry::{CommandDef, CommandRegistry};
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
    /// The simplified-query grammar (read-only), for local query expansion.
    pub grammar: metafolder_core::simplified::grammar::Grammar,
    pub gui_port: u16,
    pub daemon: Arc<DaemonProxy>,
    /// Shared /gui/input + /gui/prompt wait lock.
    pub input: Arc<crate::server::input_wait::InputWait>,
    /// Shared /gui/command dispatch wait registry.
    pub commands: Arc<crate::server::command_wait::CommandWait>,
    /// Shared bench buffer fed by the panels' `performance.measure` reports.
    pub bench: Arc<crate::server::bench::BenchBuffer>,
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
    Ok(InitialState {
        workspaces: app.gui.workspaces(),
        layout: app.gui.layout(),
        keybindings: app.keybindings.lock_recover().compiled(),
        commands: app.registry.list(),
        panel_types: app.config.list_panel_types()?,
        style_css: app.config.load_style()?,
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
pub fn panel_unsplit(app: AppHandle) -> Result<(), String> {
    app.gui.panel_unsplit()
}

#[tauri::command]
pub fn panel_split_toggle(app: AppHandle) -> Result<(), String> {
    app.gui.panel_split_toggle()
}

/// Mouse path: the slot header's close button hides its own slot,
/// which is not necessarily the non-focused one (unlike `panel:unsplit`).
#[tauri::command]
pub fn slot_hide(app: AppHandle, slot: SlotId) {
    app.gui.hide_slot(slot)
}

#[tauri::command]
pub fn panel_swap(app: AppHandle) -> Result<(), String> {
    app.gui.panel_swap()
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
pub fn list_commands(app: AppHandle) -> Vec<CommandDef> {
    app.registry.list()
}

#[tauri::command]
pub fn register_command(
    app: AppHandle,
    panel_type: String,
    name: String,
    label: String,
    reveal: Option<bool>,
    log: Option<bool>,
) -> Result<(), String> {
    app.registry.register_panel(
        &panel_type,
        &name,
        &label,
        reveal.unwrap_or(false),
        log.unwrap_or(true),
    );
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
    let mut keybindings = app.keybindings.lock_recover();
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
    app.keybindings.lock_recover().compiled()
}

/// Settings view: upserts a keybinding in keybindings.toml, swaps in the
/// recompiled set and pushes it to every document.
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
    *app.keybindings.lock_recover() = set;
    crate::push_keybindings(&app.gui, &compiled);
    Ok(compiled)
}

/// Settings view: unbinds a combo in keybindings.toml (reverting to a shipped
/// default is a git operation on the config repo, not done here).
#[tauri::command]
pub fn remove_user_keybinding(
    app: AppHandle,
    combo: String,
) -> Result<Vec<CompiledBinding>, String> {
    let set = app.config.remove_user_keybinding(&combo)?;
    let compiled = set.compiled();
    *app.keybindings.lock_recover() = set;
    crate::push_keybindings(&app.gui, &compiled);
    Ok(compiled)
}

#[tauri::command]
pub fn list_panel_types(app: AppHandle) -> Result<Vec<String>, String> {
    app.config.list_panel_types()
}

/// Current stylesheet, for manual reloads. An unreadable file yields empty CSS
/// at this runtime path; startup already fails when configuration is missing.
#[tauri::command]
pub fn load_style(app: AppHandle) -> String {
    app.config.load_style().unwrap_or_default()
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

/// Appends a line to the workspace message log (used by the command
/// dispatcher to echo command invocations).
#[tauri::command]
pub fn append_message(app: AppHandle, ws_id: String, text: String) -> Result<(), String> {
    app.gui.append_message(&ws_id, &text)
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

/// Expands simplified-language text to normal DSL text locally, via the shared
/// grammar in core — no daemon round-trip (spec-query). Relative date macros
/// resolve against the local clock.
#[tauri::command]
pub fn expand_query(app: AppHandle, text: String) -> Result<String, String> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    metafolder_core::simplified::engine::expand_at(&app.grammar, &text, now_ms)
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

/// A panel reported a `performance.measure` (the bench harness): append it to
/// the buffer that `GET /gui/bench` reads.
#[tauri::command]
pub fn bench_record(app: AppHandle, name: String, duration_ms: f64) {
    app.bench.record(&name, duration_ms);
}

/// Reports the outcome of a `POST /gui/command` invocation back to the waiting
/// HTTP handler. Called by the frontend after its `dispatch()` resolves.
#[tauri::command]
pub fn command_done(app: AppHandle, invocation_id: String, ok: bool, error: Option<String>) {
    use crate::server::command_wait::CommandOutcome;
    let Ok(id) = uuid::Uuid::parse_str(&invocation_id) else {
        return;
    };
    let outcome = if ok {
        CommandOutcome::Ok
    } else {
        CommandOutcome::Error(error.unwrap_or_else(|| "command failed".into()))
    };
    // A missing wait (already timed out) is fine: nothing to resolve.
    app.commands.resolve(id, outcome);
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

/// `devtools:open` — the WebKit inspector; replaces the Inspect Element
/// metarecord of the suppressed native context menu.
#[tauri::command]
pub fn open_devtools(window: tauri::WebviewWindow) {
    window.open_devtools();
}

#[tauri::command]
pub fn quit(window: tauri::Window) {
    let _ = window.close();
}
