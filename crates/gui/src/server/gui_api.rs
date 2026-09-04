//! `/gui/*` scripting endpoints (spec-gui "Scripting / GUI API").

use super::ServerState;
use crate::events;
use crate::keybindings::{CompiledBinding, KeybindingSet};
use crate::server::command_wait::CommandOutcome;
use crate::server::input_wait::{InputOutcome, PromptOutcome};
use crate::state::layout::SlotId;
use crate::state::{GuiState, Question};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use metafolder_core::sync::MutexExt;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error": message}))).into_response()
}

fn map_state_error(error: String) -> Response {
    let status = if error.starts_with("unknown workspace") || error.starts_with("no workspace") {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::BAD_REQUEST
    };
    error_response(status, &error)
}

/// The error is boxed because a `Response` is large relative to a `SlotId`.
fn parse_slot(slot: &str) -> Result<SlotId, Box<Response>> {
    match slot {
        "left" => Ok(SlotId::Left),
        "right" => Ok(SlotId::Right),
        other => Err(Box::new(error_response(
            StatusCode::BAD_REQUEST,
            &format!("unknown slot: {other} (left | right)"),
        ))),
    }
}

// ── Workspaces ────────────────────────────────────────────────────────────

pub async fn list_workspaces(State(state): State<ServerState>) -> Response {
    Json(state.gui.workspaces()).into_response()
}

#[derive(Deserialize, Default)]
pub struct CreateWorkspaceBody {
    #[serde(default)]
    active_repo: Option<String>,
    /// The run id of the script creating this workspace (`METAFOLDER_GUI_TASK`,
    /// sent by `mf gui workspace new`). The workspace then belongs to that
    /// script, which is how a script that opens two scratch workspaces keeps
    /// its question bar visible in both (spec-gui "Script session").
    #[serde(default)]
    task: Option<String>,
}

pub async fn create_workspace(
    State(state): State<ServerState>,
    body: Option<Json<CreateWorkspaceBody>>,
) -> Response {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let task = body.task;
    let mut active_repo = body.active_repo;
    // Resolve the repo's human name so the workspace is auto-named after it
    // (spec-gui "Workspace name"); best-effort, falls back to "Workspace N".
    let mut repo_name = None;
    if active_repo.is_none() {
        // Default to the daemon's first loaded repository.
        if let Ok(response) = state.daemon.request("GET", "/repos", None).await {
            active_repo = response.body[0]["repo_uuid"].as_str().map(str::to_string);
            repo_name = response.body[0]["name"].as_str().map(str::to_string);
        }
    } else if let Some(uuid) = &active_repo {
        repo_name = state.daemon.repo_name(uuid).await;
    }
    let id = state.gui.create_workspace_named(active_repo, repo_name);
    if let Some(task) = task {
        state.gui.script_claim_workspace(&task, &id);
    }
    Json(json!({ "id": id })).into_response()
}

pub async fn delete_workspace(
    State(state): State<ServerState>,
    Path(id): Path<String>,
) -> Response {
    match state.gui.close_workspace(&id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => map_state_error(error),
    }
}

// ── Layout ────────────────────────────────────────────────────────────────

pub async fn get_layout(State(state): State<ServerState>) -> Response {
    let layout = state.gui.layout();
    let slot = |payload: &crate::state::layout::SlotPayload| {
        if payload.visible {
            payload.workspace_id.clone().map(Value::from).unwrap_or(Value::Null)
        } else {
            Value::Null
        }
    };
    Json(json!({ "left": slot(&layout.left), "right": slot(&layout.right) })).into_response()
}

pub async fn put_layout(
    State(state): State<ServerState>,
    Json(body): Json<Map<String, Value>>,
) -> Response {
    // Only the keys present in the body are updated (partial update);
    // an explicit null hides the slot.
    for (key, slot_id) in [("left", SlotId::Left), ("right", SlotId::Right)] {
        match body.get(key) {
            None => {}
            Some(Value::Null) => state.gui.hide_slot(slot_id),
            Some(Value::String(ws_id)) => {
                if let Err(error) = state.gui.tab_assign(ws_id, slot_id) {
                    return map_state_error(error);
                }
            }
            Some(other) => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    &format!("'{key}' must be a workspace id or null, got {other}"),
                );
            }
        }
    }
    Json(json!({})).into_response()
}

// ── Panel views ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct PutViewBody {
    #[serde(rename = "type")]
    panel_type: String,
    #[serde(default)]
    path: Option<String>,
}

pub async fn put_panel_view(
    State(state): State<ServerState>,
    Path(slot): Path<String>,
    Json(body): Json<PutViewBody>,
) -> Response {
    let slot_id = match parse_slot(&slot) {
        Ok(slot_id) => slot_id,
        Err(response) => return *response,
    };

    // Show the slot first; an unassigned slot inherits the focused
    // slot's workspace (decision: the spec only says "shown first").
    let layout = state.gui.layout();
    let payload = match slot_id {
        SlotId::Left => &layout.left,
        SlotId::Right => &layout.right,
    };
    let ws_id = match &payload.workspace_id {
        Some(ws_id) => ws_id.clone(),
        None => match state.gui.focused_workspace_id() {
            Some(ws_id) => ws_id,
            None => return error_response(StatusCode::CONFLICT, "no workspace to assign"),
        },
    };
    if let Err(error) = state.gui.tab_assign(&ws_id, slot_id) {
        return map_state_error(error);
    }
    if let Err(error) = state.gui.set_panel_type(slot_id, &body.panel_type) {
        return map_state_error(error);
    }

    // type=file + path: select that file (spec-gui PUT /gui/panels).
    if body.panel_type == "file" {
        if let Some(path) = &body.path {
            if let Err(error) = state.gui.set_var(&ws_id, "selected_paths", json!([path])) {
                return map_state_error(error);
            }
            let entry = lookup_record_by_path(&state, &ws_id, path).await;
            if let Err(error) = state.gui.set_var(&ws_id, "selected_metarecord", entry) {
                return map_state_error(error);
            }
        }
    }
    Json(json!({})).into_response()
}

/// Best-effort: the metarecord whose `mfr_path` resolves to `path`
/// in the workspace's active repo; `Null` for untracked files.
async fn lookup_record_by_path(state: &ServerState, ws_id: &str, path: &str) -> Value {
    let Ok(Value::String(repo)) = state.gui.get_var(ws_id, "active_repo") else {
        return Value::Null;
    };
    // Repo root (for the repo-relative query path).
    let Ok(repos) = state.daemon.request("GET", "/repos", None).await else {
        return Value::Null;
    };
    let Some(root) = repos
        .body
        .as_array()
        .into_iter()
        .flatten()
        .find(|r| r["repo_uuid"] == repo.as_str())
        .and_then(|r| r["root"].as_str())
    else {
        return Value::Null;
    };
    let Some(relative) = path.strip_prefix(root).map(|p| p.trim_start_matches('/')) else {
        return Value::Null;
    };
    // Query in the daemon's tree convention (spec-query "Exact-node equality"):
    // the repository root is the EMPTY string and every descendant keeps a
    // single leading slash. `mfr_path = "<path>"` on a tree_ref field resolves
    // that exact node — the root's empty name matches "", a "/…" operand is
    // path-resolved. A top-level child must therefore be "/name", never a
    // `follows -> "/"` (whose "/" resolves to no node, since the root is "").
    // This mirrors the shell-side `mf_gui_query_path` helper.
    let tree_path = if relative.is_empty() { String::new() } else { format!("/{relative}") };
    let query = json!({
        "type": "eq",
        "field": "mfr_path",
        "value": {"type": "string", "value": tree_path},
    });
    match state
        .daemon
        .request("POST", &format!("/repos/{repo}/query"), Some(json!({"query": query})))
        .await
    {
        Ok(response) if response.status == 200 => match response.body.as_array() {
            Some(uuids) if !uuids.is_empty() => {
                json!({"uuid": uuids[0], "repo": repo})
            }
            _ => Value::Null,
        },
        _ => Value::Null,
    }
}

pub async fn get_panel_view(
    State(state): State<ServerState>,
    Path(slot): Path<String>,
) -> Response {
    let slot_id = match parse_slot(&slot) {
        Ok(slot_id) => slot_id,
        Err(response) => return *response,
    };
    let layout = state.gui.layout();
    let payload = match slot_id {
        SlotId::Left => &layout.left,
        SlotId::Right => &layout.right,
    };
    let (Some(ws_id), Some(panel_type)) = (&payload.workspace_id, &payload.panel_type) else {
        return error_response(StatusCode::NOT_FOUND, "no panel displayed in this slot");
    };
    let status = if state.gui.panel_ready(ws_id, panel_type) { "ready" } else { "loading" };
    Json(json!({ "type": panel_type, "status": status })).into_response()
}

// ── Command dispatch ──────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CommandBody {
    invocation: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

/// Runs an arbitrary command invocation through the frontend's own
/// `dispatch()` — the same path as the command input and keybindings, so
/// external and internal invocation stay in lockstep. Blocks until the
/// frontend reports the outcome via the `command_done` Tauri command, or
/// until the optional timeout elapses. Concurrent calls are allowed; each
/// gets its own id.
pub async fn post_command(
    State(state): State<ServerState>,
    body: Option<Json<CommandBody>>,
) -> Response {
    let Some(Json(body)) = body else {
        return error_response(StatusCode::BAD_REQUEST, "missing invocation");
    };
    if body.invocation.trim().is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "missing invocation");
    }

    let (id, receiver) = state.commands.begin();
    state.gui.notify(
        events::COMMAND_REQUESTED,
        json!({"invocation_id": id.simple().to_string(), "invocation": body.invocation}),
    );

    let outcome = match body.timeout_ms {
        Some(ms) => tokio::time::timeout(Duration::from_millis(ms), receiver)
            .await
            .ok()
            .and_then(Result::ok),
        None => receiver.await.ok(),
    };
    state.commands.end(id); // release on the timeout path

    let payload = match outcome {
        Some(CommandOutcome::Ok) => json!({"event": "ok"}),
        Some(CommandOutcome::Error(message)) => json!({"event": "error", "message": message}),
        Some(CommandOutcome::Closed) => json!({"event": "closed"}),
        None => json!({"event": "timeout"}),
    };
    Json(payload).into_response()
}

// ── Bench harness ─────────────────────────────────────────────────────────

/// Snapshots the recorded `performance.measure` (`mf:*`) entries reported by
/// the panels (spec-gui "Bench harness").
pub async fn get_bench(State(state): State<ServerState>) -> Response {
    Json(json!({"records": state.bench.snapshot()})).into_response()
}

/// Empties the bench buffer — a driver script calls this before each scenario.
pub async fn clear_bench(State(state): State<ServerState>) -> Response {
    state.bench.clear();
    Json(json!({})).into_response()
}

// ── Messages ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct MessageBody {
    text: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

pub async fn post_message(
    State(state): State<ServerState>,
    Query(params): Query<HashMap<String, String>>,
    Json(body): Json<MessageBody>,
) -> Response {
    let ws_id = match params.get("workspace_id") {
        Some(ws_id) => ws_id.clone(),
        None => match state.gui.focused_workspace_id() {
            Some(ws_id) => ws_id,
            None => return error_response(StatusCode::CONFLICT, "no focused workspace"),
        },
    };
    match state.gui.post_status(&ws_id, &body.text, "info", body.timeout_ms) {
        Ok(()) => Json(json!({})).into_response(),
        Err(error) => map_state_error(error),
    }
}

// ── Script progress ───────────────────────────────────────────────────────

/// `POST /gui/progress` — a running script updates its own task bar entry
/// (spec-gui "Scripting"). `task` is the run id the GUI injected as
/// `METAFOLDER_GUI_TASK`; an absent or unknown one is a lenient no-op, so a
/// script run outside the GUI never fails on it.
#[derive(Deserialize, Default)]
pub struct ProgressBody {
    #[serde(default)]
    task: Option<String>,
    #[serde(default)]
    done: Option<u64>,
    #[serde(default)]
    total: Option<u64>,
    #[serde(default)]
    phase: Option<String>,
}

pub async fn post_progress(
    State(state): State<ServerState>,
    body: Option<Json<ProgressBody>>,
) -> Response {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    if let Some(task) = body.task.as_deref() {
        state.gui.script_progress(task, body.done, body.total, body.phase);
    }
    Json(json!({})).into_response()
}

// ── Input and prompt waits ────────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct InputBody {
    #[serde(default)]
    keys: Vec<String>,
    /// The asking script's run id (`METAFOLDER_GUI_TASK`). It scopes the
    /// question to the workspaces that script owns and marks its task-bar entry
    /// "awaiting an answer" rather than "working".
    #[serde(default)]
    task: Option<String>,
    /// Question shown while the wait is active, in a dedicated bar separate
    /// from the status/error line (spec-gui "Scripting"). Optional.
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

/// Pushes the compiled table — plus the temporary `answer:send` bindings, while
/// the script keys are enabled — and broadcasts the live question so the
/// frontend can show its dedicated bar with the right checkbox state.
///
/// Both are derived from `GuiState`, not from arguments, so flipping the
/// checkbox (`script-keys:toggle`) re-pushes exactly the same question with or
/// without its answer bindings: a disabled key must not stay bound as a
/// fallback, or the panel's own `y` would still lose to the script's.
pub fn push_keytable(gui: &GuiState, keybindings: &Mutex<KeybindingSet>) {
    let question = gui.question();
    let enabled = gui.script_keys_enabled();
    let mut bindings: Vec<CompiledBinding> = keybindings.lock_recover().compiled();
    if enabled {
        for key in question.iter().flat_map(|q| &q.keys) {
            if let Ok(keys) = crate::keybindings::parse_combo(key) {
                bindings.push(CompiledBinding {
                    keys,
                    invocation: format!("answer:send {key}"),
                    when: None,
                    text_input: false,
                    focus: None,
                });
            }
        }
    }
    gui.notify(events::KEYBINDINGS_CHANGED, json!({ "bindings": bindings }));
    gui.notify(
        events::INPUT_WAIT_CHANGED,
        json!({ "active": question.is_some(),
                "temp_keys": question.as_ref().map(|q| q.keys.clone()).unwrap_or_default(),
                "prompt": question.as_ref().and_then(|q| q.prompt.clone()),
                "workspaces": question.as_ref().map(|q| q.workspaces.clone()).unwrap_or_default(),
                "task": question.as_ref().and_then(|q| q.task.clone()),
                "script_keys": enabled }),
    );
}

/// The workspaces the asking script owns, for scoping its question bar. Empty
/// for a wait with no run id (or one the GUI never launched): such a wait
/// belongs to nobody and is always shown.
fn script_workspaces(state: &ServerState, task: Option<&str>) -> Vec<String> {
    task.map(|task| state.gui.script_workspaces(task)).unwrap_or_default()
}

/// Undoes everything a wait installs, on *every* exit path — a resolved answer,
/// a timeout, and crucially a cancelled handler future.
///
/// The last one is why this is a guard and not a tail of statements: when the
/// HTTP client goes away (a script killed mid-question), axum drops the handler
/// future where it is suspended. Releasing the lock only after the `await`
/// meant it stayed held forever, so every later wait answered 409 for the rest
/// of the GUI's life — the next script appeared to "just stop" at its first
/// question, with nothing on screen to say why (spec-gui "Script session").
struct WaitGuard<'a> {
    state: &'a ServerState,
    /// The asking script's run id, marked "awaiting an answer" meanwhile.
    task: Option<String>,
    /// Input waits install temporary answer bindings + a question bar to
    /// remove; prompts install neither.
    keytable: bool,
}

impl<'a> WaitGuard<'a> {
    fn begin(state: &'a ServerState, task: Option<String>, keytable: bool) -> Self {
        if let Some(task) = &task {
            state.gui.script_waiting(task, true);
        }
        Self { state, task, keytable }
    }
}

impl Drop for WaitGuard<'_> {
    fn drop(&mut self) {
        self.state.input.end();
        if self.keytable {
            self.state.gui.set_question(None);
            push_keytable(&self.state.gui, &self.state.keybindings);
        }
        if let Some(task) = &self.task {
            self.state.gui.script_waiting(task, false);
        }
    }
}

pub async fn post_input(
    State(state): State<ServerState>,
    body: Option<Json<InputBody>>,
) -> Response {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    // The user's ways out of the question are not a script's to take (spec-gui
    // "Reserved keys"). Checked before the lock, so a refused wait leaves the
    // next script free to ask.
    let reserved =
        crate::keybindings::reserved_combos(&state.keybindings.lock_recover().compiled());
    if let Some(key) = body.keys.iter().find(|key| crate::keybindings::is_reserved(&reserved, key))
    {
        return error_response(
            StatusCode::BAD_REQUEST,
            &format!("'{key}' is reserved by the GUI and cannot be awaited by a script"),
        );
    }
    let Some(receiver) = state.input.begin_input() else {
        return error_response(StatusCode::CONFLICT, "another input wait is active");
    };
    state.gui.set_question(Some(Question {
        keys: body.keys.clone(),
        prompt: body.prompt.clone(),
        workspaces: script_workspaces(&state, body.task.as_deref()),
        task: body.task.clone(),
    }));
    push_keytable(&state.gui, &state.keybindings);
    let _guard = WaitGuard::begin(&state, body.task.clone(), true);

    let outcome = match body.timeout_ms {
        Some(ms) => tokio::time::timeout(Duration::from_millis(ms), receiver)
            .await
            .ok()
            .and_then(Result::ok),
        None => receiver.await.ok(),
    };
    let payload = match outcome {
        Some(InputOutcome::Answer(value)) => json!({"event": "answer", "value": value}),
        Some(InputOutcome::Closed) => json!({"event": "closed"}),
        None => json!({"event": "timeout"}),
    };
    Json(payload).into_response()
}

#[derive(Deserialize)]
pub struct PromptBody {
    prompt: String,
    /// The asking script's run id — see [`InputBody::task`].
    #[serde(default)]
    task: Option<String>,
    /// Values offered by the command input autocomplete during the prompt.
    #[serde(default)]
    completions: Vec<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

pub async fn post_prompt(
    State(state): State<ServerState>,
    Json(body): Json<PromptBody>,
) -> Response {
    let Some(receiver) = state.input.begin_prompt() else {
        return error_response(StatusCode::CONFLICT, "another input wait is active");
    };
    state.gui.notify(
        events::PROMPT_REQUESTED,
        json!({ "prompt": body.prompt, "completions": body.completions }),
    );
    let _guard = WaitGuard::begin(&state, body.task.clone(), false);

    let outcome = match body.timeout_ms {
        Some(ms) => tokio::time::timeout(Duration::from_millis(ms), receiver)
            .await
            .ok()
            .and_then(Result::ok),
        None => receiver.await.ok(),
    };

    let payload = match outcome {
        Some(PromptOutcome::Confirm(text)) => json!({"event": "confirm", "text": text}),
        Some(PromptOutcome::Cancel) => json!({"event": "cancel"}),
        Some(PromptOutcome::Closed) => json!({"event": "closed"}),
        None => json!({"event": "timeout"}),
    };
    Json(payload).into_response()
}

// ── Status ────────────────────────────────────────────────────────────────

pub async fn get_status(State(state): State<ServerState>) -> Response {
    let layout = state.gui.layout();
    let slot = |payload: &crate::state::layout::SlotPayload, focused: bool| {
        if !payload.visible || payload.workspace_id.is_none() {
            return Value::Null;
        }
        json!({
            "workspace_id": payload.workspace_id,
            "panel_type": payload.panel_type,
            "focused": focused,
        })
    };
    Json(json!({
        "workspaces": state.gui.workspaces(),
        "layout": {
            "left": slot(&layout.left, layout.focused == SlotId::Left),
            "right": slot(&layout.right, layout.focused == SlotId::Right),
        },
        "daemon_connected": state.daemon.last_connected().unwrap_or(false),
        "input_wait_active": state.input.is_active(),
    }))
    .into_response()
}
