//! Axum route handlers. Blocking SQLite work is dispatched through
//! `tokio::task::spawn_blocking`; every error is rendered as the JSON
//! `{"error": ...}` shape via [`ApiError`].

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use metafolder_core::metarecord::{Field, MetaRecord, Value};
use metafolder_core::sync::MutexExt;

use metafolder_core::query::Query as MetaQuery;

use crate::db;
use crate::error::ApiError;
use crate::log::Writer;
use crate::pagination::{self, Cursor, Page};
use crate::query_exec::{self, SortKey};
use crate::repo::RepoLocator;
use crate::reserved;
use crate::state::{AppState, RepoState, RollbackLock};

pub fn build(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/repos", get(list_repos))
        .route("/repos/init", post(init_repo))
        .route("/repos/load", post(load_repo))
        .route("/repos/:repo/metarecords", get(list_metarecords).post(create_record_endpoint))
        .route(
            "/repos/:repo/metarecords/:uuid",
            get(get_record_endpoint).delete(delete_record_endpoint).patch(patch_metarecord),
        )
        .route("/repos/:repo/query", post(run_query))
        .route("/repos/:repo/tree/resolve", post(tree_resolve))
        .route("/repos/:repo/set", post(batch_set))
        .route("/repos/:repo/log", get(get_log))
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
        .route("/repos/:repo/reconcile", post(full_reconcile))
        .route("/repos/:repo/track", post(track))
        .route("/repos/:repo/metarecords/:uuid/reconcile", post(metarecord_reconcile))
        .route("/repos/:repo/metarecords/:uuid/fields", post(append_field))
        .route(
            "/repos/:repo/metarecords/:uuid/fields/:field_id",
            put(replace_field).delete(delete_field),
        )
        .with_state(state)
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
struct TreeResolveBody {
    #[serde(default = "default_tree_field")]
    field: String,
    #[serde(default)]
    uuids: Vec<String>,
}

fn default_tree_field() -> String {
    "mfr_path".to_string()
}

/// `POST /repos/:repo/tree/resolve`: resolves each metarecord's TreeRef
/// positions for `field` (default `mfr_path`) to repo-root-relative paths.
/// A field is a multi-map, so each metarecord maps to an array of paths
/// (positions whose parent is stale are skipped). Resolution uses the in-memory
/// tree cache — one round-trip whatever the depth.
async fn tree_resolve(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<TreeResolveBody>, JsonRejection>,
) -> Result<Response, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let Json(body) = payload?;
    let uuids = body
        .uuids
        .iter()
        .map(|s| parse_uuid(s))
        .collect::<Result<Vec<_>, _>>()?;
    let field = body.field;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = repo_state.conn.lock_recover();
        let mut cache = repo_state.lock_cache();
        let mut out = serde_json::Map::new();
        for uuid in uuids {
            let paths = cache.paths_of(&conn, &field, uuid)?;
            out.insert(hex(uuid), json!(paths));
        }
        Ok(Json(serde_json::Value::Object(out)).into_response())
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
        writer.commit()?;
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

async fn health() -> Json<serde_json::Value> {
    Json(json!({"status": "ok"}))
}

async fn list_repos(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::to_value(state.list_repos()).expect("repo list serialization"))
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
    let uuid = tokio::task::spawn_blocking(move || state.load_repo(locator))
        .await
        .map_err(|e| ApiError::internal(format!("blocking task failed: {e}")))??;
    Ok(Json(json!({"repo_uuid": hex(uuid)})))
}

// ── MetaRecord listing ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct PageParams {
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    cursor: Option<String>,
}

async fn list_metarecords(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    Query(params): Query<PageParams>,
) -> Result<Response, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = repo_state.conn.lock_recover();
        match params.limit {
            None => {
                let uuids = db::list_entries(&conn, repo_uuid)?;
                let hexes: Vec<String> = uuids.into_iter().map(hex).collect();
                Ok(Json(hexes).into_response())
            }
            Some(limit) => {
                let hash = pagination::context_hash(&["metarecord-list", &hex(repo_uuid)]);
                let after = match &params.cursor {
                    None => None,
                    Some(token) => Some(pagination::decode(token, hash)?.last_uuid()?),
                };
                let mut uuids = db::list_entries_page(&conn, repo_uuid, after, limit + 1)?;
                let next_cursor = if uuids.len() > limit {
                    uuids.truncate(limit);
                    let last = *uuids.last().expect("non-empty page");
                    Some(pagination::encode(&Cursor { keys: vec![], uuid: hex(last), h: hash }))
                } else {
                    None
                };
                let results: Vec<String> = uuids.into_iter().map(hex).collect();
                Ok(Json(Page { results, next_cursor, total: None }).into_response())
            }
        }
    })
    .await
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
            other => {
                return Err(ApiError::bad_request(format!(
                    "invalid mode '{other}' (expected 'linear' or 'tree')"
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
        // Navigation rewrites tree positions arbitrarily: drop the cache.
        repo_state.lock_cache().clear();
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
            // The step rewrote tree positions arbitrarily: drop the cache.
            repo_state.lock_cache().clear();
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
}

/// Scans metarecords and reports every constraint violation (the schema file is
/// never validated retroactively on edit).
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
        if let Some(schema) = guard.as_ref() {
            let fields = schema.constrained_fields();
            for uuid in &uuids {
                for violation in
                    crate::schema::validate_entry_fields(schema, &conn, *uuid, &fields)?
                {
                    violations
                        .push(serde_json::to_value(&violation).expect("violation serialization"));
                }
            }
        }
        Ok(Json(json!({"checked": uuids.len(), "violations": violations})))
    })
    .await
}

// ── Reconcile and track ───────────────────────────────────────────────────────

fn default_true() -> bool {
    true
}

#[derive(Deserialize)]
struct ReconcileBody {
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
        Self { threshold: None, mime: true, refresh: true }
    }
}

async fn full_reconcile(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Option<Json<ReconcileBody>>,
) -> Result<Json<crate::reconcile::ReconcileResult>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let body = payload.map(|Json(b)| b).unwrap_or_default();
    if let Some(t) = body.threshold {
        if !(0.0..=1.0).contains(&t) {
            return Err(ApiError::bad_request("threshold must be in the range [0, 1]"));
        }
    }
    with_repo(&state, repo_uuid, move |repo_state| {
        repo_state.ensure_writable()?;
        Ok(Json(crate::reconcile::reconcile_full(repo_state, body.threshold, body.mime, body.refresh)?))
    })
    .await
}

async fn metarecord_reconcile(
    State(state): State<Arc<AppState>>,
    Path((repo, uuid)): Path<(String, String)>,
    payload: Option<Json<ReconcileBody>>,
) -> Result<Json<crate::reconcile::ReconcileResult>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let uuid = parse_uuid(&uuid)?;
    let body = payload.map(|Json(b)| b).unwrap_or_default();
    with_repo(&state, repo_uuid, move |repo_state| {
        repo_state.ensure_writable()?;
        Ok(Json(crate::reconcile::reconcile_metarecord(repo_state, uuid, body.mime, body.refresh)?))
    })
    .await
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
        if body.count && body.limit.is_none() {
            // The unwrapped (bare array) response has nowhere to carry it.
            return Err(ApiError::bad_request("'count' requires 'limit'"));
        }
        let conn = repo_state.conn.lock_recover();
        let mut cache = repo_state.lock_cache();
        let (uuids, next_cursor) = query_exec::execute(
            &conn,
            &mut cache,
            repo_uuid,
            &body.query,
            &body.sort,
            body.limit,
            body.cursor.as_deref(),
        )?;
        let total = if body.count {
            Some(query_exec::count(&conn, &mut cache, repo_uuid, &body.query)?)
        } else {
            None
        };
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
    })
    .await
}

#[derive(Deserialize)]
struct BatchSetBody {
    query: MetaQuery,
    name: String,
    value: Value,
    #[serde(default)]
    force: bool,
}

/// Runs the query server-side and sets the field on every match in a single
/// transaction (one revision).
async fn batch_set(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<BatchSetBody>, JsonRejection>,
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
        for uuid in &uuids {
            writer.set_field(*uuid, &body.name, body.value.clone())?;
            validate_schema(repo_state, writer.connection(), *uuid, std::slice::from_ref(&body.name))?;
        }
        writer.commit()?;
        Ok(Json(json!({"updated": uuids.len()})))
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
        writer.commit()?;
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
        writer.commit()?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

#[derive(Deserialize)]
struct SetFieldBody {
    name: String,
    value: Value,
    #[serde(default)]
    force: bool,
}

async fn patch_metarecord(
    State(state): State<Arc<AppState>>,
    Path((repo, uuid)): Path<(String, String)>,
    payload: Result<Json<SetFieldBody>, JsonRejection>,
) -> Result<Json<MetaRecord>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    let uuid = parse_uuid(&uuid)?;
    write_record(&state, repo_uuid, uuid, move |writer| {
        check_writable(&body.name, body.force)?;
        ensure_exists(writer.connection(), uuid)?;
        writer.set_field(uuid, &body.name, body.value)?;
        Ok(vec![body.name])
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
    write_record(&state, repo_uuid, uuid, move |writer| {
        check_writable(&body.name, body.force)?;
        ensure_exists(writer.connection(), uuid)?;
        writer.append_field(uuid, &body.name, body.value)?;
        Ok(vec![body.name])
    })
    .await
    .map(Json)
}

#[derive(Deserialize)]
struct ReplaceFieldBody {
    value: Value,
    #[serde(default)]
    force: bool,
}

/// Finds the field row of a metarecord by id, or 404.
fn owned_field_name(
    conn: &rusqlite::Connection,
    uuid: Uuid,
    field_id: i64,
) -> Result<String, ApiError> {
    db::get_field_rows(conn, uuid)?
        .into_iter()
        .find(|r| r.id == field_id)
        .map(|r| r.name)
        .ok_or_else(|| {
            ApiError::not_found(format!("Field {field_id} not found on entry {uuid}"))
        })
}

async fn replace_field(
    State(state): State<Arc<AppState>>,
    Path((repo, uuid, field_id)): Path<(String, String, i64)>,
    payload: Result<Json<ReplaceFieldBody>, JsonRejection>,
) -> Result<Json<MetaRecord>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    let uuid = parse_uuid(&uuid)?;
    write_record(&state, repo_uuid, uuid, move |writer| {
        let name = owned_field_name(writer.connection(), uuid, field_id)?;
        check_writable(&name, body.force)?;
        writer.replace_field(uuid, field_id, body.value)?;
        Ok(vec![name])
    })
    .await
    .map(Json)
}

#[derive(Deserialize, Default)]
struct ForceBody {
    #[serde(default)]
    force: bool,
}

async fn delete_field(
    State(state): State<Arc<AppState>>,
    Path((repo, uuid, field_id)): Path<(String, String, i64)>,
    payload: Option<Json<ForceBody>>,
) -> Result<StatusCode, ApiError> {
    let force = payload.map(|Json(b)| b.force).unwrap_or(false);
    let repo_uuid = parse_uuid(&repo)?;
    let uuid = parse_uuid(&uuid)?;
    write_record(&state, repo_uuid, uuid, move |writer| {
        let name = owned_field_name(writer.connection(), uuid, field_id)?;
        check_writable(&name, force)?;
        writer.delete_field(uuid, field_id)?;
        Ok(vec![name])
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
