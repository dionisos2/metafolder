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

use metafolder_core::metarecord::{Field, FieldType, MetaRecord, Value, ZERO_UUID};
use metafolder_core::sync::MutexExt;

use metafolder_core::query::Query as MetaQuery;

use crate::db;
use crate::error::ApiError;
use crate::log::Writer;
use crate::orphans;
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
        .route("/repos/:repo/metarecords/:uuid/mf-sync", get(get_record_mf_sync))
        .route(
            "/repos/:repo/fields/:id",
            get(get_field_by_id).patch(patch_field_by_id).delete(delete_field_by_id),
        )
        .route("/repos/:repo/retype", post(retype_field))
        .route("/repos/:repo/fields", get(list_fields))
        .route("/repos/:repo/tree/roots", get(tree_roots))
        .route("/repos/:repo/tree/children", get(tree_children))
        .route("/repos/:repo/tree/resolve-path", post(resolve_tree_path))
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
        .route("/repos/:repo/log/revisions/:rev_id", get(get_revision).patch(patch_revision))
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
        .route("/repos/:repo/mounts", get(mounts))
        .route("/repos/:repo/orphans/scan", post(orphans_scan))
        .route("/repos/:repo/orphans/clear", post(orphans_clear))
        .route("/repos/:repo/track", post(track))
        .route("/repos/:repo/eligibility", post(eligibility_explain))
        .route("/repos/:repo/ignore/effective", get(effective_ignore))
        // ── Cross-repo sync (spec-sync) ─────────────────────────────────────
        .route("/sync/:a/:b/links", get(sync_list_links).post(sync_create_link))
        .route("/sync/:a/:b/links/:link", get(sync_get_link).delete(sync_delete_link))
        .route("/sync/:a/:b/links/commit", post(sync_commit))
        .route("/sync/:a/:b/status", get(sync_status))
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

pub fn hex(uuid: Uuid) -> String {
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
        let uuids = resolve_query_uuids(repo_state, &conn, &mut cache, &body.query, &|| false)?;
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

/// `GET /repos/:repo/metarecords/:uuid/mf-sync`: the record's effective
/// `mf_sync` mode (spec-sync) — `external` when an external tool owns its
/// content, else `internal`. Resolved from the record's `mfr_path` position
/// (inherited like `mf_watch`); a record with no `mfr_path` is `internal`.
async fn get_record_mf_sync(
    State(state): State<Arc<AppState>>,
    Path((repo, uuid)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let uuid = parse_uuid(&uuid)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = repo_state.conn.lock_recover();
        let mut cache = repo_state.lock_cache();
        let paths = cache.paths_of(&conn, "mfr_path", uuid)?;
        let mode = match paths.first() {
            // `paths_of` and `resolve_mf_sync` (eligibility) share the same
            // leading-"/"-rooted form (`""` = root, `/a/b` = nested).
            Some(p) => crate::eligibility::resolve_mf_sync(&conn, &mut cache, p)?,
            None => "internal".to_string(),
        };
        Ok(Json(json!({ "mf_sync": mode })))
    })
    .await
}

#[derive(Deserialize)]
struct ResolvePathBody {
    #[serde(default = "default_tree_field")]
    field: String,
    /// Repo-root-relative path (components split on `/`, in the Path string
    /// format): leading-`/`-rooted for the filesystem forest (e.g. `/music/jazz`,
    /// whose first component is the empty root name), no leading `/` for a
    /// named-root forest such as tags (e.g. `tag1/tag2`).
    path: String,
}

/// `POST /repos/:repo/tree/resolve-path`: resolves a repo-root-relative path in
/// the TreeRef `field` (default `mfr_path`) to the uuid of the node at that
/// path, or `null` when no such node exists. The inverse of `resolve-tree`
/// (uuid → paths). Used to set a TreeRef value from a path: resolve the parent
/// path to a uuid, then post `{parent, name}`. Paths are unique within a forest
/// (sibling names are unique), so at most one node matches. One in-memory
/// round-trip through the tree cache.
async fn resolve_tree_path(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<ResolvePathBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let Json(body) = payload?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = repo_state.conn.lock_recover();
        let mut cache = repo_state.lock_cache();
        let uuid = cache.resolve_path(&conn, &body.field, &body.path)?;
        Ok(Json(json!({ "uuid": uuid.map(hex) })))
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
        // The data-derived catalog is served from the in-memory index. When it
        // is already warm (present) we bring it up to HEAD first — a forward
        // delta after a write is incremental and cheap, the same refresh
        // `run_query_filter` performs. This matters because the GUI re-warms the
        // catalog on every change it sees, right after a write when the index is
        // one op stale: serving that from the O(rows) `SELECT DISTINCT` table
        // scan is a multi-second stall on a large repository. Only when the
        // index is *cold* (absent — e.g. before warmup finishes) do we fall back
        // to the DB scan, rather than building the whole index synchronously here
        // just to enumerate field names.
        let data = {
            let mut index_guard = repo_state.index.lock_recover();
            match index_guard.as_mut() {
                Some(index) => {
                    index.refresh(&conn, &|| false)?;
                    index.field_catalog(None)
                }
                None => db::distinct_field_names(&conn, None)?,
            }
        };
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

#[derive(Deserialize)]
struct TreeChildrenParams {
    #[serde(default = "default_tree_field")]
    field: String,
    /// The parent node's metarecord uuid (hex), whose direct children are listed.
    uuid: String,
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
        let out: Vec<serde_json::Value> = roots
            .into_iter()
            .map(|(uuid, name)| json!({"uuid": hex(uuid), "name": name}))
            .collect();
        Ok(Json(serde_json::Value::Array(out)))
    })
    .await
}

/// `GET /repos/:repo/tree/children?field=<field>&uuid=<hex>`: the direct
/// children of one TreeRef node as `[{"uuid": "<hex>", "name": "<name>"}, ...]`,
/// ordered by name. Served from the (eager) tree cache in memory, falling back
/// to one DB query. Lets a client list a directory's tracked entries — names +
/// their metarecords — in one call, without a query and a per-record fetch of
/// every child.
async fn tree_children(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    Query(params): Query<TreeChildrenParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let parent = parse_uuid(&params.uuid)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = repo_state.conn.lock_recover();
        let mut cache = repo_state.lock_cache();
        let mut children = cache.children_of(&conn, &params.field, parent)?;
        children.sort_by(|a, b| a.0.cmp(&b.0));
        let out: Vec<serde_json::Value> = children
            .into_iter()
            .map(|(name, uuid)| json!({"uuid": hex(uuid), "name": name}))
            .collect();
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

/// Shared scaffold for the single-metarecord write handlers (`patch`, `append`,
/// `replace`, `delete`): runs on the blocking pool, gates on repository
/// writability, opens a logged [`Writer`], lets `write` resolve the touched
/// field name(s) and perform the mutation, then runs schema delta validation
/// over those names and commits. Returns the resulting metarecord (handlers that
/// answer 204 simply discard it). A validation failure or any closure error
/// drops the Writer, rolling the whole write back.
///
/// With an optional optimistic-concurrency precondition (spec-data-model
/// "Conditional writes"): when `expected_version` is given and the metarecord's
/// current version differs, the write is rejected with `409` (nothing written,
/// no revision), fenced by the transaction's exclusive lock.
async fn write_record_checked<F>(
    state: &AppState,
    repo_uuid: Uuid,
    uuid: Uuid,
    expected_version: Option<u64>,
    write: F,
) -> Result<MetaRecord, ApiError>
where
    F: FnOnce(&mut Writer) -> Result<Vec<String>, ApiError> + Send + 'static,
{
    with_repo(state, repo_uuid, move |repo_state| {
        repo_state.ensure_writable()?;
        let mut conn = repo_state.conn.lock_recover();
        let mut writer = Writer::begin(&mut conn, None)?;
        ensure_version(writer.connection(), uuid, expected_version)?;
        let touched = write(&mut writer)?;
        validate_schema(repo_state, writer.connection(), uuid, &touched)?;
        let tree_touched = writer.touched_tree();
        let watch_touched = writer.touched_watch();
        writer.commit()?;
        // Manual TreeRef writes bypass the watcher's incremental cache upkeep;
        // rebuild the complete cache so reads stay correct (no-op if absent).
        if tree_touched {
            repo_state.lock_cache().populate(&conn)?;
        }
        // A change to mf_watch/mf_ignore may have grown or shrunk the watched
        // scope; re-place the inotify watches accordingly.
        if watch_touched {
            repo_state.refresh_watches(&conn);
        }
        metarecord_response(&conn, uuid)
    })
    .await
}

/// The optional `?expected_version=` optimistic-concurrency query parameter on
/// single-record write endpoints.
#[derive(serde::Deserialize, Default)]
struct ExpectedVersion {
    expected_version: Option<u64>,
}

/// 409 (no revision written) when `expected` is given and the metarecord's
/// current version differs — the optimistic-concurrency precondition used by
/// cross-repo sync propagation (spec-data-model "Conditional writes").
fn ensure_version(
    conn: &rusqlite::Connection,
    uuid: Uuid,
    expected: Option<u64>,
) -> Result<(), ApiError> {
    if let Some(expected) = expected {
        let current = db::get_version(conn, uuid)?;
        if current != Some(expected) {
            return Err(ApiError::conflict(format!(
                "expected_version {expected} but current is {}",
                current.map_or("absent".to_string(), |v| v.to_string())
            )));
        }
    }
    Ok(())
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
        // Wire-protocol version (spec-gui): a client compares this against its
        // own `core::API_VERSION` and refuses/warns on a mismatch. Distinct
        // from `version` (the crate semver), which does not track the contract.
        "api_version": metafolder_core::API_VERSION,
        "repos": state.list_repos(false).len(),
    }))
}

/// The optional `?all=true` query parameter on `GET /repos` (include system repos).
#[derive(serde::Deserialize, Default)]
struct ListReposParams {
    #[serde(default)]
    all: bool,
}

async fn list_repos(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ListReposParams>,
) -> Json<serde_json::Value> {
    Json(serde_json::to_value(state.list_repos(params.all)).expect("repo list serialization"))
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
        CancelOutcome::NotFound => Err(ApiError::not_found(format!("Task not found: {task_uuid}"))),
    }
}

#[derive(Deserialize)]
struct InitBody {
    root: PathBuf,
    #[serde(default)]
    metafolder: Option<PathBuf>,
    #[serde(default)]
    name: Option<String>,
    /// Create a daemon-internal repository (e.g. a sync plan repo, spec-sync):
    /// hidden from `GET /repos` unless `?all=true`.
    #[serde(default)]
    system: bool,
}

async fn init_repo(
    State(state): State<Arc<AppState>>,
    payload: Result<Json<InitBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(body) = payload?;
    // An empty/whitespace name falls back to the directory-derived default.
    let name = body.name.filter(|n| !n.trim().is_empty());
    let uuid = tokio::task::spawn_blocking(move || {
        state.init_repo(&body.root, body.metafolder.as_deref(), name.as_deref(), body.system)
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
    // fallback); the response returns its uuid immediately, plus the warmup's
    // task id (null when already warm) so the CLI can wait on it.
    let task_id = spawn_load_warmup(state_for_warmup, uuid);
    Ok(Json(json!({
        "repo_uuid": hex(uuid),
        "task_id": task_id.map(|id| id.as_simple().to_string()),
    })))
}

/// Spawns the background warmup task for a freshly loaded repository and
/// returns its task id. A no-op returning `None` when the repository is
/// already warm (a redundant load); when a warmup is already running, returns
/// the running task's id so the caller can wait on it.
fn spawn_load_warmup(state: Arc<AppState>, repo_uuid: Uuid) -> Option<Uuid> {
    let repo_state = state.repo(repo_uuid).ok()?;
    if repo_state.lock_cache().is_complete() {
        return None; // already warm (e.g. re-load of a loaded repo)
    }
    let Some(task_id) = repo_state.tasks.start_unique(TaskKind::Load) else {
        // A warmup is already in progress: hand back its id.
        return repo_state.tasks.active_id(TaskKind::Load);
    };
    tokio::task::spawn_blocking(move || {
        repo_state.tasks.mark_running(task_id);
        repo_state.warmup(&|phase, done, total| {
            repo_state.tasks.set_progress(task_id, phase, done, total);
        });
        repo_state.tasks.finish(task_id, None);
    });
    Some(task_id)
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
        let limit = params.limit;

        let mut ops: Vec<crate::log::OpRow> = match mode {
            "tree" => crate::log::all_ops(&conn)?,
            "linear" => match head {
                None => vec![],
                Some(head) => {
                    // With a limit, bound the ancestry walk so a huge log is not
                    // read in full (spec-event-log "limit"): most recent first,
                    // then reversed to oldest-first.
                    let mut chain = match limit {
                        Some(l) => crate::log::ancestry_ops_limited(&conn, head, l)?,
                        None => crate::log::ancestry_ops(&conn, head)?,
                    };
                    chain.reverse(); // root → HEAD, oldest first
                    chain
                }
            },
            // The active line through HEAD: ancestry plus the forward
            // continuation to the most-recent leaf (keeps the redo future
            // visible, hides divergent branches).
            "active" => match head {
                None => vec![],
                Some(head) => match limit {
                    // Fast path: a limited request whose HEAD has no forward
                    // continuation (the common case — no rollback) has an active
                    // line equal to its ancestry, so bound that instead of
                    // scanning every operation to rebuild forward branches
                    // (`active_line_ops` loads the whole log).
                    Some(l) if !crate::log::has_children(&conn, head)? => {
                        let mut chain = crate::log::ancestry_ops_limited(&conn, head, l)?;
                        chain.reverse();
                        chain
                    }
                    _ => crate::log::active_line_ops(&conn, head)?,
                },
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
        // Repository-wide totals, so a client showing a bounded window (the GUI
        // log panel fetches only the most recent `limit` operations) can still
        // report how much log there is. Two counts off the primary keys — not
        // the size of the returned window, and unaffected by `limit`/`mode`.
        let total_operations: i64 = conn
            .query_row("SELECT COUNT(*) FROM operation", [], |r| r.get(0))
            .map_err(anyhow::Error::from)?;
        let total_revisions: i64 = conn
            .query_row("SELECT COUNT(*) FROM revision", [], |r| r.get(0))
            .map_err(anyhow::Error::from)?;

        Ok(Json(json!({
            "head": head,
            "operations": op_values,
            "revisions": revisions,
            "total_operations": total_operations,
            "total_revisions": total_revisions,
        })))
    })
    .await
}

#[derive(Deserialize)]
struct SinceParams {
    #[serde(default)]
    op: Option<i64>,
    /// Cap on the number of operations carried in one response. A delta larger
    /// than this is not streamed: `truncated` is set and `operations` is empty,
    /// so the client does one coarse whole-repo refresh instead of invalidating
    /// tens of thousands of records op-by-op (a large reconcile).
    #[serde(default)]
    limit: Option<i64>,
}

/// Default cap on the change-feed delta size (see [`SinceParams::limit`]).
const SINCE_DEFAULT_LIMIT: i64 = 500;

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
        let limit = params.limit.unwrap_or(SINCE_DEFAULT_LIMIT).max(0);
        let mut truncated = false;
        let operations = match params.op {
            Some(since) => {
                if crate::log::ops_since_count(&conn, since)? > limit {
                    // Oversized delta: signal a coarse refresh instead of
                    // streaming every operation (a large reconcile would flood
                    // the client).
                    truncated = true;
                    Vec::new()
                } else {
                    let ops = crate::log::ops_since(&conn, since)?;
                    let mut out = Vec::with_capacity(ops.len());
                    for op in &ops {
                        out.push(op_json(&conn, op, false)?);
                    }
                    out
                }
            }
            None => Vec::new(),
        };
        Ok(Json(json!({"head": head, "operations": operations, "truncated": truncated})))
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
            let head =
                head.ok_or_else(|| ApiError::not_found("the history is empty (no HEAD revision)"))?;
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
                .prepare("SELECT id FROM operation WHERE rev_id = ?1 ORDER BY seq")
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
        // Observation-only task (spec-tasks), like prune: rollback rewrites
        // arbitrary state under the connection lock and rebuilds the tree cache.
        observed(repo_state, TaskKind::Rollback, "rolling back", |repo_state| {
            repo_state.ensure_writable()?;
            let mut conn = repo_state.conn.lock_recover();
            let resolved = crate::log::resolve_target(&conn, &target)?;
            let result = crate::log::navigate(&mut conn, resolved)?;
            // Navigation rewrites tree positions arbitrarily: rebuild the cache
            // from the new state (keeps it complete; `populate` clears first).
            repo_state.lock_cache().populate(&conn)?;
            Ok(Json(result))
        })
    })
    .await
}

/// Runs `f` as an observation-only task (spec-tasks): registers a task of
/// `kind`, marks it running with `phase`, and records its terminal state. Like
/// `query`, the operation's result travels with the HTTP response, so the task
/// carries no result payload and its counts stay unknown. Used for the
/// synchronous, connection-lock-holding log operations (prune, rollback) so
/// other clients can see why their work is queued.
fn observed<T>(
    repo_state: &RepoState,
    kind: TaskKind,
    phase: &'static str,
    f: impl FnOnce(&RepoState) -> Result<T, ApiError>,
) -> Result<T, ApiError> {
    let task = repo_state.tasks.start(kind);
    repo_state.tasks.mark_running(task);
    repo_state.tasks.set_progress(task, phase, None, None);
    let outcome = f(repo_state);
    match &outcome {
        Ok(_) => repo_state.tasks.finish(task, None),
        Err(e) => repo_state.tasks.fail(task, &e.message),
    }
    outcome
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
        // Observation-only task (spec-tasks): the result travels with this
        // response, so the task carries no result payload and its counts stay
        // unknown. Registered because prune holds the connection lock and can be
        // long on a large log, so other clients see why their work is queued.
        observed(repo_state, TaskKind::Prune, "pruning", |repo_state| {
            repo_state.ensure_writable()?;
            let mut conn = repo_state.conn.lock_recover();
            let resolved = crate::log::resolve_target(&conn, &target)?
                .ok_or_else(|| ApiError::bad_request("cannot prune to the empty state"))?;
            let (ops, revisions) = crate::log::prune(&mut conn, mode, resolved)
                .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
            Ok(Json(json!({"pruned_operations": ops, "pruned_revisions": revisions})))
        })
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
    // For an inverse (rollback) step, expose the metarecord version this step
    // restores to (`entity_version_before`). The CLI matches it against a trash
    // entry's recorded version to auto-restore the exact file the deletion
    // displaced (spec-trash "rollback auto-restore"). Omitted on forward (redo)
    // steps, so auto-restore never fires while re-applying a deletion.
    if matches!(dir, crate::log::NavDir::Inverse) {
        if let Some(v) = op.entity_version_before {
            value["entity_version_before"] = json!(v);
        }
        // One event can write several fields of one record (orphaning writes
        // `mfr_path` *and* `mfr_path_old`), so each op restores to its own
        // intermediate version while a trash entry only ever recorded the
        // version the record held before the whole revision. Expose that one
        // too: it is what the CLI correlates the entry against, identically on
        // every op of the revision.
        if let Some(v) = crate::log::entity_version_before_revision(conn, op)? {
            value["entity_version_before_revision"] = json!(v);
        }
    }
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
        let mut by_type: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
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
            let new_head = crate::log::coordinated_step(&mut conn, target, skip)?;
            // The step rewrote tree positions arbitrarily: rebuild the cache
            // from the new state (keeps it complete; `populate` clears first).
            repo_state.lock_cache().populate(&conn)?;
            let next = crate::log::nav_path(&conn, new_head, target)?;
            if let Some((op, dir)) = next.first() {
                let mut cache = repo_state.lock_cache();
                let op_json = action_op_json(&conn, &mut cache, &repo_state.config.root, op, *dir)?;
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
        let loaded = crate::schema::load_for_repo(&repo_state.metafolder_dir, &repo_state.config)
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
        let guard = repo_state.schema.lock_recover();
        let mut violations: Vec<serde_json::Value> = Vec::new();
        let mut checked = 0usize;
        if let Some(schema) = guard.as_ref() {
            // Which metarecords to validate. A scoped check (`query`) walks that
            // (usually small) result set. A whole-repo check does NOT scan every
            // metarecord: it validates only the ones the index-served candidate
            // queries flag as *able* to violate — nearly none on a healthy repo,
            // so the once-per-open heads-up stays cheap even at 400k records.
            let uuids = match &body.query {
                Some(query) => {
                    let mut cache = repo_state.lock_cache();
                    resolve_query_uuids(repo_state, &conn, &mut cache, query, &|| false)?
                }
                None => crate::schema::violation_candidates(
                    schema,
                    &conn,
                    // A cap on candidates suffices once we only need `limit + 1`
                    // violations to report truncation; unbounded for a full audit.
                    body.limit.map(|l| l + 1),
                )?,
            };
            let fields = schema.constrained_fields();
            // Collect up to `limit + 1` so truncation is exact, then stop.
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
            // For a whole-repo check that ran to completion, report the true
            // repository size as `checked`: the candidate set is an exhaustive
            // superset of the violators, so every metarecord was effectively
            // examined (the non-candidates are provably clean).
            let truncated = body.limit.is_some_and(|l| violations.len() > l);
            if body.query.is_none() && !truncated {
                checked = db::count_metarecords(&conn)?;
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
    /// Extract embedded `mfr_meta_*` fields for files not yet analysed
    /// (default true; spec-platform "Embedded metadata extraction").
    #[serde(default = "default_true")]
    metadata: bool,
    /// Refresh the stat-derived `mfr_*` fields of files/directories still at
    /// their recorded path, catching in-place edits (default true).
    #[serde(default = "default_true")]
    refresh: bool,
}

impl Default for ReconcileBody {
    fn default() -> Self {
        Self { metarecord: None, threshold: None, mime: true, metadata: true, refresh: true }
    }
}

/// `POST /repos/:repo/reconcile`: starts a reconcile as a background task
/// (spec-tasks). Returns `202 Accepted` with the task id immediately; progress
/// and the final `ReconcileResult` are observed via `GET …/tasks/:id`. A
/// concurrent reconcile is rejected with `409`. With `metarecord` in the body
/// the reconcile is scoped to that metarecord's subtree; absent, it covers the
/// whole repository.
/// `GET /repos/:repo/mounts`: the repository's declared mount points — every
/// metarecord carrying `mfr_mount` — with the state read from disk right now
/// (spec-file-tracking "Mount status"). Read-only and cheap: one stat pair per
/// mount point, no walk. It is how a client explains a subtree that looks empty
/// or stale ("volume not mounted") instead of showing it as deleted.
async fn mounts(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = repo_state.conn.lock_recover();
        let mut cache = repo_state.lock_cache();
        let mounts = crate::mount::declared(&conn, &mut cache, &repo_state.config.root)?;
        Ok(Json(json!({ "mounts": mounts })))
    })
    .await
}

/// `POST /repos/:repo/orphans/scan`: read-only disk scan for tracked
/// metarecords whose `mfr_path` is definitely gone (spec-file-tracking "Orphan
/// scan"). Returns `{count, orphans: [{uuid, stale_path}]}`. Unlike reconcile it
/// writes nothing; unlike a query it consults the filesystem, so it is a
/// distinct operation rather than a predicate.
async fn orphans_scan(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let orphans = orphans::scan_orphans(repo_state)?;
        Ok(Json(json!({ "count": orphans.len(), "orphans": orphans })))
    })
    .await
}

#[derive(Deserialize)]
struct OrphansClearBody {
    /// The metarecords to orphan — typically the uuids a prior scan returned.
    #[serde(default)]
    uuids: Vec<String>,
}

/// `POST /repos/:repo/orphans/clear`: orphan the given metarecords whose file is
/// still gone — snapshot `mfr_path_old`, set `mfr_path` to `Nothing`, cascade to
/// descendants (spec-file-tracking "Orphan scan"). Re-verifies each against the
/// disk, so a since-recreated file is skipped. Returns `{cleared}`.
async fn orphans_clear(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<OrphansClearBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let Json(body) = payload?;
    let uuids = body.uuids.iter().map(|s| parse_uuid(s)).collect::<Result<Vec<_>, _>>()?;
    with_repo(&state, repo_uuid, move |repo_state| {
        repo_state.ensure_writable()?;
        let cleared = orphans::clear_orphans(repo_state, &uuids)?;
        Ok(Json(json!({ "cleared": cleared })))
    })
    .await
}

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
    let task_id = repo_state.tasks.start_unique(TaskKind::Reconcile).ok_or_else(|| {
        ApiError::conflict("a reconcile is already in progress for this repository")
    })?;

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
                body.metadata,
                body.refresh,
                &crate::reconcile::Reporter::new(&progress, &cancel),
            ),
            None => crate::reconcile::reconcile_full_reported(
                &repo_state,
                body.threshold,
                body.mime,
                body.metadata,
                body.refresh,
                &crate::reconcile::Reporter::new(&progress, &cancel),
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

#[derive(Deserialize)]
struct EligibilityBody {
    paths: Vec<String>,
}

/// The most paths one `POST /eligibility` call may explain. A directory
/// listing is the intended unit; a bigger batch is a client bug, and each path
/// costs an ancestor-chain walk under the repo's cache lock.
const ELIGIBILITY_MAX_PATHS: usize = 1000;

/// `POST /repos/:repo/eligibility`: read-only dry run of the watch/ignore
/// algorithm for a batch of repo-root-relative paths, each with the reason it
/// was decided (spec-file-tracking "Eligibility explain"). The whole batch
/// shares one `EligibilityCache`, so ancestor fields and compiled patterns are
/// read once for a listing.
async fn eligibility_explain(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<EligibilityBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    if body.paths.len() > ELIGIBILITY_MAX_PATHS {
        return Err(ApiError::bad_request(format!(
            "at most {ELIGIBILITY_MAX_PATHS} paths per call, got {}",
            body.paths.len()
        )));
    }
    for path in &body.paths {
        if !path.is_empty() && !path.starts_with('/') {
            return Err(ApiError::bad_request(format!(
                "path must be repo-root-relative with a leading slash: {path:?}"
            )));
        }
    }
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = repo_state.conn.lock_recover();
        let mut cache = repo_state.lock_cache();
        let mut ec = crate::eligibility::EligibilityCache::default();
        let mut results = Vec::with_capacity(body.paths.len());
        for path in &body.paths {
            let e = crate::eligibility::explain_cached(&conn, &mut cache, path, &mut ec)?;
            results.push(json!({
                "path": path,
                "eligible": e.eligible,
                "reason": e.reason.as_str(),
                "watch_scope": e.watch_scope,
                "ignore_source": e.ignore_source,
                "pattern": e.pattern,
            }));
        }
        Ok(Json(json!({ "results": results })))
    })
    .await
}

#[derive(Deserialize)]
struct EffectiveIgnoreParams {
    #[serde(default)]
    path: String,
}

/// `GET /repos/:repo/ignore/effective?path=<rel>`: the `mf_ignore` set that
/// governs a directory and where it comes from (spec-file-tracking "Effective
/// ignore set") — what a client needs to warn that writing here would shadow an
/// inherited set rather than extend it.
async fn effective_ignore(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    Query(params): Query<EffectiveIgnoreParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    if !params.path.is_empty() && !params.path.starts_with('/') {
        return Err(ApiError::bad_request(format!(
            "path must be repo-root-relative with a leading slash: {:?}",
            params.path
        )));
    }
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = repo_state.conn.lock_recover();
        let mut cache = repo_state.lock_cache();
        let e = crate::eligibility::effective_ignore(&conn, &mut cache, &params.path)?;
        Ok(Json(json!({
            "source": e.source,
            "source_uuid": e.source_uuid.map(hex),
            "direct": e.direct,
            "patterns": e.patterns,
        })))
    })
    .await
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
                return Err(ApiError::bad_request(format!(
                    "unsupported path component in {abs:?}"
                )));
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
        // Idempotent: a path already tracked returns its existing metarecord
        // uuid rather than an error, so callers can `track` without first
        // checking (spec-file-tracking "Single-metarecord track").
        if let Some(existing) = cache.resolve_path(&conn, "mfr_path", &rel)? {
            return Ok(Json(json!({"uuid": hex(existing)})));
        }
        let untracked = [Field::new("mf_watch", Value::Bool(false))];
        let mut writer = Writer::begin(&mut conn, None)?;
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
        let outcome = run_query_inner(repo_state, &body, task);
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

/// One page of query results: the metarecords, the cursor for the next page
/// (`None` at the end), and the total — present only when the body asked to
/// `count`.
type QueryPage = (Vec<Uuid>, Option<String>, Option<usize>);

/// Resolves a query's page (and optional total) through the in-memory bitmap
/// index when it is applicable, falling back to the SQL engine otherwise.
///
/// Bring the repo's in-memory index up to the current HEAD (building it on the
/// first use), then hand back a shared reference to it. The single acquisition
/// point for the two live-query call sites (`field_catalog` and
/// `run_query_filter`) so they cannot drift.
fn ensure_index<'g>(
    conn: &rusqlite::Connection,
    guard: &'g mut Option<crate::index::RepoIndex>,
    cancel: &dyn Fn() -> bool,
) -> Result<&'g crate::index::RepoIndex, ApiError> {
    match guard.as_mut() {
        // Already built: bring it up to the current HEAD (incrementally when the
        // delta is a forward extension, else an internal full rebuild). A full
        // rebuild polls `cancel` so a Stop on a query that triggered it works.
        Some(index) => index.refresh(conn, cancel)?,
        None => *guard = Some(crate::index::RepoIndex::build_reported(conn, &|_, _| {}, cancel)?),
    }
    Ok(guard.as_ref().expect("index built above"))
}

/// Resolves a query's index seeds and rewrites its index-unsupported text leaves
/// — the shared preparation feeding the bitmap index, used by both the paginated
/// query path ([`run_query_filter`]) and the whole-set resolution
/// ([`resolve_query_uuids`]). Path targets and exact-node operands resolve to
/// metarecords through the tree cache; the text leaves the index cannot serve
/// (Matches, Osm Direct, multi-term Osm Path) are pre-resolved to `UuidIn` sets.
/// `full_set` forces that rewrite (a whole-set resolution wants every match, so
/// the SQL early-`limit` optimisation that keeps a bare-leaf *page* cheaper does
/// not apply); an Osm-Path leaf anywhere in the query forces it too — see
/// [`crate::index::contains_osm_path`].
fn prepare_indexed_query<'a>(
    conn: &rusqlite::Connection,
    cache: &mut crate::tree_cache::TreeCache,
    query: &MetaQuery,
    full_set: bool,
) -> Result<(crate::index::QueryRoots<'a>, MetaQuery), ApiError> {
    let mut roots = crate::index::QueryRoots::new();
    let mut path_targets = Vec::new();
    crate::index::collect_path_targets(query, &mut path_targets);
    for (field, path) in path_targets {
        if let Some(uuid) = cache.resolve_path(conn, &field, &path)? {
            roots.path.insert((field, path), uuid);
        }
    }
    // Exact-node `Eq`/`Neq` operands (`mfr_path = "/a/b.txt"`): resolved through
    // the same cache, but the entry is always inserted — including a `None` for a
    // path that is no node — so the index can tell "resolved to nothing" (empty
    // result) from "nobody resolved it" (defer to SQL).
    let mut node_paths = Vec::new();
    crate::index::collect_node_paths(query, &mut node_paths);
    for (field, path) in node_paths {
        let node = cache.resolve_path(conn, &field, &path)?;
        roots.node.insert((field, path), node);
    }
    let has_osm_path = crate::index::contains_osm_path(query);
    let indexed = if full_set || has_osm_path {
        query_exec::resolve_index_leaves(conn, cache, query)?
    } else {
        query.clone()
    };
    Ok((roots, indexed))
}

/// Resolves a query to *all* its matching uuids, index-accelerated — the
/// set-layer counterpart of [`run_query_filter`] (batch field writes, query
/// delete, tree resolution) which operate on the whole match set rather than a
/// page. Shares [`prepare_indexed_query`] so these writes get the same bitmap
/// acceleration as reads; an unsupported shape falls back to the SQL engine.
fn resolve_query_uuids(
    repo_state: &RepoState,
    conn: &rusqlite::Connection,
    cache: &mut crate::tree_cache::TreeCache,
    query: &MetaQuery,
    cancel: &dyn Fn() -> bool,
) -> Result<Vec<Uuid>, ApiError> {
    query_exec::validate_query(query)?;
    let (roots, indexed) = prepare_indexed_query(conn, cache, query, true)?;
    let mut index_guard = repo_state.index.lock_recover();
    let index = ensure_index(conn, &mut index_guard, cancel)?;
    match index.evaluate_page_with_roots(&indexed, &[], None, None, &roots) {
        Ok((uuids, _)) => Ok(uuids),
        Err(_unsupported) => Ok(query_exec::execute(conn, cache, query, &[], None, None)?.0),
    }
}

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
    body: &QueryBody,
    cancel: &dyn Fn() -> bool,
) -> Result<QueryPage, ApiError> {
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

    // Resolve the query's index seeds (Path targets, exact-node operands) and
    // pre-resolve the text leaves the index cannot serve. The engine choice must
    // be a function of the query *shape* alone (`full_set = false` here): the
    // list asks for `count` on the first page only, and if that toggled the
    // preparation, page 1 and page 2 could run on different engines and reject
    // each other's cursor.
    let (mut roots, indexed_query) = prepare_indexed_query(conn, cache, &body.query, false)?;
    // Full-path sort keys for a `tree_ref` sort key, rebuilt from the resident
    // forest (spec-data-model "Sort specification"). Borrows the cache, so the
    // borrow must end before the SQL fallback below takes it mutably again.
    let sort_keys = crate::tree_cache::SortKeys::new(cache);
    roots.keys = Some(&sort_keys);

    let mut index_guard = repo_state.index.lock_recover();
    let index = ensure_index(conn, &mut index_guard, cancel)?;
    // The index build/refresh above is the heavy phase on a large repo; if a
    // Stop landed during it, don't start the (also non-trivial) evaluation.
    if cancel() {
        return Err(ApiError::conflict("query cancelled"));
    }

    // With `count` the page and the total come from a single evaluation; without
    // it, only the page is computed.
    let paged = if body.count {
        index
            .page_and_count(&indexed_query, &sort_by, body.limit, body.cursor.as_deref(), &roots)
            .map(|(uuids, next, total)| (uuids, next, Some(total as usize)))
    } else {
        index
            .evaluate_page_with_roots(
                &indexed_query,
                &sort_by,
                body.limit,
                body.cursor.as_deref(),
                &roots,
            )
            .map(|(uuids, next)| (uuids, next, None))
    };
    match paged {
        Ok(page) => Ok(page),
        Err(_unsupported) => {
            let (uuids, next_cursor) = query_exec::execute(
                conn,
                cache,
                &body.query,
                &body.sort,
                body.limit,
                body.cursor.as_deref(),
            )?;
            // Counting here means running the whole CTE chain a second time, so
            // skip it when the page already proves the total: a first page that
            // came back short is the entire match set.
            let total = match (body.count, body.cursor.is_none() && next_cursor.is_none()) {
                (false, _) => None,
                (true, true) => Some(uuids.len()),
                (true, false) => Some(query_exec::count(conn, cache, &body.query)?),
            };
            Ok((uuids, next_cursor, total))
        }
    }
}

fn run_query_inner(
    repo_state: &RepoState,
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
        // Cooperative cancellation (spec-tasks): the SQLite interrupt only aborts
        // a running statement, so it cannot stop the index build/evaluation or the
        // result assembly (all Rust). Those phases poll this flag instead.
        let cancel = || repo_state.tasks.is_cancel_requested(task);
        let mut cache = repo_state.lock_cache();
        let (uuids, next_cursor, total) =
            run_query_filter(repo_state, &conn, &mut cache, body, &cancel)?;
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
                query_exec::assemble_selected(&conn, &uuids, fields_filter.as_deref(), &cancel)?
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
fn resolved_values(
    value: Option<Value>,
    values: Option<Vec<Value>>,
) -> Result<Vec<Value>, ApiError> {
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
        let uuids = resolve_query_uuids(repo_state, &conn, &mut cache, &body.query, &|| false)?;
        drop(cache);

        let mut writer = Writer::begin(&mut conn, None)?;
        for uuid in &uuids {
            writer.set_field_multi(*uuid, &body.name, rows.clone())?;
            validate_schema(
                repo_state,
                writer.connection(),
                *uuid,
                std::slice::from_ref(&body.name),
            )?;
        }
        let tree_touched = writer.touched_tree();
        let watch_touched = writer.touched_watch();
        writer.commit()?;
        if tree_touched {
            repo_state.lock_cache().populate(&conn)?;
        }
        if watch_touched {
            repo_state.refresh_watches(&conn);
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
        let uuids = resolve_query_uuids(repo_state, &conn, &mut cache, &body.query, &|| false)?;
        drop(cache);

        let mut writer = Writer::begin(&mut conn, None)?;
        for uuid in &uuids {
            writer.append_field(*uuid, &body.name, value.clone())?;
            validate_schema(
                repo_state,
                writer.connection(),
                *uuid,
                std::slice::from_ref(&body.name),
            )?;
        }
        let tree_touched = writer.touched_tree();
        let watch_touched = writer.touched_watch();
        writer.commit()?;
        if tree_touched {
            repo_state.lock_cache().populate(&conn)?;
        }
        if watch_touched {
            repo_state.refresh_watches(&conn);
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
        let uuids = resolve_query_uuids(repo_state, &conn, &mut cache, &body.query, &|| false)?;
        drop(cache);

        let mut writer = Writer::begin(&mut conn, None)?;
        let mut changed = 0usize;
        for uuid in &uuids {
            if writer.delete_fields_valued(*uuid, &body.name, &value)? > 0 {
                changed += 1;
                validate_schema(
                    repo_state,
                    writer.connection(),
                    *uuid,
                    std::slice::from_ref(&body.name),
                )?;
            }
        }
        let tree_touched = writer.touched_tree();
        let watch_touched = writer.touched_watch();
        writer.commit()?;
        if tree_touched {
            repo_state.lock_cache().populate(&conn)?;
        }
        if watch_touched {
            repo_state.refresh_watches(&conn);
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
        let uuids = resolve_query_uuids(repo_state, &conn, &mut cache, &body.query, &|| false)?;
        drop(cache);

        let mut writer = Writer::begin(&mut conn, None)?;
        let mut changed = 0usize;
        for uuid in &uuids {
            if writer.delete_fields_named(*uuid, &body.name)? > 0 {
                changed += 1;
                validate_schema(
                    repo_state,
                    writer.connection(),
                    *uuid,
                    std::slice::from_ref(&body.name),
                )?;
            }
        }
        let tree_touched = writer.touched_tree();
        let watch_touched = writer.touched_watch();
        writer.commit()?;
        if tree_touched {
            repo_state.lock_cache().populate(&conn)?;
        }
        if watch_touched {
            repo_state.refresh_watches(&conn);
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
        let mut writer = Writer::begin(&mut conn, None)?;
        let summary = writer.retype_field(&name, to)?;
        let tree_touched = writer.touched_tree();
        let watch_touched = writer.touched_watch();
        writer.commit()?;
        if tree_touched {
            repo_state.lock_cache().populate(&conn)?;
        }
        if watch_touched {
            repo_state.refresh_watches(&conn);
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

/// `POST /repos/:repo/query/delete` — deletes every metarecord matching `query` in a
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
        let uuids = resolve_query_uuids(repo_state, &conn, &mut cache, &body.query, &|| false)?;
        drop(cache);

        let mut writer = Writer::begin(&mut conn, None)?;
        for uuid in &uuids {
            writer.delete_metarecord(*uuid)?;
        }
        let tree_touched = writer.touched_tree();
        let watch_touched = writer.touched_watch();
        writer.commit()?;
        // Deleting a metarecord with a TreeRef removes tree nodes: rebuild the
        // complete cache so reads stay correct (no-op if absent).
        if tree_touched {
            repo_state.lock_cache().populate(&conn)?;
        }
        // Deleting a record that carried mf_watch/mf_ignore can shrink the
        // watched scope; re-place the inotify watches accordingly.
        if watch_touched {
            repo_state.refresh_watches(&conn);
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
    /// Optional caller-supplied UUID (sync bare-record creation, spec-sync).
    /// Rejected with 409 if a metarecord already has it.
    #[serde(default)]
    uuid: Option<String>,
}

async fn create_record_endpoint(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<CreateBody>, JsonRejection>,
) -> Result<Json<MetaRecord>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    let supplied = body.uuid.as_deref().map(parse_uuid).transpose()?;
    with_repo(&state, repo_uuid, move |repo_state| {
        repo_state.ensure_writable()?;
        for field in &body.fields {
            check_writable(&field.name, body.force)?;
        }
        let mut conn = repo_state.conn.lock_recover();
        let touched: Vec<String> = body.fields.iter().map(|f| f.name.clone()).collect();
        let mut writer = Writer::begin(&mut conn, None)?;
        let created = match supplied {
            Some(uuid) => {
                if db::get_version(writer.connection(), uuid)?.is_some() {
                    return Err(ApiError::conflict(format!("metarecord already exists: {uuid}")));
                }
                writer.create_metarecord_with_uuid(uuid, body.fields)?
            }
            None => writer.create_metarecord(body.fields)?,
        };
        validate_schema(repo_state, writer.connection(), created.uuid, &touched)?;
        let tree_touched = writer.touched_tree();
        let watch_touched = writer.touched_watch();
        writer.commit()?;
        if tree_touched {
            repo_state.lock_cache().populate(&conn)?;
        }
        if watch_touched {
            repo_state.refresh_watches(&conn);
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
    Query(ev): Query<ExpectedVersion>,
) -> Result<StatusCode, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let uuid = parse_uuid(&uuid)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        repo_state.ensure_writable()?;
        let mut conn = repo_state.conn.lock_recover();
        if db::get_version(&conn, uuid)?.is_none() {
            return Err(ApiError::not_found(format!("Metarecord not found: {uuid}")));
        }
        let mut writer = Writer::begin(&mut conn, None)?;
        ensure_version(writer.connection(), uuid, ev.expected_version)?;
        writer.delete_metarecord(uuid)?;
        let tree_touched = writer.touched_tree();
        let watch_touched = writer.touched_watch();
        writer.commit()?;
        if tree_touched {
            repo_state.lock_cache().populate(&conn)?;
        }
        if watch_touched {
            repo_state.refresh_watches(&conn);
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
    Query(ev): Query<ExpectedVersion>,
    payload: Result<Json<RecordFieldBody>, JsonRejection>,
) -> Result<Json<MetaRecord>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    let uuid = parse_uuid(&uuid)?;
    let rows = resolved_values(body.value, body.values)?;
    write_record_checked(&state, repo_uuid, uuid, ev.expected_version, move |writer| {
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
    Query(ev): Query<ExpectedVersion>,
    payload: Option<Json<ForceBody>>,
) -> Result<StatusCode, ApiError> {
    let force = payload.map(|Json(b)| b.force).unwrap_or(false);
    let repo_uuid = parse_uuid(&repo)?;
    let uuid = parse_uuid(&uuid)?;
    write_record_checked(&state, repo_uuid, uuid, ev.expected_version, move |writer| {
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
    Query(ev): Query<ExpectedVersion>,
    payload: Result<Json<SetRecordBody>, JsonRejection>,
) -> Result<Json<MetaRecord>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    let uuid = parse_uuid(&uuid)?;
    write_record_checked(&state, repo_uuid, uuid, ev.expected_version, move |writer| {
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
    Query(ev): Query<ExpectedVersion>,
    payload: Result<Json<SetFieldBody>, JsonRejection>,
) -> Result<Json<MetaRecord>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    let uuid = parse_uuid(&uuid)?;
    let value = single_value(body.value, body.values)?;
    write_record_checked(&state, repo_uuid, uuid, ev.expected_version, move |writer| {
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

        let mut writer = Writer::begin(&mut conn, None)?;
        writer.rename_field(uuid, id, &new_name, new_value)?;
        validate_schema(
            repo_state,
            writer.connection(),
            uuid,
            &[old.name.clone(), new_name.clone()],
        )?;
        let tree_touched = writer.touched_tree();
        let watch_touched = writer.touched_watch();
        writer.commit()?;
        if tree_touched {
            repo_state.lock_cache().populate(&conn)?;
        }
        if watch_touched {
            repo_state.refresh_watches(&conn);
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
        let mut writer = Writer::begin(&mut conn, None)?;
        writer.delete_field(uuid, id)?;
        validate_schema(repo_state, writer.connection(), uuid, std::slice::from_ref(&row.name))?;
        let tree_touched = writer.touched_tree();
        let watch_touched = writer.touched_watch();
        writer.commit()?;
        if tree_touched {
            repo_state.lock_cache().populate(&conn)?;
        }
        if watch_touched {
            repo_state.refresh_watches(&conn);
        }
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

// ── Cross-repo sync (spec-sync "HTTP API") ──────────────────────────────────

use crate::sync;

/// Resolves two repo selectors to loaded repos in canonical order and runs `f`
/// on the blocking pool with both (`repo_a` < `repo_b` lexicographically).
/// 404 if either repo is not loaded; 400 if the two are the same repo.
async fn with_pair<T, F>(state: &AppState, x: &str, y: &str, f: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce(&RepoState, &RepoState) -> Result<T, ApiError> + Send + 'static,
{
    let x = parse_uuid(x)?;
    let y = parse_uuid(y)?;
    let (a, b) = sync::canonical_pair(x, y)
        .ok_or_else(|| ApiError::bad_request("a repository cannot be synced with itself"))?;
    let repo_a = state.repo(a)?;
    let repo_b = state.repo(b)?;
    tokio::task::spawn_blocking(move || f(&repo_a, &repo_b))
        .await
        .map_err(|e| ApiError::internal(format!("blocking task failed: {e}")))?
}

/// An opened per-pair sync database plus the repo hosting its file.
struct PairDb {
    conn: rusqlite::Connection,
    host: Uuid,
}

/// Locates and opens the pair's sync database (spec-sync "Location and
/// discovery"). `create_host` (a repo of the pair) creates the file under that
/// repo's `internal/` when absent; `None` is a read-only call, which returns
/// `None` for a pair that has no sync database yet.
fn open_pair_db(
    repo_a: &RepoState,
    repo_b: &RepoState,
    create_host: Option<Uuid>,
) -> Result<Option<PairDb>, ApiError> {
    let a = repo_a.config.repo_uuid;
    let b = repo_b.config.repo_uuid;
    let a_int = repo_a.internal_dir();
    let b_int = repo_b.internal_dir();

    match sync::locate(&a_int, &b_int, a, b) {
        sync::Located::Ambiguous => {
            Err(ApiError::conflict("sync database present in both repos; delete the stale copy"))
        }
        sync::Located::Found(path) => {
            let host = if path.starts_with(&a_int) { a } else { b };
            let conn = sync::open(&path)?;
            let ok = sync::read_meta(&conn, "repo_a")?.as_deref()
                == Some(&a.as_simple().to_string())
                && sync::read_meta(&conn, "repo_b")?.as_deref() == Some(&b.as_simple().to_string());
            if !ok {
                return Err(ApiError::conflict("sync database identity mismatch"));
            }
            // Relocation is supported: keep meta.host in step with the file's
            // actual location.
            if sync::read_meta(&conn, "host")?.as_deref() != Some(&host.as_simple().to_string()) {
                sync::write_meta(&conn, a, b, host)?;
            }
            Ok(Some(PairDb { conn, host }))
        }
        sync::Located::Absent => match create_host {
            None => Ok(None),
            Some(host) => {
                let dir = if host == a { &a_int } else { &b_int };
                std::fs::create_dir_all(dir)
                    .map_err(|e| ApiError::internal(format!("create internal/: {e}")))?;
                let conn = sync::open(&dir.join(sync::sync_db_filename(a, b)))?;
                sync::write_meta(&conn, a, b, host)?;
                Ok(Some(PairDb { conn, host }))
            }
        },
    }
}

fn link_json(l: &sync::Link) -> serde_json::Value {
    json!({
        "uuid": hex(l.uuid),
        "record_a": hex(l.record_a),
        "record_b": hex(l.record_b),
        "version_a": l.version_a,
        "version_b": l.version_b,
    })
}

/// `GET /sync/:a/:b/links` — list links (read-only).
async fn sync_list_links(
    State(state): State<Arc<AppState>>,
    Path((a, b)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    with_pair(&state, &a, &b, move |repo_a, repo_b| {
        let (ra, rb) = (repo_a.config.repo_uuid, repo_b.config.repo_uuid);
        let (host, links) = match open_pair_db(repo_a, repo_b, None)? {
            None => (serde_json::Value::Null, vec![]),
            Some(db) => (json!(hex(db.host)), sync::list_links(&db.conn)?),
        };
        Ok(Json(json!({
            "repo_a": hex(ra),
            "repo_b": hex(rb),
            "host": host,
            "links": links.iter().map(link_json).collect::<Vec<_>>(),
        })))
    })
    .await
}

#[derive(Deserialize)]
struct CreateLinkBody {
    record_a: String,
    record_b: String,
    #[serde(default)]
    host: Option<String>,
}

/// `POST /sync/:a/:b/links` — create a link (canonical roles).
async fn sync_create_link(
    State(state): State<Arc<AppState>>,
    Path((a, b)): Path<(String, String)>,
    payload: Result<Json<CreateLinkBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(body) = payload?;
    with_pair(&state, &a, &b, move |repo_a, repo_b| {
        let ra = repo_a.config.repo_uuid;
        let rb = repo_b.config.repo_uuid;
        let record_a = parse_uuid(&body.record_a)?;
        let record_b = parse_uuid(&body.record_b)?;
        // Both endpoint metarecords must exist.
        if db::get_version(&repo_a.conn.lock_recover(), record_a)?.is_none() {
            return Err(ApiError::not_found(format!("metarecord not found in repo_a: {record_a}")));
        }
        if db::get_version(&repo_b.conn.lock_recover(), record_b)?.is_none() {
            return Err(ApiError::not_found(format!("metarecord not found in repo_b: {record_b}")));
        }
        let host = match &body.host {
            Some(h) => {
                let h = parse_uuid(h)?;
                if h != ra && h != rb {
                    return Err(ApiError::bad_request("host must be one of the pair"));
                }
                h
            }
            None => ra,
        };
        let db = open_pair_db(repo_a, repo_b, Some(host))?.expect("create_host given");
        if sync::link_for_record(&db.conn, sync::Side::A, record_a)?.is_some()
            || sync::link_for_record(&db.conn, sync::Side::B, record_b)?.is_some()
        {
            return Err(ApiError::conflict("a record is already linked in this pair"));
        }
        let link = sync::create_link(&db.conn, record_a, record_b)?;
        Ok(Json(link_json(&link)))
    })
    .await
}

/// `GET /sync/:a/:b/links/:link` — one link, its decoded snapshot, and both
/// endpoint metarecords inline.
async fn sync_get_link(
    State(state): State<Arc<AppState>>,
    Path((a, b, link)): Path<(String, String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    with_pair(&state, &a, &b, move |repo_a, repo_b| {
        let link_uuid = parse_uuid(&link)?;
        let Some(db) = open_pair_db(repo_a, repo_b, None)? else {
            return Err(ApiError::not_found("no sync database for this pair"));
        };
        let Some(l) = sync::get_link(&db.conn, link_uuid)? else {
            return Err(ApiError::not_found(format!("link not found: {link_uuid}")));
        };
        let snapshot: Vec<serde_json::Value> = sync::read_snapshot(&db.conn, link_uuid)?
            .into_iter()
            .map(|f| {
                json!({
                    "name": f.name,
                    "value": &f.value,
                    "value_b": f.value_uuid_b.map(hex),
                })
            })
            .collect();
        let record_a = metarecord_response(&repo_a.conn.lock_recover(), l.record_a).ok();
        let record_b = metarecord_response(&repo_b.conn.lock_recover(), l.record_b).ok();
        Ok(Json(json!({
            "link": link_json(&l),
            "snapshot": snapshot,
            "record_a": record_a,
            "record_b": record_b,
        })))
    })
    .await
}

#[derive(Deserialize, Default)]
struct WithEndpointParam {
    with_endpoint: Option<String>,
}

/// `DELETE /sync/:a/:b/links/:link[?with_endpoint=a|b]` — delete a link (and its
/// snapshot). With `with_endpoint`, the endpoint metarecord on that side is
/// deleted first (spec-sync "Metarecord deletion propagation" normative order).
async fn sync_delete_link(
    State(state): State<Arc<AppState>>,
    Path((a, b, link)): Path<(String, String, String)>,
    Query(ep): Query<WithEndpointParam>,
) -> Result<Json<serde_json::Value>, ApiError> {
    with_pair(&state, &a, &b, move |repo_a, repo_b| {
        let link_uuid = parse_uuid(&link)?;
        let Some(db) = open_pair_db(repo_a, repo_b, None)? else {
            return Err(ApiError::not_found("no sync database for this pair"));
        };
        let Some(l) = sync::get_link(&db.conn, link_uuid)? else {
            return Err(ApiError::not_found(format!("link not found: {link_uuid}")));
        };
        if let Some(side) = ep.with_endpoint.as_deref() {
            let (repo, record) = match side {
                "a" => (repo_a, l.record_a),
                "b" => (repo_b, l.record_b),
                _ => return Err(ApiError::bad_request("with_endpoint must be 'a' or 'b'")),
            };
            repo.ensure_writable()?;
            let mut conn = repo.conn.lock_recover();
            if db::get_version(&conn, record)?.is_some() {
                let mut writer = Writer::begin(&mut conn, None)?;
                writer.delete_metarecord(record)?;
                let tree_touched = writer.touched_tree();
                writer.commit()?;
                if tree_touched {
                    repo.lock_cache().populate(&conn)?;
                }
            }
        }
        sync::delete_link(&db.conn, link_uuid)?;
        Ok(Json(json!({ "deleted": true })))
    })
    .await
}

/// `GET /sync/:a/:b/status` — per-link change/conflict state (spec-sync
/// truth table). Read-only.
async fn sync_status(
    State(state): State<Arc<AppState>>,
    Path((a, b)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    with_pair(&state, &a, &b, move |repo_a, repo_b| {
        let (ra, rb) = (repo_a.config.repo_uuid, repo_b.config.repo_uuid);
        let links = match open_pair_db(repo_a, repo_b, None)? {
            None => vec![],
            Some(db) => sync::list_links(&db.conn)?,
        };
        let conn_a = repo_a.conn.lock_recover();
        let conn_b = repo_b.conn.lock_recover();
        let mut out = Vec::with_capacity(links.len());
        for l in &links {
            let ea = db::get_version(&conn_a, l.record_a)?;
            let eb = db::get_version(&conn_b, l.record_b)?;
            let state = link_state(ea, eb, l.version_a, l.version_b);
            out.push(json!({
                "uuid": hex(l.uuid),
                "state": state,
                "e_a_version": ea,
                "e_b_version": eb,
                "version_a": l.version_a,
                "version_b": l.version_b,
            }));
        }
        Ok(Json(json!({ "repo_a": hex(ra), "repo_b": hex(rb), "links": out })))
    })
    .await
}

/// The change-detection state of a link (spec-sync "status"), by precedence.
fn link_state(ea: Option<u64>, eb: Option<u64>, va: Option<u64>, vb: Option<u64>) -> &'static str {
    match (ea.is_none(), eb.is_none()) {
        (true, true) => return "missing_both",
        (true, false) => return "missing_a",
        (false, true) => return "missing_b",
        (false, false) => {}
    }
    let (Some(va), Some(vb)) = (va, vb) else {
        return "never_synced";
    };
    match (ea != Some(va), eb != Some(vb)) {
        (false, false) => "in_sync",
        (true, false) => "ahead_a",
        (false, true) => "ahead_b",
        (true, true) => "conflict",
    }
}

#[derive(Deserialize)]
struct CommitBody {
    commits: Vec<CommitEntry>,
}

#[derive(Deserialize)]
struct CommitEntry {
    link: String,
    version_a: u64,
    version_b: u64,
    #[serde(default)]
    snapshot: Vec<SnapshotEntry>,
}

#[derive(Deserialize)]
struct SnapshotEntry {
    name: String,
    value: Value,
    #[serde(default)]
    value_b: Option<String>,
}

/// `POST /sync/:a/:b/links/commit` — batched sync-commit (spec-sync).
async fn sync_commit(
    State(state): State<Arc<AppState>>,
    Path((a, b)): Path<(String, String)>,
    payload: Result<Json<CommitBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(body) = payload?;
    with_pair(&state, &a, &b, move |repo_a, repo_b| {
        let mut db = open_pair_db(repo_a, repo_b, Some(repo_a.config.repo_uuid))?
            .expect("create_host given");
        let mut commits = Vec::with_capacity(body.commits.len());
        for c in body.commits {
            let snapshot = c
                .snapshot
                .into_iter()
                .map(|f| {
                    let value_uuid_b = match f.value_b {
                        Some(h) => Some(parse_uuid(&h)?),
                        None => None,
                    };
                    Ok(sync::SnapshotField { name: f.name, value: f.value, value_uuid_b })
                })
                .collect::<Result<Vec<_>, ApiError>>()?;
            commits.push(sync::Commit {
                link: parse_uuid(&c.link)?,
                version_a: c.version_a,
                version_b: c.version_b,
                snapshot,
            });
        }
        let n = commits.len();
        sync::commit_batch(&mut db.conn, &commits)?;
        Ok(Json(json!({ "committed": n })))
    })
    .await
}
