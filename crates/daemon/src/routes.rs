//! Axum route handlers. Blocking SQLite work is dispatched through
//! `tokio::task::spawn_blocking`; every error is rendered as the JSON
//! `{"error": ...}` shape via [`ApiError`].

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use metafolder_core::metarecord::{Field, MetaRecord, FieldType, Value, ZERO_UUID};
use metafolder_core::sync::MutexExt;

use metafolder_core::query::Query as MetaQuery;

use crate::db;
use crate::error::ApiError;
use crate::log::Writer;
use crate::pagination::Page;
use crate::query_exec::{self, SortKey};
use crate::repo::RepoLocator;
use crate::reserved;
use crate::state::{AppState, RepoState, RollbackLock};
use crate::tasks::TaskKind;

pub fn build(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/tasks", get(list_all_tasks))
        .route("/repos", get(list_repos))
        .route("/repos/init", post(init_repo))
        .route("/repos/load", post(load_repo))
        .route("/repos/:repo", get(get_repo).patch(rename_repo))
        .route("/repos/:repo/unload", post(unload_repo))
        // ── Resource layer (single, directly-addressed) ──────────────────────
        .route("/repos/:repo/metarecords", post(create_record_endpoint))
        .route(
            "/repos/:repo/metarecords/:uuid",
            get(get_record_endpoint).put(put_metarecord).delete(delete_record_endpoint),
        )
        .route("/repos/:repo/metarecords/:uuid/fields", post(append_field))
        .route(
            "/repos/:repo/metarecords/:uuid/fields/:name",
            get(get_record_field).put(set_record_field).delete(unset_record_field),
        )
        .route(
            "/repos/:repo/metarecords/:uuid/fields/:name/resolve-tree",
            get(resolve_record_field_tree),
        )
        .route(
            "/repos/:repo/fields/:id",
            get(get_field_by_id).patch(patch_field_by_id).delete(delete_field_by_id),
        )
        .route("/repos/:repo/retype", post(retype_field))
        .route("/repos/:repo/fields", get(list_fields))
        .route("/repos/:repo/tree/roots", get(tree_roots))
        // ── Set layer (by predicate) ─────────────────────────────────────────
        .route("/repos/:repo/query", post(run_query))
        .route("/repos/:repo/query/delete", post(delete_by_query))
        .route("/repos/:repo/query/fields/set", post(batch_set))
        .route("/repos/:repo/query/fields/append", post(batch_append))
        .route("/repos/:repo/query/fields/remove", post(batch_remove))
        .route("/repos/:repo/query/fields/unset", post(batch_unset))
        .route("/repos/:repo/query/fields/resolve-tree", post(query_resolve_tree))
        .route("/repos/:repo/log", get(get_log))
        .route("/repos/:repo/log/since", get(get_log_since))
        .route(
            "/repos/:repo/log/revisions/:rev_id",
            get(get_revision).patch(patch_revision),
        )
        .route("/repos/:repo/log/prune", post(prune_log))
        .route("/repos/:repo/rollback", post(rollback))
        .route("/repos/:repo/rollback/plan", get(rollback_plan))
        .route("/repos/:repo/rollback/plan/summary", get(rollback_plan_summary))
        .route("/repos/:repo/rollback/start", post(rollback_start))
        .route("/repos/:repo/rollback/step", post(rollback_step))
        .route("/repos/:repo/rollback/abort", post(rollback_abort))
        .route("/repos/:repo/schema", get(get_schema))
        .route("/repos/:repo/schema/reload", post(reload_schema))
        .route("/repos/:repo/schema/check", post(check_schema))
        .route("/repos/:repo/tasks", get(list_repo_tasks))
        .route("/repos/:repo/tasks/:task", get(get_task))
        .route("/repos/:repo/tasks/:task/cancel", post(cancel_task))
        .route("/repos/:repo/reconcile", post(full_reconcile))
        .route("/repos/:repo/track", post(track))
        .with_state(state)
}

/// The router with the session-token authentication layer (spec-auth): every
/// request must carry `Authorization: Bearer <token>`. Used by the daemon
/// binary; tests drive [`build`] directly (no network, no token).
pub fn build_authenticated(state: Arc<AppState>, token: Arc<str>) -> Router {
    build(state).layer(axum::middleware::from_fn_with_state(token, require_token))
}

/// Rejects requests whose bearer token does not match (constant-time).
async fn require_token(
    State(token): State<Arc<str>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let provided = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    let authorized = metafolder_core::auth::bearer_token(provided)
        .map(|t| metafolder_core::auth::constant_time_eq(t, &token))
        .unwrap_or(false);
    if authorized {
        next.run(request).await
    } else {
        ApiError::unauthorized("missing or invalid session token").into_response()
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn parse_uuid(s: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(s).map_err(|_| ApiError::bad_request(format!("invalid UUID: '{s}'")))
}

fn hex(uuid: Uuid) -> String {
    uuid.as_simple().to_string()
}

/// Runs blocking repository work on the blocking thread pool.
async fn with_repo<T, F>(state: &AppState, repo_uuid: Uuid, f: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce(&RepoState) -> Result<T, ApiError> + Send + 'static,
{
    let repo = state.repo(repo_uuid)?;
    tokio::task::spawn_blocking(move || f(&repo))
        .await
        .map_err(|e| ApiError::internal(format!("blocking task failed: {e}")))?
}

#[derive(Deserialize)]
struct QueryResolveTreeBody {
    query: MetaQuery,
    #[serde(default = "default_tree_field")]
    field: String,
}

fn default_tree_field() -> String {
    "mfr_path".to_string()
}

/// `POST /repos/:repo/query/fields/resolve-tree`: resolves the TreeRef `field`
/// (default `mfr_path`) of every metarecord matching `query` to repo-root-
/// relative paths. A field is a multi-map, so each metarecord maps to an array
/// of paths (stale positions skipped). Resolution uses the in-memory tree cache
/// — one round-trip whatever the depth. (Target an explicit set with a
/// `uuid_in` query.)
async fn query_resolve_tree(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<QueryResolveTreeBody>, JsonRejection>,
) -> Result<Response, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let Json(body) = payload?;
    let field = body.field;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = repo_state.conn.lock_recover();
        let mut cache = repo_state.lock_cache();
        let (uuids, _) =
            query_exec::execute(&conn, &mut cache, repo_uuid, &body.query, &[], None, None)?;
        let mut out = serde_json::Map::new();
        for uuid in uuids {
            let paths = cache.paths_of(&conn, &field, uuid)?;
            out.insert(hex(uuid), json!(paths));
        }
        Ok(Json(serde_json::Value::Object(out)).into_response())
    })
    .await
}

/// `GET /repos/:repo/metarecords/:uuid/fields/:name/resolve-tree`: the direct
/// (single-metarecord) form of `resolve-tree`.
async fn resolve_record_field_tree(
    State(state): State<Arc<AppState>>,
    Path((repo, uuid, name)): Path<(String, String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let uuid = parse_uuid(&uuid)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = repo_state.conn.lock_recover();
        let mut cache = repo_state.lock_cache();
        let paths = cache.paths_of(&conn, &name, uuid)?;
        Ok(Json(json!({ "paths": paths })))
    })
    .await
}

#[derive(Deserialize)]
struct ListFieldsParams {
    /// Optional value-type filter (e.g. `tree_ref`, `ref`); absent = all types.
    #[serde(rename = "type")]
    type_filter: Option<String>,
}

/// `GET /repos/:repo/fields[?type=<value_type>]`: the distinct field names
/// known to the repository, each with its value type — the data-derived catalog
/// (field names present on metarecords, `Nothing` excluded) merged with the
/// schema's declared field types (schema-priority on conflict; schema-only
/// fields, e.g. `path: tree_ref` declared but not yet carried, are included).
/// With `?type=`, only that value type is returned (e.g. `tree_ref` to populate
/// a picker), applied after the merge. Response is a JSON array
/// `[{"name": ..., "type": ...}, ...]` ordered by name.
async fn list_fields(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    Query(params): Query<ListFieldsParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        // The data-derived catalog comes from the in-memory index (built at
        // load, refreshed to HEAD) — its `present`/`types` maps already hold
        // every distinct field name and value type, no DB scan. Mirrors
        // `run_query_filter`'s index acquisition (conn first, then the index).
        let conn = repo_state.conn.lock_recover();
        // Extract the schema's declared types into an owned Vec, releasing the
        // schema lock before taking the index lock (never hold both).
        let schema_decls = repo_state
            .schema
            .lock_recover()
            .as_ref()
            .map(|s| s.declared_types())
            .unwrap_or_default();
        let mut index_guard = repo_state.index.lock_recover();
        match index_guard.as_mut() {
            Some(index) => index.refresh(&conn, repo_uuid)?,
            None => *index_guard = Some(crate::index::RepoIndex::build(&conn, repo_uuid)?),
        }
        let data = index_guard.as_ref().expect("index built above").field_catalog(None);
        drop(index_guard);
        // Merge in the schema (schema-priority, schema-only fields added), then
        // apply the `?type=` filter (so a schema-only field of that type shows).
        let names =
            crate::schema::merge_field_catalog(data, schema_decls, params.type_filter.as_deref());
        let out: Vec<serde_json::Value> =
            names.into_iter().map(|(name, ty)| json!({"name": name, "type": ty})).collect();
        Ok(Json(serde_json::Value::Array(out)))
    })
    .await
}

#[derive(Deserialize)]
struct TreeRootsParams {
    #[serde(default = "default_tree_field")]
    field: String,
}

/// `GET /repos/:repo/tree/roots?field=<field>`: the forest roots of a TreeRef
/// field — the nodes whose direct parent is the root sentinel (no parent).
/// Response `[{"uuid": "<hex>", "name": "<name>"}, ...]`, ordered by name. This
/// is the entry point for navigating a forest top-down (the empty path the
/// query DSL resolves to the sentinel matches the *children* of the named root,
/// not the roots themselves, and only when a root is literally named ""). The
/// tree-explorer panel starts here.
async fn tree_roots(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    Query(params): Query<TreeRootsParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = repo_state.conn.lock_recover();
        // Roots are stored with `value_uuid = ZERO_UUID` (the sentinel).
        let mut roots = db::tree_children(&conn, &params.field, ZERO_UUID)?;
        roots.sort_by(|a, b| a.1.cmp(&b.1));
        let out: Vec<serde_json::Value> =
            roots.into_iter().map(|(uuid, name)| json!({"uuid": hex(uuid), "name": name})).collect();
        Ok(Json(serde_json::Value::Array(out)))
    })
    .await
}

/// Fetches the full metadata object of a metarecord, or 404.
fn metarecord_response(conn: &rusqlite::Connection, uuid: Uuid) -> Result<MetaRecord, ApiError> {
    db::get_metarecord(conn, uuid)?
        .ok_or_else(|| ApiError::not_found(format!("Metarecord not found: {uuid}")))
}

fn check_writable(name: &str, force: bool) -> Result<(), ApiError> {
    reserved::check_writable(name, force).map_err(ApiError::bad_request)
}

/// Delta validation against the user schema: called after applying a user
/// write (inside the transaction), with the touched field names. On
/// violation the caller drops the Writer, rolling the whole write back.
fn validate_schema(
    repo_state: &RepoState,
    conn: &rusqlite::Connection,
    uuid: Uuid,
    touched: &[String],
) -> Result<(), ApiError> {
    let guard = repo_state.schema.lock_recover();
    let Some(schema) = guard.as_ref() else {
        return Ok(());
    };
    let violations = crate::schema::validate_entry_fields(schema, conn, uuid, touched)?;
    if violations.is_empty() {
        Ok(())
    } else {
        Err(crate::schema::violation_error(violations))
    }
}

/// Shared scaffold for the single-metarecord write handlers (`patch`,
/// `append`, `replace`, `delete`): runs on the blocking pool, gates on
/// repository writability, opens a logged [`Writer`], lets `write` resolve the
/// touched field name(s) and perform the mutation, then runs schema delta
/// validation over those names and commits. Returns the resulting metarecord
/// (handlers that answer 204 simply discard it). A validation failure or any
/// closure error drops the Writer, rolling the whole write back.
async fn write_record<F>(
    state: &AppState,
    repo_uuid: Uuid,
    uuid: Uuid,
    write: F,
) -> Result<MetaRecord, ApiError>
where
    F: FnOnce(&mut Writer) -> Result<Vec<String>, ApiError> + Send + 'static,
{
    with_repo(state, repo_uuid, move |repo_state| {
        repo_state.ensure_writable()?;
        let mut conn = repo_state.conn.lock_recover();
        let mut writer = Writer::begin(&mut conn, repo_uuid, None)?;
        let touched = write(&mut writer)?;
        validate_schema(repo_state, writer.connection(), uuid, &touched)?;
        let tree_touched = writer.touched_tree();
        writer.commit()?;
        // Manual TreeRef writes bypass the watcher's incremental cache upkeep;
        // rebuild the complete cache so reads stay correct (no-op if absent).
        if tree_touched {
            repo_state.lock_cache().populate(&conn)?;
        }
        metarecord_response(&conn, uuid)
    })
    .await
}

/// 404 unless the metarecord exists. Shared by the write handlers that target
/// a metarecord by uuid rather than by an existing field row.
fn ensure_exists(conn: &rusqlite::Connection, uuid: Uuid) -> Result<(), ApiError> {
    if db::get_version(conn, uuid)?.is_none() {
        return Err(ApiError::not_found(format!("Metarecord not found: {uuid}")));
    }
    Ok(())
}

// ── Health and repositories ───────────────────────────────────────────────────

async fn health(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "repos": state.list_repos().len(),
    }))
}

async fn list_repos(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::to_value(state.list_repos()).expect("repo list serialization"))
}

/// `GET /repos/:repo` — one loaded repository's info (404 if not loaded).
async fn get_repo(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let info = state.repo_info(repo_uuid)?;
    Ok(Json(serde_json::to_value(info).expect("repo info serialization")))
}

#[derive(Deserialize)]
struct RenameBody {
    name: String,
}

/// `PATCH /repos/:repo` — rename a loaded repository (409 on name clash,
/// persisted to config.json).
async fn rename_repo(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<RenameBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    let name = body.name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("repository name must not be empty"));
    }
    let info = state.rename_repo(repo_uuid, name)?;
    Ok(Json(serde_json::to_value(info).expect("repo info serialization")))
}

/// `GET /tasks`: every task across all loaded repositories (spec-tasks).
async fn list_all_tasks(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::to_value(state.all_tasks()).expect("tasks serialization"))
}

/// `GET /repos/:repo/tasks`: the repository's currently retained tasks.
async fn list_repo_tasks(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo = state.repo(parse_uuid(&repo)?)?;
    Ok(Json(serde_json::to_value(repo.tasks.list()).expect("tasks serialization")))
}

/// `GET /repos/:repo/tasks/:task`: one task by id (404 if unknown or evicted).
async fn get_task(
    State(state): State<Arc<AppState>>,
    Path((repo, task)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let task_uuid = parse_uuid(&task)?;
    let repo = state.repo(parse_uuid(&repo)?)?;
    repo.tasks
        .get(task_uuid)
        .map(|t| Json(serde_json::to_value(t).expect("task serialization")))
        .ok_or_else(|| ApiError::not_found(format!("Task not found: {task_uuid}")))
}

/// `POST /repos/:repo/tasks/:task/cancel`: requests cancellation of a task
/// (spec-tasks "Cancellation"). A `reconcile` is stopped cooperatively (it rolls
/// its transaction back); a running `query` is interrupted via SQLite. The task
/// transitions to `cancelled` once its worker unwinds; this returns the task's
/// current view. `flush` is not cancellable (400); a terminal task is a 409;
/// an unknown id a 404.
async fn cancel_task(
    State(state): State<Arc<AppState>>,
    Path((repo, task)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    use crate::tasks::CancelOutcome;
    let task_uuid = parse_uuid(&task)?;
    let repo = state.repo(parse_uuid(&repo)?)?;
    match repo.tasks.request_cancel(task_uuid) {
        CancelOutcome::Requested => repo
            .tasks
            .get(task_uuid)
            .map(|t| Json(serde_json::to_value(t).expect("task serialization")))
            .ok_or_else(|| ApiError::not_found(format!("Task not found: {task_uuid}"))),
        CancelOutcome::AlreadyTerminal => {
            Err(ApiError::conflict(format!("Task already finished: {task_uuid}")))
        }
        CancelOutcome::NotCancellable => {
            Err(ApiError::bad_request("this kind of task cannot be cancelled"))
        }
        CancelOutcome::NotFound => {
            Err(ApiError::not_found(format!("Task not found: {task_uuid}")))
        }
    }
}

#[derive(Deserialize)]
struct InitBody {
    root: PathBuf,
    #[serde(default)]
    metafolder: Option<PathBuf>,
    #[serde(default)]
    name: Option<String>,
}

async fn init_repo(
    State(state): State<Arc<AppState>>,
    payload: Result<Json<InitBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(body) = payload?;
    // An empty/whitespace name falls back to the directory-derived default.
    let name = body.name.filter(|n| !n.trim().is_empty());
    let uuid = tokio::task::spawn_blocking(move || {
        state.init_repo(&body.root, body.metafolder.as_deref(), name.as_deref())
    })
    .await
    .map_err(|e| ApiError::internal(format!("blocking task failed: {e}")))??;
    Ok(Json(json!({"repo_uuid": hex(uuid)})))
}

#[derive(Deserialize)]
struct LoadBody {
    #[serde(default)]
    root: Option<PathBuf>,
    #[serde(default)]
    metafolder: Option<PathBuf>,
}

async fn load_repo(
    State(state): State<Arc<AppState>>,
    payload: Result<Json<LoadBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(body) = payload?;
    let locator = match (body.root, body.metafolder) {
        (Some(root), None) => RepoLocator::Root(root),
        (None, Some(dir)) => RepoLocator::Metafolder(dir),
        _ => {
            return Err(ApiError::bad_request(
                "exactly one of 'root' or 'metafolder' must be provided",
            ))
        }
    };
    let state_for_warmup = state.clone();
    let uuid = tokio::task::spawn_blocking(move || state.load_repo(locator))
        .await
        .map_err(|e| ApiError::internal(format!("blocking task failed: {e}")))??;
    // Warm the repository (tree cache + query index) in the background, as an
    // observable `load` task so the GUI shows a progress bar (spec-tasks). The
    // repository is already loaded and answers queries meanwhile (via the DB
    // fallback); the response returns its uuid immediately, unchanged.
    spawn_load_warmup(state_for_warmup, uuid);
    Ok(Json(json!({"repo_uuid": hex(uuid)})))
}

/// Spawns the background warmup task for a freshly loaded repository. A no-op
/// when the repository is already warm (a redundant load) or a warmup is
/// already running.
fn spawn_load_warmup(state: Arc<AppState>, repo_uuid: Uuid) {
    let Ok(repo_state) = state.repo(repo_uuid) else { return };
    if repo_state.lock_cache().is_complete() {
        return; // already warm (e.g. re-load of a loaded repo)
    }
    let Some(task_id) = repo_state.tasks.start_unique(TaskKind::Load) else {
        return; // a warmup is already in progress
    };
    tokio::task::spawn_blocking(move || {
        repo_state.tasks.mark_running(task_id);
        repo_state.warmup(&|phase, done, total| {
            repo_state.tasks.set_progress(task_id, phase, done, total);
        });
        repo_state.tasks.finish(task_id, None);
    });
}

/// `POST /repos/:repo/unload`: stops the repository's watcher/executor and
/// releases its database lock, removing it from the loaded set (spec-main
/// "Repository management"). 404 if not loaded; 409 if a rollback navigation is
/// in progress. Runs on a blocking thread because dropping the state joins the
/// executor thread.
async fn unload_repo(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    tokio::task::spawn_blocking(move || state.unload_repo(repo_uuid))
        .await
        .map_err(|e| ApiError::internal(format!("blocking task failed: {e}")))??;
    Ok(Json(json!({"repo_uuid": hex(repo_uuid)})))
}

// ── MetaRecord CRUD ─────────────────────────────────────────────────────────────

// ── Event log and rollback ────────────────────────────────────────────────────

/// Serializes one operation row, optionally with its snapshots.
fn op_json(
    conn: &rusqlite::Connection,
    op: &crate::log::OpRow,
    include_snapshots: bool,
) -> Result<serde_json::Value, ApiError> {
    let mut value = json!({
        "id": op.id,
        "parent_id": op.parent_id,
        "rev_id": op.rev_id,
        "seq": op.seq,
        "op_type": op.op_type,
        "entity_uuid": hex(op.entity_uuid),
        "field_name": op.field_name,
    });
    if include_snapshots {
        value["snapshots_before"] = snapshots_json(conn, op.id, 0)?;
        value["snapshots_after"] = snapshots_json(conn, op.id, 1)?;
    }
    Ok(value)
}

/// Snapshot rows in their raw column form (spec-event-log examples).
fn snapshots_json(
    conn: &rusqlite::Connection,
    op_id: i64,
    is_new: i64,
) -> Result<serde_json::Value, ApiError> {
    let blob_hex = |b: Vec<u8>| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    let mut out = Vec::new();
    for row in crate::log::snapshots(conn, op_id, is_new)? {
        // Raw column form (spec-event-log examples), null columns omitted.
        let encoded = db::encode_value(&row.value);
        let mut snapshot = json!({
            "field_id": row.id,
            "field_name": row.name,
            "value_type": encoded.value_type,
        });
        if let Some(text) = encoded.text {
            snapshot["value_text"] = json!(text);
        }
        if let Some(int) = encoded.int {
            snapshot["value_int"] = json!(int);
        }
        if let Some(real) = encoded.real {
            snapshot["value_real"] = json!(real);
        }
        if let Some(uuid) = encoded.uuid {
            snapshot["value_uuid"] = json!(blob_hex(uuid));
        }
        if let Some(repo) = encoded.ref_repo {
            snapshot["value_ref_repo"] = json!(blob_hex(repo));
        }
        if let Some(name) = encoded.name {
            snapshot["value_name"] = json!(name);
        }
        out.push(snapshot);
    }
    Ok(serde_json::Value::Array(out))
}

fn revision_json(conn: &rusqlite::Connection, rev_id: i64) -> Result<serde_json::Value, ApiError> {
    use rusqlite::OptionalExtension as _;
    let row: Option<(i64, Option<String>)> = conn
        .query_row(
            "SELECT timestamp, label FROM revision WHERE id = ?1",
            rusqlite::params![rev_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(anyhow::Error::from)?;
    let (timestamp, label) =
        row.ok_or_else(|| ApiError::not_found(format!("revision {rev_id} not found")))?;
    Ok(json!({"id": rev_id, "timestamp": timestamp, "label": label}))
}

#[derive(Deserialize)]
struct LogParams {
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    metarecord_uuid: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    since: Option<i64>,
    #[serde(default)]
    until: Option<i64>,
    #[serde(default)]
    include_snapshots: Option<bool>,
}

async fn get_log(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    Query(params): Query<LogParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let entity_filter = params.metarecord_uuid.as_deref().map(parse_uuid).transpose()?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = repo_state.conn.lock_recover();
        let head = crate::log::get_head(&conn)?;
        let mode = params.mode.as_deref().unwrap_or("linear");

        let mut ops: Vec<crate::log::OpRow> = match mode {
            "tree" => crate::log::all_ops(&conn)?,
            "linear" => match head {
                None => vec![],
                Some(head) => {
                    let mut chain = crate::log::ancestry_ops(&conn, head)?;
                    chain.reverse(); // root → HEAD, oldest first
                    chain
                }
            },
            // The active line through HEAD: ancestry plus the forward
            // continuation to the most-recent leaf (keeps the redo future
            // visible, hides divergent branches).
            "active" => match head {
                None => vec![],
                Some(head) => crate::log::active_line_ops(&conn, head)?,
            },
            other => {
                return Err(ApiError::bad_request(format!(
                    "invalid mode '{other}' (expected 'linear', 'active' or 'tree')"
                )))
            }
        };

        // Revision timestamps, for since/until filtering.
        let mut rev_meta: std::collections::HashMap<i64, (i64, Option<String>)> =
            std::collections::HashMap::new();
        {
            let mut stmt = conn
                .prepare("SELECT id, timestamp, label FROM revision")
                .map_err(anyhow::Error::from)?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get(1)?, r.get(2)?)))
                .map_err(anyhow::Error::from)?;
            for row in rows {
                let (id, ts, label) = row.map_err(anyhow::Error::from)?;
                rev_meta.insert(id, (ts, label));
            }
        }

        ops.retain(|op| {
            if let Some(filter) = entity_filter {
                if op.entity_uuid != filter {
                    return false;
                }
            }
            let ts = rev_meta.get(&op.rev_id).map(|(ts, _)| *ts).unwrap_or(0);
            params.since.is_none_or(|s| ts >= s) && params.until.is_none_or(|u| ts <= u)
        });
        // `limit` keeps the most recent operations.
        if let Some(limit) = params.limit {
            if ops.len() > limit {
                ops.drain(..ops.len() - limit);
            }
        }

        let include_snapshots = params.include_snapshots.unwrap_or(false);
        let mut op_values = Vec::with_capacity(ops.len());
        let mut seen_revs = std::collections::HashSet::new();
        let mut revisions = Vec::new();
        for op in &ops {
            op_values.push(op_json(&conn, op, include_snapshots)?);
            if seen_revs.insert(op.rev_id) {
                if let Some((ts, label)) = rev_meta.get(&op.rev_id) {
                    revisions.push(json!({"id": op.rev_id, "timestamp": ts, "label": label}));
                }
            }
        }
        Ok(Json(json!({"head": head, "operations": op_values, "revisions": revisions})))
    })
    .await
}

#[derive(Deserialize)]
struct SinceParams {
    #[serde(default)]
    op: Option<i64>,
}

/// Change feed for client caches: the current log `head` plus every operation
/// created after `?op=<id>` (across all branches; each names its `entity_uuid`).
/// With no `op` it returns just the head (a baseline), and an empty `operations`
/// when nothing changed — so one call both detects a change and describes it.
async fn get_log_since(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    Query(params): Query<SinceParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = repo_state.conn.lock_recover();
        let head = crate::log::get_head(&conn)?;
        let operations = match params.op {
            Some(since) => {
                let ops = crate::log::ops_since(&conn, since)?;
                let mut out = Vec::with_capacity(ops.len());
                for op in &ops {
                    out.push(op_json(&conn, op, false)?);
                }
                out
            }
            None => Vec::new(),
        };
        Ok(Json(json!({"head": head, "operations": operations})))
    })
    .await
}

async fn get_revision(
    State(state): State<Arc<AppState>>,
    Path((repo, rev_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = repo_state.conn.lock_recover();
        let head = crate::log::get_head(&conn)?;
        let rev_id: i64 = if rev_id == "head" {
            let head = head
                .ok_or_else(|| ApiError::not_found("the history is empty (no HEAD revision)"))?;
            crate::log::get_op(&conn, head)?
                .ok_or_else(|| ApiError::internal("HEAD operation vanished"))?
                .rev_id
        } else {
            rev_id
                .parse()
                .map_err(|_| ApiError::bad_request(format!("invalid revision id '{rev_id}'")))?
        };

        let mut revision = revision_json(&conn, rev_id)?;
        let mut ops = Vec::new();
        let mut is_head = false;
        {
            let mut stmt = conn
                .prepare(
                    "SELECT id FROM operation WHERE rev_id = ?1 ORDER BY seq",
                )
                .map_err(anyhow::Error::from)?;
            let ids = stmt
                .query_map(rusqlite::params![rev_id], |r| r.get::<_, i64>(0))
                .map_err(anyhow::Error::from)?
                .collect::<Result<Vec<i64>, _>>()
                .map_err(anyhow::Error::from)?;
            for id in ids {
                let op = crate::log::get_op(&conn, id)?
                    .ok_or_else(|| ApiError::internal("operation vanished"))?;
                if Some(op.id) == head {
                    is_head = true;
                }
                ops.push(op_json(&conn, &op, true)?);
            }
        }
        revision["is_head"] = json!(is_head);
        Ok(Json(json!({"revision": revision, "operations": ops})))
    })
    .await
}

#[derive(Deserialize)]
struct LabelBody {
    label: Option<String>,
}

async fn patch_revision(
    State(state): State<Arc<AppState>>,
    Path((repo, rev_id)): Path<(String, i64)>,
    payload: Result<Json<LabelBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = repo_state.conn.lock_recover();
        let changed = conn
            .execute(
                "UPDATE revision SET label = ?1 WHERE id = ?2",
                rusqlite::params![body.label, rev_id],
            )
            .map_err(anyhow::Error::from)?;
        if changed == 0 {
            return Err(ApiError::not_found(format!("revision {rev_id} not found")));
        }
        Ok(Json(revision_json(&conn, rev_id)?))
    })
    .await
}

/// A rollback/prune target: exactly one of the four forms.
#[derive(Deserialize)]
struct TargetBody {
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    timestamp: Option<i64>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    prev_revision: Option<bool>,
}

impl TargetBody {
    fn into_target(self) -> Result<crate::log::Target, ApiError> {
        match (self.id, self.timestamp, self.label, self.prev_revision) {
            (Some(id), None, None, None) => Ok(crate::log::Target::Id(id)),
            (None, Some(ts), None, None) => Ok(crate::log::Target::Timestamp(ts)),
            (None, None, Some(label), None) => Ok(crate::log::Target::Label(label)),
            (None, None, None, Some(true)) => Ok(crate::log::Target::PrevRevision),
            _ => Err(ApiError::bad_request(
                "target must be exactly one of {id}, {timestamp}, {label}, {prev_revision: true}",
            )),
        }
    }
}

#[derive(Deserialize)]
struct RollbackBody {
    target: TargetBody,
}

/// MetaRecord-only atomic rollback (spec-event-log `POST /rollback`).
async fn rollback(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<RollbackBody>, JsonRejection>,
) -> Result<Json<crate::log::NavResult>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    let target = body.target.into_target()?;
    with_repo(&state, repo_uuid, move |repo_state| {
        repo_state.ensure_writable()?;
        let mut conn = repo_state.conn.lock_recover();
        let resolved = crate::log::resolve_target(&conn, &target)?;
        let result = crate::log::navigate(&mut conn, repo_uuid, resolved)?;
        // Navigation rewrites tree positions arbitrarily: rebuild the cache
        // from the new state (keeps it complete; `populate` clears first).
        repo_state.lock_cache().populate(&conn)?;
        Ok(Json(result))
    })
    .await
}

#[derive(Deserialize)]
struct PruneBody {
    mode: String,
    target: TargetBody,
}

async fn prune_log(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<PruneBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    let mode = match body.mode.as_str() {
        "before" => crate::log::PruneMode::Before,
        "linearize" => crate::log::PruneMode::Linearize,
        other => {
            return Err(ApiError::bad_request(format!(
                "invalid prune mode '{other}' (expected 'before' or 'linearize')"
            )))
        }
    };
    let target = body.target.into_target()?;
    with_repo(&state, repo_uuid, move |repo_state| {
        repo_state.ensure_writable()?;
        let mut conn = repo_state.conn.lock_recover();
        let resolved = crate::log::resolve_target(&conn, &target)?
            .ok_or_else(|| ApiError::bad_request("cannot prune to the empty state"))?;
        let (ops, revisions) = crate::log::prune(&mut conn, mode, resolved)
            .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
        Ok(Json(json!({"pruned_operations": ops, "pruned_revisions": revisions})))
    })
    .await
}

// ── Coordinated navigation (spec-event-log "Coordinated navigation") ────────────

/// Query-parameter target form for the plan endpoints.
#[derive(Deserialize)]
struct PlanParams {
    #[serde(default)]
    target_id: Option<i64>,
    #[serde(default)]
    target_label: Option<String>,
    #[serde(default)]
    target_timestamp: Option<i64>,
    #[serde(default)]
    target_prev_revision: Option<bool>,
}

impl PlanParams {
    fn into_target(self) -> Result<crate::log::Target, ApiError> {
        match (self.target_id, self.target_timestamp, self.target_label, self.target_prev_revision)
        {
            (Some(id), None, None, None) => Ok(crate::log::Target::Id(id)),
            (None, Some(ts), None, None) => Ok(crate::log::Target::Timestamp(ts)),
            (None, None, Some(label), None) => Ok(crate::log::Target::Label(label)),
            (None, None, None, Some(true)) => Ok(crate::log::Target::PrevRevision),
            _ => Err(ApiError::bad_request(
                "target must be exactly one of target_id, target_timestamp, target_label, target_prev_revision",
            )),
        }
    }
}

/// Resolves the `mfr_path` of one operation snapshot to an OS-native absolute
/// path, for the `from`/`to` of a `move_file` action.
fn snapshot_abs_path(
    conn: &rusqlite::Connection,
    cache: &mut crate::tree_cache::TreeCache,
    root: &std::path::Path,
    op_id: i64,
    is_new: i64,
) -> Result<Option<String>, ApiError> {
    for row in crate::log::snapshots(conn, op_id, is_new)? {
        if row.name == "mfr_path" {
            if let Value::TreeRef { parent, name } = row.value {
                let parent_rel = match parent {
                    Some(p) => cache.path_of(conn, "mfr_path", p)?.unwrap_or_default(),
                    None => String::new(),
                };
                let rel = format!("{parent_rel}/{name}");
                let abs = root.join(rel.trim_start_matches('/'));
                return Ok(Some(abs.to_string_lossy().into_owned()));
            }
        }
    }
    Ok(None)
}

/// Builds the action JSON for one navigation step (spec-event-log: the
/// response `op_type` reflects the *action to execute* — a stored `file_moved`
/// becomes `move_file` with `from`/`to`; everything else is unchanged).
fn action_op_json(
    conn: &rusqlite::Connection,
    cache: &mut crate::tree_cache::TreeCache,
    root: &std::path::Path,
    op: &crate::log::OpRow,
    dir: crate::log::NavDir,
) -> Result<serde_json::Value, ApiError> {
    let is_move = op.op_type == "file_moved";
    let action = if is_move { "move_file" } else { op.op_type.as_str() };
    let mut value = json!({
        "id": op.id,
        "op_type": action,
        "entity_uuid": hex(op.entity_uuid),
    });
    if is_move {
        // Inverse: undo the move (after → before). Forward: redo (before → after).
        let (from_is_new, to_is_new) = match dir {
            crate::log::NavDir::Inverse => (1, 0),
            crate::log::NavDir::Forward => (0, 1),
        };
        let from = snapshot_abs_path(conn, cache, root, op.id, from_is_new)?;
        let to = snapshot_abs_path(conn, cache, root, op.id, to_is_new)?;
        if let (Some(from), Some(to)) = (from, to) {
            value["from"] = json!(from);
            value["to"] = json!(to);
        }
    }
    Ok(value)
}

async fn rollback_plan(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    Query(params): Query<PlanParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let target = params.into_target()?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = repo_state.conn.lock_recover();
        let head = crate::log::get_head(&conn)?;
        let resolved = crate::log::resolve_target(&conn, &target)?;
        let path = crate::log::nav_path(&conn, head, resolved)?;
        let mut cache = repo_state.lock_cache();
        let mut ops = Vec::with_capacity(path.len());
        for (op, dir) in &path {
            ops.push(action_op_json(&conn, &mut cache, &repo_state.config.root, op, *dir)?);
        }
        let total = ops.len();
        Ok(Json(json!({"operations": ops, "total": total})))
    })
    .await
}

async fn rollback_plan_summary(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    Query(params): Query<PlanParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let target = params.into_target()?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = repo_state.conn.lock_recover();
        let head = crate::log::get_head(&conn)?;
        let resolved = crate::log::resolve_target(&conn, &target)?;
        let path = crate::log::nav_path(&conn, head, resolved)?;
        let mut by_type: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
        let mut revs = std::collections::HashSet::new();
        for (op, _) in &path {
            *by_type.entry(op.op_type.clone()).or_insert(0) += 1;
            revs.insert(op.rev_id);
        }
        Ok(Json(json!({
            "total_operations": path.len(),
            "by_type": by_type,
            "revisions_affected": revs.len(),
        })))
    })
    .await
}

async fn rollback_start(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<RollbackBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    let target = body.target.into_target()?;
    with_repo(&state, repo_uuid, move |repo_state| {
        if repo_state.is_rollback_locked() {
            return Err(ApiError::conflict("a rollback navigation is already in progress"));
        }
        let conn = repo_state.conn.lock_recover();
        let head = crate::log::get_head(&conn)?;
        let resolved = crate::log::resolve_target(&conn, &target)?;
        if resolved == head {
            // Nothing to do: the lock is not entered.
            return Ok(Json(json!({"op": null, "remaining": 0})));
        }
        let path = crate::log::nav_path(&conn, head, resolved)?;
        let (op, dir) = path.first().expect("non-empty path when head != target");
        let mut cache = repo_state.lock_cache();
        let first = action_op_json(&conn, &mut cache, &repo_state.config.root, op, *dir)?;
        let remaining = path.len() - 1;
        drop(cache);
        drop(conn);
        *repo_state.rollback_lock.lock_recover() = Some(RollbackLock { target: resolved });
        Ok(Json(json!({"op": first, "remaining": remaining})))
    })
    .await
}

#[derive(Deserialize, Default)]
struct StepBody {
    #[serde(default)]
    skip: bool,
}

async fn rollback_step(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<StepBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // The body is optional: `{}` and an empty body both mean "apply inverse".
    let skip = payload.map(|Json(b)| b.skip).unwrap_or(false);
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let target = {
            let guard = repo_state.rollback_lock.lock_recover();
            let lock = guard.as_ref().ok_or_else(|| {
                ApiError::conflict("no rollback navigation in progress; call start first")
            })?;
            lock.target
        };

        let done = {
            let mut conn = repo_state.conn.lock_recover();
            let new_head = crate::log::coordinated_step(&mut conn, repo_uuid, target, skip)?;
            // The step rewrote tree positions arbitrarily: rebuild the cache
            // from the new state (keeps it complete; `populate` clears first).
            repo_state.lock_cache().populate(&conn)?;
            let next = crate::log::nav_path(&conn, new_head, target)?;
            if let Some((op, dir)) = next.first() {
                let mut cache = repo_state.lock_cache();
                let op_json =
                    action_op_json(&conn, &mut cache, &repo_state.config.root, op, *dir)?;
                let remaining = next.len() - 1;
                return Ok(Json(json!({"op": op_json, "remaining": remaining})));
            }
            true
        };

        if done {
            // HEAD reached the target: release the lock, replay the buffer.
            *repo_state.rollback_lock.lock_recover() = None;
            crate::executor::flush_pending(repo_state)?;
        }
        Ok(Json(json!({"op": null, "remaining": 0})))
    })
    .await
}

async fn rollback_abort(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        {
            let mut guard = repo_state.rollback_lock.lock_recover();
            if guard.is_none() {
                return Err(ApiError::conflict("no rollback navigation in progress"));
            }
            *guard = None;
        }
        crate::executor::flush_pending(repo_state)?;
        let conn = repo_state.conn.lock_recover();
        let head = crate::log::get_head(&conn)?;
        Ok(Json(json!({"head": head})))
    })
    .await
}

// ── User schema ───────────────────────────────────────────────────────────────

async fn get_schema(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let repo_state = state.repo(repo_uuid)?;
    let guard = repo_state.schema.lock_recover();
    Ok(Json(match guard.as_ref() {
        Some(schema) => schema.raw().clone(),
        None => crate::schema::CompiledSchema::empty_raw(),
    }))
}

/// Re-reads the schema file; on error the previous schema stays in effect.
async fn reload_schema(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let loaded =
            crate::schema::load_for_repo(&repo_state.metafolder_dir, &repo_state.config)
                .map_err(ApiError::bad_request)?;
        let raw = loaded
            .as_ref()
            .map(|s| s.raw().clone())
            .unwrap_or_else(crate::schema::CompiledSchema::empty_raw);
        *repo_state.schema.lock_recover() = loaded;
        Ok(Json(raw))
    })
    .await
}

#[derive(Deserialize, Default)]
struct CheckBody {
    #[serde(default)]
    query: Option<MetaQuery>,
    /// Cap on the number of violations returned. The scan stops once it is
    /// exceeded (so a huge repo never builds an unusable response); the response
    /// then carries `truncated: true`. `None` returns every violation.
    #[serde(default)]
    limit: Option<usize>,
}

/// Scans metarecords and reports constraint violations (the schema file is never
/// validated retroactively on edit). With `limit`, stops after that many and
/// flags `truncated`.
async fn check_schema(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Option<Json<CheckBody>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let body = payload.map(|Json(b)| b).unwrap_or_default();
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = repo_state.conn.lock_recover();
        let uuids = match &body.query {
            None => db::list_entries(&conn, repo_uuid)?,
            Some(query) => {
                let mut cache = repo_state.lock_cache();
                query_exec::execute(&conn, &mut cache, repo_uuid, query, &[], None, None)?.0
            }
        };
        let guard = repo_state.schema.lock_recover();
        let mut violations: Vec<serde_json::Value> = Vec::new();
        let mut checked = 0usize;
        if let Some(schema) = guard.as_ref() {
            let fields = schema.constrained_fields();
            // Collect up to `limit + 1` so truncation is exact, then stop the
            // scan — a repo with hundreds of thousands of violations stays cheap.
            'scan: for uuid in &uuids {
                checked += 1;
                for violation in
                    crate::schema::validate_entry_fields(schema, &conn, *uuid, &fields)?
                {
                    violations
                        .push(serde_json::to_value(&violation).expect("violation serialization"));
                    if body.limit.is_some_and(|l| violations.len() > l) {
                        break 'scan;
                    }
                }
            }
        }
        let truncated = body.limit.is_some_and(|l| violations.len() > l);
        if let Some(l) = body.limit {
            violations.truncate(l);
        }
        // `checked` is the number of metarecords actually examined — fewer than
        // the total when the scan stopped early at the cap.
        Ok(Json(json!({"checked": checked, "violations": violations, "truncated": truncated})))
    })
    .await
}

// ── Reconcile and track ───────────────────────────────────────────────────────

fn default_true() -> bool {
    true
}

#[derive(Deserialize)]
struct ReconcileBody {
    /// Optional scope: when present, reconcile only the subtree rooted at this
    /// metarecord (32-char hex); absent reconciles the whole repository
    /// (spec-tasks "Reconcile as a task"). The similarity `threshold` applies
    /// to the whole-repository reconcile only.
    #[serde(default)]
    metarecord: Option<String>,
    /// Minimum similarity score for the v2 similarity phase, range [0, 1].
    /// Absent disables similarity (v1 behaviour).
    #[serde(default)]
    threshold: Option<f64>,
    /// Compute `mfr_mime` for files that lack it (default true).
    #[serde(default = "default_true")]
    mime: bool,
    /// Refresh the stat-derived `mfr_*` fields of files/directories still at
    /// their recorded path, catching in-place edits (default true).
    #[serde(default = "default_true")]
    refresh: bool,
}

impl Default for ReconcileBody {
    fn default() -> Self {
        Self { metarecord: None, threshold: None, mime: true, refresh: true }
    }
}

/// `POST /repos/:repo/reconcile`: starts a reconcile as a background task
/// (spec-tasks). Returns `202 Accepted` with the task id immediately; progress
/// and the final `ReconcileResult` are observed via `GET …/tasks/:id`. A
/// concurrent reconcile is rejected with `409`. With `metarecord` in the body
/// the reconcile is scoped to that metarecord's subtree; absent, it covers the
/// whole repository.
async fn full_reconcile(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Option<Json<ReconcileBody>>,
) -> Result<Response, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let body = payload.map(|Json(b)| b).unwrap_or_default();
    if let Some(t) = body.threshold {
        if !(0.0..=1.0).contains(&t) {
            return Err(ApiError::bad_request("threshold must be in the range [0, 1]"));
        }
    }
    let scope = body.metarecord.as_deref().map(parse_uuid).transpose()?;
    let repo_state = state.repo(repo_uuid)?;
    repo_state.ensure_writable()?;
    let task_id = repo_state
        .tasks
        .start_unique(TaskKind::Reconcile)
        .ok_or_else(|| ApiError::conflict("a reconcile is already in progress for this repository"))?;

    // The work runs detached from this request: closing the client does not
    // interrupt it. It holds an Arc for its (bounded) duration; that is fine —
    // unlike the watcher/executor it is not a repo-lifetime task.
    tokio::task::spawn_blocking(move || {
        repo_state.tasks.mark_running(task_id);
        let progress = |phase: &str, done: Option<u64>, total: Option<u64>| {
            repo_state.tasks.set_progress(task_id, phase, done, total);
        };
        // Cooperative cancellation (spec-tasks): the reconcile polls this at its
        // progress checkpoints and bails (rolling its transaction back) when a
        // `POST …/tasks/:id/cancel` has flipped the flag.
        let cancel = || repo_state.tasks.is_cancel_requested(task_id);
        let outcome = match scope {
            Some(uuid) => crate::reconcile::reconcile_metarecord_reported(
                &repo_state,
                uuid,
                body.mime,
                body.refresh,
                &progress,
                &cancel,
            ),
            None => crate::reconcile::reconcile_full_reported(
                &repo_state,
                body.threshold,
                body.mime,
                body.refresh,
                &progress,
                &cancel,
            ),
        };
        match outcome {
            Ok(result) => {
                let value = serde_json::to_value(result).expect("reconcile result serialization");
                repo_state.tasks.finish(task_id, Some(value));
            }
            // A bail triggered by the cancel flag becomes a `cancelled` task, not
            // a `failed` one — the distinction the user asked for.
            Err(_) if cancel() => repo_state.tasks.mark_cancelled(task_id),
            Err(e) => repo_state.tasks.fail(task_id, &e.message),
        }
    });

    Ok((StatusCode::ACCEPTED, Json(json!({"task_id": hex(task_id)}))).into_response())
}

#[derive(Deserialize)]
struct TrackBody {
    path: PathBuf,
}

/// Creates the metarecord for a single filesystem path without activating
/// tracking (spec-file-tracking "Single-metarecord track"). Parents are created
/// with `mf_watch = false`; no eligibility check applies.
async fn track(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<TrackBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        repo_state.ensure_writable()?;
        let abs = body
            .path
            .canonicalize()
            .map_err(|_| ApiError::bad_request(format!("path does not exist: {:?}", body.path)))?;
        let rel_path = abs.strip_prefix(&repo_state.config.root).map_err(|_| {
            ApiError::bad_request(format!(
                "path {abs:?} is outside the repository root {:?}",
                repo_state.config.root
            ))
        })?;
        let mut rel = String::new();
        for comp in rel_path.components() {
            let std::path::Component::Normal(name) = comp else {
                return Err(ApiError::bad_request(format!("unsupported path component in {abs:?}")));
            };
            rel.push('/');
            rel.push_str(name.to_str().ok_or_else(|| {
                ApiError::bad_request(format!("non-UTF-8 name in {abs:?} is not supported"))
            })?);
        }
        if rel.is_empty() {
            return Err(ApiError::bad_request("cannot track the repository root itself"));
        }

        let mut conn = repo_state.conn.lock_recover();
        let mut cache = repo_state.lock_cache();
        if let Some(existing) = cache.resolve_path(&conn, "mfr_path", &rel)? {
            return Err(ApiError::conflict(format!(
                "path already tracked by metarecord {}",
                hex(existing)
            )));
        }
        let untracked = [Field::new("mf_watch", Value::Bool(false))];
        let mut writer = Writer::begin(&mut conn, repo_uuid, None)?;
        let uuid = crate::reconcile::create_record_for(
            &mut writer,
            &mut cache,
            &repo_state.config.root,
            &rel,
            &untracked,
            false,
        )?;
        writer.commit()?;
        Ok(Json(json!({"uuid": hex(uuid)})))
    })
    .await
}

// ── Query and batch set ───────────────────────────────────────────────────────

/// `select`: absent → UUID strings; `"*"` → full objects; list → restricted
/// objects (spec-query).
#[derive(Deserialize)]
#[serde(untagged)]
enum SelectSpec {
    Star(String),
    Fields(Vec<String>),
}

#[derive(Deserialize)]
struct QueryBody {
    query: MetaQuery,
    #[serde(default)]
    select: Option<SelectSpec>,
    #[serde(default)]
    sort: Vec<SortKey>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    cursor: Option<String>,
    /// Adds the full result count to the pagination envelope.
    #[serde(default)]
    count: bool,
}

async fn run_query(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<QueryBody>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        // Register an observation-only task (spec-tasks): the result travels
        // with this response, so the task carries no result payload and its
        // counts stay unknown (the heavy part is opaque SQL).
        let task = repo_state.tasks.start(TaskKind::Query);
        repo_state.tasks.mark_running(task);
        repo_state.tasks.set_progress(task, "querying", None, None);
        let outcome = run_query_inner(repo_state, repo_uuid, &body, task);
        // A cancel request interrupts the SQLite statement, surfacing here as an
        // error: record the task as `cancelled` (not `failed`) and report it as
        // a 409 to the waiting client.
        if outcome.is_err() && repo_state.tasks.is_cancel_requested(task) {
            repo_state.tasks.mark_cancelled(task);
            return Err(ApiError::conflict("query cancelled"));
        }
        match &outcome {
            Ok(_) => repo_state.tasks.finish(task, None),
            Err(e) => repo_state.tasks.fail(task, &e.message),
        }
        outcome
    })
    .await
}

/// Resolves a query's page (and optional total) through the in-memory bitmap
/// index when it is applicable, falling back to the SQL engine otherwise.
///
/// The index is consulted only while it reflects the current log HEAD; after any
/// write the HEAD advances and the index is rebuilt before use, so it can never
/// serve stale results. A query shape the index does not accelerate (`Matches`,
/// a path-target `Follows`, or a foreign cursor) returns `Unsupported` and the
/// SQL engine handles it — including its own cursor. Because supportedness is a
/// property of the query, a paginated session stays on one engine throughout.
fn run_query_filter(
    repo_state: &RepoState,
    conn: &rusqlite::Connection,
    cache: &mut crate::tree_cache::TreeCache,
    repo_uuid: Uuid,
    body: &QueryBody,
) -> Result<(Vec<Uuid>, Option<String>, Option<usize>), ApiError> {
    // Reject ill-defined comparisons upfront, before choosing an engine, so the
    // rejection never depends on the index→SQL fallback path (spec-query).
    query_exec::validate_query(&body.query)?;

    let sort_by: Vec<crate::index::SortBy> = body
        .sort
        .iter()
        .map(|k| crate::index::SortBy {
            field: k.field.clone(),
            ascending: matches!(k.order, query_exec::SortOrder::Asc),
        })
        .collect();

    // Resolve every Path-target follows in the query to its root metarecord
    // through the (eagerly populated) tree cache, so the bitmap index can serve
    // `mfr_path ->* "/dir"` by in-memory expansion instead of deferring to SQL.
    let mut path_targets = Vec::new();
    crate::index::collect_path_targets(&body.query, &mut path_targets);
    let mut roots = crate::index::PathRoots::new();
    for (field, path) in path_targets {
        if let Some(uuid) = cache.resolve_path(conn, &field, &path)? {
            roots.insert((field, path), uuid);
        }
    }

    let mut index_guard = repo_state.index.lock_recover();
    match index_guard.as_mut() {
        // Already built: bring it up to the current HEAD (incrementally when the
        // delta is a forward extension, else an internal full rebuild).
        Some(index) => index.refresh(conn, repo_uuid)?,
        None => *index_guard = Some(crate::index::RepoIndex::build(conn, repo_uuid)?),
    }
    let index = index_guard.as_ref().expect("index built above");

    match index.evaluate_page_with_roots(&body.query, &sort_by, body.limit, body.cursor.as_deref(), &roots)
    {
        Ok((uuids, next_cursor)) => {
            let total = body.count.then(|| {
                index.count_with_roots(&body.query, &roots).expect("a page-able query also counts")
                    as usize
            });
            Ok((uuids, next_cursor, total))
        }
        Err(_unsupported) => {
            let (uuids, next_cursor) = query_exec::execute(
                conn,
                cache,
                repo_uuid,
                &body.query,
                &body.sort,
                body.limit,
                body.cursor.as_deref(),
            )?;
            let total = body
                .count
                .then(|| query_exec::count(conn, cache, repo_uuid, &body.query))
                .transpose()?;
            Ok((uuids, next_cursor, total))
        }
    }
}

fn run_query_inner(
    repo_state: &RepoState,
    repo_uuid: Uuid,
    body: &QueryBody,
    task: Uuid,
) -> Result<Response, ApiError> {
    {
        if body.count && body.limit.is_none() {
            // The unwrapped (bare array) response has nowhere to carry it.
            return Err(ApiError::bad_request("'count' requires 'limit'"));
        }
        let conn = repo_state.conn.lock_recover();
        // Register the SQLite interrupt handle so `POST …/tasks/:id/cancel` can
        // abort this query while it runs (spec-tasks "Cancellation"). The handle
        // is harmless once the query finishes (no running statement to stop).
        let handle = conn.get_interrupt_handle();
        repo_state.tasks.set_canceller(task, Box::new(move || handle.interrupt()));
        let mut cache = repo_state.lock_cache();
        let (uuids, next_cursor, total) =
            run_query_filter(repo_state, &conn, &mut cache, repo_uuid, body)?;
        drop(cache);

        let results: Vec<serde_json::Value> = match &body.select {
            None => uuids.into_iter().map(|u| json!(hex(u))).collect(),
            Some(select) => {
                let fields_filter: Option<Vec<String>> = match select {
                    SelectSpec::Star(s) if s == "*" => None,
                    SelectSpec::Star(s) => {
                        return Err(ApiError::bad_request(format!(
                            "invalid select: '{s}' (expected \"*\" or a field list)"
                        )))
                    }
                    SelectSpec::Fields(list) => Some(list.clone()),
                };
                let mut objects = Vec::with_capacity(uuids.len());
                for uuid in uuids {
                    let mut metarecord = metarecord_response(&conn, uuid)?;
                    if let Some(filter) = &fields_filter {
                        metarecord.fields.retain(|f| filter.contains(&f.name));
                    }
                    objects.push(serde_json::to_value(metarecord).expect("metarecord serialization"));
                }
                objects
            }
        };

        if body.limit.is_some() {
            Ok(Json(Page { results, next_cursor, total }).into_response())
        } else {
            Ok(Json(results).into_response())
        }
    }
}

#[derive(Deserialize)]
struct BatchSetBody {
    query: MetaQuery,
    name: String,
    #[serde(default)]
    value: Option<Value>,
    #[serde(default)]
    values: Option<Vec<Value>>,
    #[serde(default)]
    force: bool,
}

/// Resolves a `{value | values}` field-write body to its row set; exactly one of
/// the two must be present (set accepts several, the single-value ops one).
fn resolved_values(value: Option<Value>, values: Option<Vec<Value>>) -> Result<Vec<Value>, ApiError> {
    match (value, values) {
        (Some(_), Some(_)) => {
            Err(ApiError::bad_request("provide either 'value' or 'values', not both"))
        }
        (Some(v), None) => Ok(vec![v]),
        (None, Some(vs)) => Ok(vs),
        (None, None) => Err(ApiError::bad_request("missing 'value' (or 'values')")),
    }
}

/// Like [`resolved_values`] but for operations that take exactly one value
/// (append, remove).
fn single_value(value: Option<Value>, values: Option<Vec<Value>>) -> Result<Value, ApiError> {
    match (value, values) {
        (Some(v), None) => Ok(v),
        _ => Err(ApiError::bad_request("this operation takes a single 'value'")),
    }
}

/// Runs the query server-side and sets the field on every match in a single
/// transaction (one revision). `value` sets one row; `values` a multi-map set —
/// either way one `SetField` op per metarecord.
async fn batch_set(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<BatchSetBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    let rows = resolved_values(body.value, body.values)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        repo_state.ensure_writable()?;
        check_writable(&body.name, body.force)?;
        let mut conn = repo_state.conn.lock_recover();
        let mut cache = repo_state.lock_cache();
        let (uuids, _) =
            query_exec::execute(&conn, &mut cache, repo_uuid, &body.query, &[], None, None)?;
        drop(cache);

        let mut writer = Writer::begin(&mut conn, repo_uuid, None)?;
        for uuid in &uuids {
            writer.set_field_multi(*uuid, &body.name, rows.clone())?;
            validate_schema(repo_state, writer.connection(), *uuid, std::slice::from_ref(&body.name))?;
        }
        let tree_touched = writer.touched_tree();
        writer.commit()?;
        if tree_touched {
            repo_state.lock_cache().populate(&conn)?;
        }
        Ok(Json(json!({"updated": uuids.len()})))
    })
    .await
}

/// Runs the query server-side and appends one field row to every match in a
/// single transaction (one revision) — the bulk form of `POST
/// /metarecords/:uuid/fields`. Multi-map: never replaces existing rows.
async fn batch_append(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<BatchSetBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    let value = single_value(body.value, body.values)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        repo_state.ensure_writable()?;
        check_writable(&body.name, body.force)?;
        let mut conn = repo_state.conn.lock_recover();
        let mut cache = repo_state.lock_cache();
        let (uuids, _) =
            query_exec::execute(&conn, &mut cache, repo_uuid, &body.query, &[], None, None)?;
        drop(cache);

        let mut writer = Writer::begin(&mut conn, repo_uuid, None)?;
        for uuid in &uuids {
            writer.append_field(*uuid, &body.name, value.clone())?;
            validate_schema(repo_state, writer.connection(), *uuid, std::slice::from_ref(&body.name))?;
        }
        let tree_touched = writer.touched_tree();
        writer.commit()?;
        if tree_touched {
            repo_state.lock_cache().populate(&conn)?;
        }
        Ok(Json(json!({"updated": uuids.len()})))
    })
    .await
}

/// Runs the query server-side and removes every field row equal to
/// `(name, value)` from each match in a single transaction (one revision) — the
/// inverse of `batch_append`. `updated` counts the metarecords actually changed
/// (those that carried at least one matching row).
async fn batch_remove(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<BatchSetBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    let value = single_value(body.value, body.values)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        repo_state.ensure_writable()?;
        check_writable(&body.name, body.force)?;
        let mut conn = repo_state.conn.lock_recover();
        let mut cache = repo_state.lock_cache();
        let (uuids, _) =
            query_exec::execute(&conn, &mut cache, repo_uuid, &body.query, &[], None, None)?;
        drop(cache);

        let mut writer = Writer::begin(&mut conn, repo_uuid, None)?;
        let mut changed = 0usize;
        for uuid in &uuids {
            if writer.delete_fields_valued(*uuid, &body.name, &value)? > 0 {
                changed += 1;
                validate_schema(repo_state, writer.connection(), *uuid, std::slice::from_ref(&body.name))?;
            }
        }
        let tree_touched = writer.touched_tree();
        writer.commit()?;
        if tree_touched {
            repo_state.lock_cache().populate(&conn)?;
        }
        Ok(Json(json!({"updated": changed})))
    })
    .await
}

#[derive(Deserialize)]
struct BatchUnsetBody {
    query: MetaQuery,
    name: String,
    #[serde(default)]
    force: bool,
}

/// Runs the query server-side and removes the field *entirely* (every row of
/// `name`) from each match in a single transaction (one revision; one
/// `DeleteField` op per affected metarecord). The field becomes unknown. `updated`
/// counts the metarecords that carried the field.
async fn batch_unset(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<BatchUnsetBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        repo_state.ensure_writable()?;
        check_writable(&body.name, body.force)?;
        let mut conn = repo_state.conn.lock_recover();
        let mut cache = repo_state.lock_cache();
        let (uuids, _) =
            query_exec::execute(&conn, &mut cache, repo_uuid, &body.query, &[], None, None)?;
        drop(cache);

        let mut writer = Writer::begin(&mut conn, repo_uuid, None)?;
        let mut changed = 0usize;
        for uuid in &uuids {
            if writer.delete_fields_named(*uuid, &body.name)? > 0 {
                changed += 1;
                validate_schema(repo_state, writer.connection(), *uuid, std::slice::from_ref(&body.name))?;
            }
        }
        let tree_touched = writer.touched_tree();
        writer.commit()?;
        if tree_touched {
            repo_state.lock_cache().populate(&conn)?;
        }
        Ok(Json(json!({"updated": changed})))
    })
    .await
}

#[derive(Deserialize)]
struct RetypeBody {
    name: String,
    to: String,
}

/// `POST /repos/:repo/retype`: converts every non-`Nothing` row of the field
/// `name` to a new scalar type, repository-wide, in one revision (spec-data-model
/// "Changing a field's type"). Reserved fields (`mfr_*`/`mf_*`) are rejected
/// unconditionally — the system owns their types.
async fn retype_field(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<RetypeBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    let name = body.name;
    if name.starts_with("mfr_") || name.starts_with("mf_") {
        return Err(ApiError::bad_request(format!(
            "field '{name}' is reserved; its type is owned by the system and cannot be retyped"
        )));
    }
    let to = FieldType::parse(&body.to).ok_or_else(|| {
        ApiError::bad_request(format!(
            "invalid target type '{}': retype targets one of \
             string/int/float/bool/datetime/ref/tree_ref/externalref/refbase",
            body.to
        ))
    })?;
    with_repo(&state, repo_uuid, move |repo_state| {
        repo_state.ensure_writable()?;
        let mut conn = repo_state.conn.lock_recover();
        let mut writer = Writer::begin(&mut conn, repo_uuid, None)?;
        let summary = writer.retype_field(&name, to)?;
        let tree_touched = writer.touched_tree();
        writer.commit()?;
        if tree_touched {
            repo_state.lock_cache().populate(&conn)?;
        }
        Ok(Json(json!({
            "converted": summary.converted,
            "fallback_count": summary.fallback_uuids.len(),
            "fallback_uuids": summary.fallback_uuids.iter().map(|u| hex(*u)).collect::<Vec<_>>(),
        })))
    })
    .await
}

#[derive(Deserialize)]
struct QueryDeleteBody {
    query: MetaQuery,
}

/// `POST /repos/:repo/delete` — deletes every metarecord matching `query` in a
/// single transaction (one revision). Atomic and free of the client-side
/// TOCTOU of selecting then deleting one-by-one over HTTP.
async fn delete_by_query(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<QueryDeleteBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        repo_state.ensure_writable()?;
        let mut conn = repo_state.conn.lock_recover();
        let mut cache = repo_state.lock_cache();
        let (uuids, _) =
            query_exec::execute(&conn, &mut cache, repo_uuid, &body.query, &[], None, None)?;
        drop(cache);

        let mut writer = Writer::begin(&mut conn, repo_uuid, None)?;
        for uuid in &uuids {
            writer.delete_metarecord(*uuid)?;
        }
        let tree_touched = writer.touched_tree();
        writer.commit()?;
        // Deleting a metarecord with a TreeRef removes tree nodes: rebuild the
        // complete cache so reads stay correct (no-op if absent).
        if tree_touched {
            repo_state.lock_cache().populate(&conn)?;
        }
        Ok(Json(json!({"deleted": uuids.len()})))
    })
    .await
}

#[derive(Deserialize)]
struct CreateBody {
    fields: Vec<Field>,
    #[serde(default)]
    force: bool,
}

async fn create_record_endpoint(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<CreateBody>, JsonRejection>,
) -> Result<Json<MetaRecord>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        repo_state.ensure_writable()?;
        for field in &body.fields {
            check_writable(&field.name, body.force)?;
        }
        let mut conn = repo_state.conn.lock_recover();
        let touched: Vec<String> = body.fields.iter().map(|f| f.name.clone()).collect();
        let mut writer = Writer::begin(&mut conn, repo_uuid, None)?;
        let created = writer.create_metarecord(body.fields)?;
        validate_schema(repo_state, writer.connection(), created.uuid, &touched)?;
        let tree_touched = writer.touched_tree();
        writer.commit()?;
        if tree_touched {
            repo_state.lock_cache().populate(&conn)?;
        }
        Ok(Json(created))
    })
    .await
}

async fn get_record_endpoint(
    State(state): State<Arc<AppState>>,
    Path((repo, uuid)): Path<(String, String)>,
) -> Result<Json<MetaRecord>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let uuid = parse_uuid(&uuid)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = repo_state.conn.lock_recover();
        Ok(Json(metarecord_response(&conn, uuid)?))
    })
    .await
}

async fn delete_record_endpoint(
    State(state): State<Arc<AppState>>,
    Path((repo, uuid)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let uuid = parse_uuid(&uuid)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        repo_state.ensure_writable()?;
        let mut conn = repo_state.conn.lock_recover();
        if db::get_version(&conn, uuid)?.is_none() {
            return Err(ApiError::not_found(format!("Metarecord not found: {uuid}")));
        }
        let mut writer = Writer::begin(&mut conn, repo_uuid, None)?;
        writer.delete_metarecord(uuid)?;
        let tree_touched = writer.touched_tree();
        writer.commit()?;
        if tree_touched {
            repo_state.lock_cache().populate(&conn)?;
        }
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

#[derive(Deserialize)]
struct SetFieldBody {
    name: String,
    #[serde(default)]
    value: Option<Value>,
    #[serde(default)]
    values: Option<Vec<Value>>,
    #[serde(default)]
    force: bool,
}

#[derive(Deserialize)]
struct RecordFieldBody {
    #[serde(default)]
    value: Option<Value>,
    #[serde(default)]
    values: Option<Vec<Value>>,
    #[serde(default)]
    force: bool,
}

/// `GET /repos/:repo/metarecords/:uuid/fields/:name` — the field's value(s).
async fn get_record_field(
    State(state): State<Arc<AppState>>,
    Path((repo, uuid, name)): Path<(String, String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let uuid = parse_uuid(&uuid)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = repo_state.conn.lock_recover();
        ensure_exists(&conn, uuid)?;
        let rows = db::get_field_rows_named(&conn, uuid, &name)?;
        let values: Vec<&Value> = rows.iter().map(|r| &r.value).collect();
        Ok(Json(json!({ "name": name, "values": values })))
    })
    .await
}

/// `PUT /repos/:repo/metarecords/:uuid/fields/:name` — set: replaces all rows of
/// `name` (one `SetField` op). `value` (one row) or `values` (multi-map).
async fn set_record_field(
    State(state): State<Arc<AppState>>,
    Path((repo, uuid, name)): Path<(String, String, String)>,
    payload: Result<Json<RecordFieldBody>, JsonRejection>,
) -> Result<Json<MetaRecord>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    let uuid = parse_uuid(&uuid)?;
    let rows = resolved_values(body.value, body.values)?;
    write_record(&state, repo_uuid, uuid, move |writer| {
        check_writable(&name, body.force)?;
        ensure_exists(writer.connection(), uuid)?;
        writer.set_field_multi(uuid, &name, rows)?;
        Ok(vec![name])
    })
    .await
    .map(Json)
}

/// `DELETE /repos/:repo/metarecords/:uuid/fields/:name` — unset: removes every
/// row of `name` (one `DeleteField` op), leaving the field unknown.
async fn unset_record_field(
    State(state): State<Arc<AppState>>,
    Path((repo, uuid, name)): Path<(String, String, String)>,
    payload: Option<Json<ForceBody>>,
) -> Result<StatusCode, ApiError> {
    let force = payload.map(|Json(b)| b.force).unwrap_or(false);
    let repo_uuid = parse_uuid(&repo)?;
    let uuid = parse_uuid(&uuid)?;
    write_record(&state, repo_uuid, uuid, move |writer| {
        check_writable(&name, force)?;
        ensure_exists(writer.connection(), uuid)?;
        writer.delete_fields_named(uuid, &name)?;
        Ok(vec![name])
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct SetRecordBody {
    fields: Vec<Field>,
    #[serde(default)]
    force: bool,
}

/// `PUT /repos/:repo/metarecords/:uuid` — whole-record set: replaces the entire
/// field set, keeping the UUID, as one `SetRecord` op (spec-query). Literal
/// overwrite; reserved field names still need `force` to be written.
async fn put_metarecord(
    State(state): State<Arc<AppState>>,
    Path((repo, uuid)): Path<(String, String)>,
    payload: Result<Json<SetRecordBody>, JsonRejection>,
) -> Result<Json<MetaRecord>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    let uuid = parse_uuid(&uuid)?;
    write_record(&state, repo_uuid, uuid, move |writer| {
        for field in &body.fields {
            check_writable(&field.name, body.force)?;
        }
        ensure_exists(writer.connection(), uuid)?;
        let touched: Vec<String> = body.fields.iter().map(|f| f.name.clone()).collect();
        writer.set_record(uuid, body.fields)?;
        Ok(touched)
    })
    .await
    .map(Json)
}

async fn append_field(
    State(state): State<Arc<AppState>>,
    Path((repo, uuid)): Path<(String, String)>,
    payload: Result<Json<SetFieldBody>, JsonRejection>,
) -> Result<Json<MetaRecord>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    let uuid = parse_uuid(&uuid)?;
    let value = single_value(body.value, body.values)?;
    write_record(&state, repo_uuid, uuid, move |writer| {
        check_writable(&body.name, body.force)?;
        ensure_exists(writer.connection(), uuid)?;
        writer.append_field(uuid, &body.name, value)?;
        Ok(vec![body.name])
    })
    .await
    .map(Json)
}

#[derive(Deserialize, Default)]
struct ForceBody {
    #[serde(default)]
    force: bool,
}

// ── By-id field access (repo-level: the row id is unique per repo) ────────────

/// 404 unless field row `id` exists in this repo; returns its owning metarecord.
fn field_owner(conn: &rusqlite::Connection, id: i64) -> Result<Uuid, ApiError> {
    db::metarecord_of_field(conn, id)?
        .ok_or_else(|| ApiError::not_found(format!("Field {id} not found")))
}

/// `GET /repos/:repo/fields/:id` — read one field row by its id (`mf field get`).
async fn get_field_by_id(
    State(state): State<Arc<AppState>>,
    Path((repo, id)): Path<(String, i64)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = repo_state.conn.lock_recover();
        let row = db::get_field_row_by_id(&conn, id)?
            .ok_or_else(|| ApiError::not_found(format!("Field {id} not found")))?;
        Ok(Json(json!({"id": row.id, "name": row.name, "value": row.value})))
    })
    .await
}

#[derive(Deserialize)]
struct PatchFieldByIdBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    value: Option<Value>,
    #[serde(default)]
    force: bool,
}

/// `PATCH /repos/:repo/fields/:id` — change a row's name and/or value in place,
/// keeping its id (`mf field set`). The value type is validated against the
/// target name; reserved names (old or new) need `force`.
async fn patch_field_by_id(
    State(state): State<Arc<AppState>>,
    Path((repo, id)): Path<(String, i64)>,
    payload: Result<Json<PatchFieldByIdBody>, JsonRejection>,
) -> Result<Json<MetaRecord>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        repo_state.ensure_writable()?;
        let mut conn = repo_state.conn.lock_recover();
        let uuid = field_owner(&conn, id)?;
        let old = db::get_field_row_by_id(&conn, id)?
            .ok_or_else(|| ApiError::not_found(format!("Field {id} not found")))?;
        let new_name = body.name.clone().unwrap_or_else(|| old.name.clone());
        let new_value = body.value.clone().unwrap_or_else(|| old.value.clone());
        check_writable(&old.name, body.force)?;
        check_writable(&new_name, body.force)?;

        let mut writer = Writer::begin(&mut conn, repo_uuid, None)?;
        writer.rename_field(uuid, id, &new_name, new_value)?;
        validate_schema(
            repo_state,
            writer.connection(),
            uuid,
            &[old.name.clone(), new_name.clone()],
        )?;
        let tree_touched = writer.touched_tree();
        writer.commit()?;
        if tree_touched {
            repo_state.lock_cache().populate(&conn)?;
        }
        metarecord_response(&conn, uuid).map(Json)
    })
    .await
}

/// `DELETE /repos/:repo/fields/:id` — remove one row by id (`mf field delete`).
async fn delete_field_by_id(
    State(state): State<Arc<AppState>>,
    Path((repo, id)): Path<(String, i64)>,
    payload: Option<Json<ForceBody>>,
) -> Result<StatusCode, ApiError> {
    let force = payload.map(|Json(b)| b.force).unwrap_or(false);
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        repo_state.ensure_writable()?;
        let mut conn = repo_state.conn.lock_recover();
        let uuid = field_owner(&conn, id)?;
        let row = db::get_field_row_by_id(&conn, id)?
            .ok_or_else(|| ApiError::not_found(format!("Field {id} not found")))?;
        check_writable(&row.name, force)?;
        let mut writer = Writer::begin(&mut conn, repo_uuid, None)?;
        writer.delete_field(uuid, id)?;
        validate_schema(repo_state, writer.connection(), uuid, std::slice::from_ref(&row.name))?;
        let tree_touched = writer.touched_tree();
        writer.commit()?;
        if tree_touched {
            repo_state.lock_cache().populate(&conn)?;
        }
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}
