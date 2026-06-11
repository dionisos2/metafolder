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

use metafolder_core::entry::{Field, Metadata, Value};

use metafolder_core::query::Query as MetaQuery;

use crate::db;
use crate::error::ApiError;
use crate::log::Writer;
use crate::pagination::{self, Cursor, Page};
use crate::query_exec::{self, SortKey};
use crate::repo::RepoLocator;
use crate::reserved;
use crate::state::{AppState, RepoState};

pub fn build(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/repos", get(list_repos))
        .route("/repos/init", post(init_repo))
        .route("/repos/load", post(load_repo))
        .route("/repos/:repo/metadata", get(list_metadata).post(create_metadata))
        .route(
            "/repos/:repo/metadata/:uuid",
            get(get_metadata).delete(delete_metadata).patch(patch_metadata),
        )
        .route("/repos/:repo/query", post(run_query))
        .route("/repos/:repo/set", post(batch_set))
        .route("/repos/:repo/log", get(get_log))
        .route(
            "/repos/:repo/log/revisions/:rev_id",
            get(get_revision).patch(patch_revision),
        )
        .route("/repos/:repo/log/prune", post(prune_log))
        .route("/repos/:repo/rollback", post(rollback))
        .route("/repos/:repo/schema", get(get_schema))
        .route("/repos/:repo/schema/reload", post(reload_schema))
        .route("/repos/:repo/schema/check", post(check_schema))
        .route("/repos/:repo/reconcile", post(full_reconcile))
        .route("/repos/:repo/track", post(track))
        .route("/repos/:repo/metadata/:uuid/reconcile", post(entry_reconcile))
        .route("/repos/:repo/metadata/:uuid/fields", post(append_field))
        .route(
            "/repos/:repo/metadata/:uuid/fields/:field_id",
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

/// Fetches the full metadata object of an entry, or 404.
fn entry_response(conn: &rusqlite::Connection, uuid: Uuid) -> Result<Metadata, ApiError> {
    db::get_entry(conn, uuid)?
        .ok_or_else(|| ApiError::not_found(format!("Metadata entry not found: {uuid}")))
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
    let guard = repo_state.schema.lock().unwrap();
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
}

async fn init_repo(
    State(state): State<Arc<AppState>>,
    payload: Result<Json<InitBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(body) = payload?;
    let uuid = tokio::task::spawn_blocking(move || {
        state.init_repo(&body.root, body.metafolder.as_deref())
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

// ── Metadata listing ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct PageParams {
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    cursor: Option<String>,
}

async fn list_metadata(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    Query(params): Query<PageParams>,
) -> Result<Response, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = repo_state.conn.lock().unwrap();
        match params.limit {
            None => {
                let uuids = db::list_entries(&conn, repo_uuid)?;
                let hexes: Vec<String> = uuids.into_iter().map(hex).collect();
                Ok(Json(hexes).into_response())
            }
            Some(limit) => {
                let hash = pagination::context_hash(&["metadata-list", &hex(repo_uuid)]);
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
                Ok(Json(Page { results, next_cursor }).into_response())
            }
        }
    })
    .await
}

// ── Metadata CRUD ─────────────────────────────────────────────────────────────

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
    entry_uuid: Option<String>,
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
    let entity_filter = params.entry_uuid.as_deref().map(parse_uuid).transpose()?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = repo_state.conn.lock().unwrap();
        let head = crate::log::get_head(&conn)?;
        let mode = params.mode.as_deref().unwrap_or("linear");

        let mut ops: Vec<crate::log::OpRow> = match mode {
            "tree" => crate::log::all_ops(&conn)?,
            "linear" => match head {
                None => vec![],
                Some(head) => {
                    let mut chain = crate::log::ancestry(&conn, head)?;
                    chain.reverse(); // root → HEAD, oldest first
                    chain
                        .into_iter()
                        .map(|id| {
                            crate::log::get_op(&conn, id)?
                                .ok_or_else(|| anyhow::anyhow!("op {id} vanished"))
                        })
                        .collect::<Result<Vec<_>, _>>()?
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
        let conn = repo_state.conn.lock().unwrap();
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
        let conn = repo_state.conn.lock().unwrap();
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

/// Metadata-only atomic rollback (spec-event-log `POST /rollback`).
async fn rollback(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<RollbackBody>, JsonRejection>,
) -> Result<Json<crate::log::NavResult>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    let target = body.target.into_target()?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let mut conn = repo_state.conn.lock().unwrap();
        let resolved = crate::log::resolve_target(&conn, &target)?;
        let result = crate::log::navigate(&mut conn, repo_uuid, resolved)?;
        // Navigation rewrites tree positions arbitrarily: drop the cache.
        repo_state.cache.lock().unwrap().clear();
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
        let mut conn = repo_state.conn.lock().unwrap();
        let resolved = crate::log::resolve_target(&conn, &target)?
            .ok_or_else(|| ApiError::bad_request("cannot prune to the empty state"))?;
        let (ops, revisions) = crate::log::prune(&mut conn, mode, resolved)
            .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
        Ok(Json(json!({"pruned_operations": ops, "pruned_revisions": revisions})))
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
    let guard = repo_state.schema.lock().unwrap();
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
        *repo_state.schema.lock().unwrap() = loaded;
        Ok(Json(raw))
    })
    .await
}

#[derive(Deserialize, Default)]
struct CheckBody {
    #[serde(default)]
    query: Option<MetaQuery>,
}

/// Scans entries and reports every constraint violation (the schema file is
/// never validated retroactively on edit).
async fn check_schema(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Option<Json<CheckBody>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let body = payload.map(|Json(b)| b).unwrap_or_default();
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = repo_state.conn.lock().unwrap();
        let uuids = match &body.query {
            None => db::list_entries(&conn, repo_uuid)?,
            Some(query) => {
                let mut cache = repo_state.cache.lock().unwrap();
                query_exec::execute(&conn, &mut cache, repo_uuid, query, &[], None, None)?.0
            }
        };
        let guard = repo_state.schema.lock().unwrap();
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

async fn full_reconcile(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
) -> Result<Json<crate::reconcile::ReconcileResult>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        Ok(Json(crate::reconcile::reconcile(repo_state)?))
    })
    .await
}

async fn entry_reconcile(
    State(state): State<Arc<AppState>>,
    Path((repo, uuid)): Path<(String, String)>,
) -> Result<Json<crate::reconcile::ReconcileResult>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let uuid = parse_uuid(&uuid)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        Ok(Json(crate::reconcile::reconcile_entry(repo_state, uuid)?))
    })
    .await
}

#[derive(Deserialize)]
struct TrackBody {
    path: PathBuf,
}

/// Creates the entry for a single filesystem path without activating
/// tracking (spec-file-tracking "Single-entry track"). Parents are created
/// with `mf_watch = false`; no eligibility check applies.
async fn track(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<TrackBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
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

        let mut conn = repo_state.conn.lock().unwrap();
        let mut cache = repo_state.cache.lock().unwrap();
        if let Some(existing) = cache.resolve_path(&conn, "mfr_path", &rel)? {
            return Err(ApiError::conflict(format!(
                "path already tracked by entry {}",
                hex(existing)
            )));
        }
        let untracked = [Field::new("mf_watch", Value::Bool(false))];
        let mut writer = Writer::begin(&mut conn, repo_uuid, None)?;
        let uuid = crate::reconcile::create_entry_for(
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
}

async fn run_query(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<QueryBody>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = repo_state.conn.lock().unwrap();
        let mut cache = repo_state.cache.lock().unwrap();
        let (uuids, next_cursor) = query_exec::execute(
            &conn,
            &mut cache,
            repo_uuid,
            &body.query,
            &body.sort,
            body.limit,
            body.cursor.as_deref(),
        )?;
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
                    let mut entry = entry_response(&conn, uuid)?;
                    if let Some(filter) = &fields_filter {
                        entry.fields.retain(|f| filter.contains(&f.name));
                    }
                    objects.push(serde_json::to_value(entry).expect("entry serialization"));
                }
                objects
            }
        };

        if body.limit.is_some() {
            Ok(Json(Page { results, next_cursor }).into_response())
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
        check_writable(&body.name, body.force)?;
        let mut conn = repo_state.conn.lock().unwrap();
        let mut cache = repo_state.cache.lock().unwrap();
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

async fn create_metadata(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<CreateBody>, JsonRejection>,
) -> Result<Json<Metadata>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        for field in &body.fields {
            check_writable(&field.name, body.force)?;
        }
        let mut conn = repo_state.conn.lock().unwrap();
        let touched: Vec<String> = body.fields.iter().map(|f| f.name.clone()).collect();
        let mut writer = Writer::begin(&mut conn, repo_uuid, None)?;
        let created = writer.create_entry(body.fields)?;
        validate_schema(repo_state, writer.connection(), created.uuid, &touched)?;
        writer.commit()?;
        Ok(Json(created))
    })
    .await
}

async fn get_metadata(
    State(state): State<Arc<AppState>>,
    Path((repo, uuid)): Path<(String, String)>,
) -> Result<Json<Metadata>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let uuid = parse_uuid(&uuid)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = repo_state.conn.lock().unwrap();
        Ok(Json(entry_response(&conn, uuid)?))
    })
    .await
}

async fn delete_metadata(
    State(state): State<Arc<AppState>>,
    Path((repo, uuid)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let uuid = parse_uuid(&uuid)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let mut conn = repo_state.conn.lock().unwrap();
        if db::get_version(&conn, uuid)?.is_none() {
            return Err(ApiError::not_found(format!("Metadata entry not found: {uuid}")));
        }
        let mut writer = Writer::begin(&mut conn, repo_uuid, None)?;
        writer.delete_entry(uuid)?;
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

async fn patch_metadata(
    State(state): State<Arc<AppState>>,
    Path((repo, uuid)): Path<(String, String)>,
    payload: Result<Json<SetFieldBody>, JsonRejection>,
) -> Result<Json<Metadata>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    let uuid = parse_uuid(&uuid)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        check_writable(&body.name, body.force)?;
        let mut conn = repo_state.conn.lock().unwrap();
        if db::get_version(&conn, uuid)?.is_none() {
            return Err(ApiError::not_found(format!("Metadata entry not found: {uuid}")));
        }
        let mut writer = Writer::begin(&mut conn, repo_uuid, None)?;
        writer.set_field(uuid, &body.name, body.value)?;
        validate_schema(repo_state, writer.connection(), uuid, std::slice::from_ref(&body.name))?;
        writer.commit()?;
        Ok(Json(entry_response(&conn, uuid)?))
    })
    .await
}

async fn append_field(
    State(state): State<Arc<AppState>>,
    Path((repo, uuid)): Path<(String, String)>,
    payload: Result<Json<SetFieldBody>, JsonRejection>,
) -> Result<Json<Metadata>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    let uuid = parse_uuid(&uuid)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        check_writable(&body.name, body.force)?;
        let mut conn = repo_state.conn.lock().unwrap();
        if db::get_version(&conn, uuid)?.is_none() {
            return Err(ApiError::not_found(format!("Metadata entry not found: {uuid}")));
        }
        let mut writer = Writer::begin(&mut conn, repo_uuid, None)?;
        writer.append_field(uuid, &body.name, body.value)?;
        validate_schema(repo_state, writer.connection(), uuid, std::slice::from_ref(&body.name))?;
        writer.commit()?;
        Ok(Json(entry_response(&conn, uuid)?))
    })
    .await
}

#[derive(Deserialize)]
struct ReplaceFieldBody {
    value: Value,
    #[serde(default)]
    force: bool,
}

/// Finds the field row of an entry by id, or 404.
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
) -> Result<Json<Metadata>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    let uuid = parse_uuid(&uuid)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let mut conn = repo_state.conn.lock().unwrap();
        let name = owned_field_name(&conn, uuid, field_id)?;
        check_writable(&name, body.force)?;
        let mut writer = Writer::begin(&mut conn, repo_uuid, None)?;
        writer.replace_field(uuid, field_id, body.value)?;
        validate_schema(repo_state, writer.connection(), uuid, std::slice::from_ref(&name))?;
        writer.commit()?;
        Ok(Json(entry_response(&conn, uuid)?))
    })
    .await
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
    with_repo(&state, repo_uuid, move |repo_state| {
        let mut conn = repo_state.conn.lock().unwrap();
        let name = owned_field_name(&conn, uuid, field_id)?;
        check_writable(&name, force)?;
        let mut writer = Writer::begin(&mut conn, repo_uuid, None)?;
        writer.delete_field(uuid, field_id)?;
        validate_schema(repo_state, writer.connection(), uuid, std::slice::from_ref(&name))?;
        writer.commit()?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}
