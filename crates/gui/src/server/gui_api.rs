//! `/gui/*` scripting endpoints (spec-gui "Scripting / GUI API").

use super::ServerState;
use crate::events;
use crate::keybindings::CompiledBinding;
use crate::server::input_wait::{InputOutcome, PromptOutcome};
use crate::state::layout::SlotId;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
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
}

pub async fn create_workspace(
    State(state): State<ServerState>,
    body: Option<Json<CreateWorkspaceBody>>,
) -> Response {
    let mut active_repo = body.and_then(|Json(b)| b.active_repo);
    if active_repo.is_none() {
        // Default to the daemon's first loaded repository.
        if let Ok(response) = state.daemon.request("GET", "/repos", None).await {
            active_repo = response.body[0]["repo_uuid"].as_str().map(str::to_string);
        }
    }
    let id = state.gui.create_workspace(active_repo);
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
    let (parent, name) = match relative.rsplit_once('/') {
        Some((parent, name)) => (format!("/{parent}"), name),
        None => ("/".to_string(), relative),
    };
    let query = json!({
        "type": "and",
        "operands": [
            {"type": "follows", "field": "mfr_path", "target": parent},
            {"type": "matches", "field": "mfr_path",
             "pattern": format!("^{}$", regex_escape(name))},
        ],
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

fn regex_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        if "\\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
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
    let status = if state.gui.panel_ready(ws_id, panel_type) {
        "ready"
    } else {
        "loading"
    };
    Json(json!({ "type": panel_type, "status": status })).into_response()
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

// ── Input and prompt waits ────────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct InputBody {
    #[serde(default)]
    keys: Vec<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

/// Pushes the compiled table plus the temporary `answer:send` bindings.
fn push_keytable(state: &ServerState, temp_keys: &[String]) {
    let mut bindings: Vec<CompiledBinding> = state.keybindings.lock().unwrap().compiled();
    for key in temp_keys {
        if let Ok(keys) = crate::keybindings::parse_combo(key) {
            bindings.push(CompiledBinding {
                keys,
                invocation: format!("answer:send {key}"),
                when: None,
                text_input: false,
            });
        }
    }
    state
        .gui
        .notify(events::KEYBINDINGS_CHANGED, json!({ "bindings": bindings }));
    state.gui.notify(
        events::INPUT_WAIT_CHANGED,
        json!({ "active": !temp_keys.is_empty() || state.input.is_active(),
                "temp_keys": temp_keys }),
    );
}

pub async fn post_input(
    State(state): State<ServerState>,
    body: Option<Json<InputBody>>,
) -> Response {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let Some(receiver) = state.input.begin_input() else {
        return error_response(StatusCode::CONFLICT, "another input wait is active");
    };
    push_keytable(&state, &body.keys);

    let outcome = match body.timeout_ms {
        Some(ms) => tokio::time::timeout(Duration::from_millis(ms), receiver)
            .await
            .ok()
            .and_then(Result::ok),
        None => receiver.await.ok(),
    };
    state.input.end(); // release the lock on timeout paths
    push_keytable(&state, &[]); // remove temporary bindings

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

    let outcome = match body.timeout_ms {
        Some(ms) => tokio::time::timeout(Duration::from_millis(ms), receiver)
            .await
            .ok()
            .and_then(Result::ok),
        None => receiver.await.ok(),
    };
    state.input.end();

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
