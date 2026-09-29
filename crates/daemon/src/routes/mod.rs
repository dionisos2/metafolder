//! Axum route handlers. Blocking store work is dispatched through
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

use metafolder_core::query::{FollowTarget, Query as MetaQuery};
use metafolder_core::slowlog;

use crate::error::ApiError;
use crate::log::Writer;
use crate::orphans;
use crate::pagination::Page;
use crate::query_result::SortKey;
use crate::repo::RepoLocator;
use crate::reserved;
use crate::state::{AppState, RepoState, RollbackLock};
use crate::store::Rows;
use crate::tasks::TaskKind;

mod fs;
mod history;
mod query;
mod records;
mod repos;
mod revert;
mod schema;
mod sync;
mod tree;

use self::{
    fs::*, history::*, query::*, records::*, repos::*, revert::*, schema::*, sync::*, tree::*,
};

pub fn build(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/diagnostics", get(diagnostics_since))
        .route("/tasks", get(list_all_tasks))
        .route("/repos", get(list_repos))
        .route("/repos/init", post(init_repo))
        .route("/repos/:repo/check", post(check_repo))
        .route("/repos/:repo/backup", post(backup_repo))
        .route("/repos/:repo/restore", post(restore_loaded_repo))
        .route("/repos/restore", post(restore_repo))
        .route("/repos/:repo/reindex", post(reindex_repo))
        .route("/repos/load", post(load_repo))
        .route("/repos/:repo", get(get_repo).patch(rename_repo))
        .route("/repos/:repo/unload", post(unload_repo))
        // ── Resource layer (single, directly-addressed) ──────────────────────
        .route("/repos/:repo/metarecords", post(create_record_endpoint))
        .route("/repos/:repo/metarecords/bulk", post(bulk_create_endpoint))
        .route("/repos/:repo/metarecords/trash", post(trash_delete_endpoint))
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
        .route("/repos/:repo/query/profile", post(profile_query))
        .route("/repos/:repo/query/delete", post(delete_by_query))
        .route("/repos/:repo/query/fields/set", post(batch_set))
        .route("/repos/:repo/query/fields/add", post(batch_add))
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
        .route("/repos/:repo/revert", post(revert))
        .route("/repos/:repo/revert/plan", get(revert_plan))
        .route("/repos/:repo/revert/start", post(revert_start))
        .route("/repos/:repo/revert/commit", post(revert_commit))
        .route("/repos/:repo/revert/abort", post(revert_abort))
        .route("/repos/:repo/schema", get(get_schema))
        .route("/repos/:repo/schema/reload", post(reload_schema))
        .route("/repos/:repo/schema/check", post(check_schema))
        .route("/repos/:repo/tasks", get(list_repo_tasks))
        .route("/repos/:repo/tasks/:task", get(get_task))
        .route("/repos/:repo/tasks/:task/cancel", post(cancel_task))
        .route("/repos/:repo/reconcile", post(full_reconcile))
        .route("/repos/:repo/duplicates/scan", post(duplicates_scan))
        .route("/repos/:repo/mounts", get(mounts))
        .route("/repos/:repo/watch", get(watch_status))
        .route("/repos/:repo/watch/pause", post(watch_pause))
        .route("/repos/:repo/watch/resume", post(watch_resume))
        .route("/repos/:repo/watch/exceeded", get(watch_exceeded_list).post(watch_exceeded_set))
        .route("/repos/:repo/watch/check", post(watch_check))
        .route("/repos/:repo/watch/activity", get(watch_activity_children).post(watch_activity_of))
        .route("/repos/:repo/watch/activity/reset", post(watch_activity_reset))
        .route("/repos/:repo/orphans/scan", post(orphans_scan))
        .route("/repos/:repo/orphans/clear", post(orphans_clear))
        .route("/repos/:repo/orphans/mark", post(orphans_mark))
        .route("/repos/:repo/orphans/relink", post(orphans_relink))
        .route("/repos/:repo/track", post(track))
        .route("/repos/:repo/slow", get(slow_log).delete(clear_slow_log))
        .route("/repos/:repo/eligibility", post(eligibility_explain))
        .route("/repos/:repo/ignore/effective", get(effective_ignore))
        // ── Cross-repo sync (spec-sync) ─────────────────────────────────────
        .route("/sync/:a/:b/links", get(sync_list_links).post(sync_create_link))
        .route("/sync/:a/:b/links/:link", get(sync_get_link).delete(sync_delete_link))
        .route("/sync/:a/:b/links/commit", post(sync_commit))
        .route("/sync/:a/:b/status", get(sync_status))
        .with_state(state)
        .layer(axum::middleware::from_fn(name_operation))
}

/// The router with the session-token authentication layer (doc "Session tokens"): every
/// request must carry `Authorization: Bearer <token>`. Used by the daemon
/// binary; tests drive [`build`] directly (no network, no token).
pub fn build_authenticated(state: Arc<AppState>, token: Arc<str>) -> Router {
    build(state).layer(axum::middleware::from_fn_with_state(token, require_token))
}

/// What the request layer knows about the operation being served, carried to
/// the blocking thread that will time it (doc "Slow log").
///
/// A task-local rather than an argument: the name of the operation is a
/// property of the *request*, known only here, while the timing happens deep in
/// a handler — and threading it through every handler signature would put
/// diagnostics in the type of every route.
#[derive(Clone)]
struct RequestInfo {
    /// Method plus matched route pattern (`POST /repos/:repo/query`).
    op: String,
    /// The client's correlation id, if it sent one.
    op_id: Option<String>,
    /// What the user asked for, in the client's own words.
    client: Option<String>,
}

tokio::task_local! {
    static REQUEST: RequestInfo;
}

/// Names every request for the slow-operation log. The layer only *names* it:
/// whether anything is recorded is decided per repository, when the operation
/// ends (see [`with_repo`]).
async fn name_operation(
    // Taken as an extractor rather than read out of the extensions: it is the
    // route *pattern* (`/repos/:repo/query`), the shape a reader groups by,
    // where the URI is one occurrence of it.
    matched: Option<axum::extract::MatchedPath>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    // Everything borrowed from the request is read in this block and nothing
    // borrowed outlives it: a borrow of the request held across the `await`
    // below would make the middleware's future non-`Send` (the body is not
    // `Sync`), which no error message says plainly.
    let info = {
        let route = matched
            .as_ref()
            .map(|p| p.as_str())
            .unwrap_or_else(|| request.uri().path())
            .to_string();
        RequestInfo {
            op: format!("{} {route}", request.method()),
            op_id: short_header(request.headers(), "x-metafolder-op-id"),
            client: short_header(request.headers(), "x-metafolder-context"),
        }
    };
    REQUEST.scope(info, next.run(request)).await
}

/// One client-supplied header, capped: a rogue client must not be able to fill
/// the log with a single entry.
fn short_header(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.chars().take(slowlog::MAX_CONTEXT_CHARS).collect())
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
    let repo = state.ready_repo(repo_uuid)?;
    // The whole blocking closure is the operation, so the timing starts and
    // ends on the one thread that runs it (doc "Slow log").
    let info = REQUEST.try_with(|info| info.clone()).ok();
    tokio::task::spawn_blocking(move || {
        let _timed = info.map(|info| {
            let guard = slowlog::begin(repo.slowlog.clone(), info.op);
            if let Some(id) = info.op_id {
                slowlog::set_op_id(id);
            }
            if let Some(client) = info.client {
                slowlog::note("client", client);
            }
            guard
        });
        f(&repo)
    })
    .await
    .map_err(|e| ApiError::internal(format!("blocking task failed: {e}")))?
}

/// Fetches the full metadata object of a metarecord, or 404.
fn metarecord_response(conn: &dyn crate::store::Store, uuid: Uuid) -> Result<MetaRecord, ApiError> {
    Rows::metarecord(conn, uuid)?
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
    conn: &dyn crate::store::Store,
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
/// With an optional optimistic-concurrency precondition (doc "Conditional writes"):
/// when `expected_version` is given and the metarecord's
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
        let mut conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        let mut writer = repo_state.writer(&mut conn, None)?;
        ensure_version(writer.store(), uuid, expected_version)?;
        let touched = slowlog::timed("write.fields", || write(&mut writer))?;
        slowlog::timed("validate.schema", || {
            validate_schema(repo_state, writer.store(), uuid, &touched)
        })?;
        let effects = writer.effects();
        slowlog::timed("commit", || writer.commit())?;
        repo_state.settle(&conn, &effects)?;
        metarecord_response(&conn, uuid)
    })
    .await
}

/// Shared scaffold for the set-layer field-write handlers (`batch_set`,
/// `batch_add`, `batch_remove`, `batch_unset`): runs on the blocking pool,
/// gates on repository writability and on the field name being writable,
/// resolves the query to its match set, then opens **one** logged [`Writer`]
/// and lets `write` mutate each match in turn — the whole batch is a single
/// revision.
///
/// `write` answers whether that metarecord actually changed: a match it left
/// alone is neither schema-validated nor counted, which is what makes the
/// reported `updated` the number of metarecords the call really touched
/// (doc "No duplicate rows"). Any closure error drops the Writer,
/// rolling the whole batch back — an all-or-nothing batch, like the
/// single-record scaffold.
async fn write_matches<F>(
    state: &AppState,
    repo_uuid: Uuid,
    name: String,
    force: bool,
    query: MetaQuery,
    mut write: F,
) -> Result<Json<serde_json::Value>, ApiError>
where
    F: FnMut(&mut Writer, Uuid) -> Result<bool, ApiError> + Send + 'static,
{
    with_repo(state, repo_uuid, move |repo_state| {
        repo_state.ensure_writable()?;
        check_writable(&name, force)?;
        slowlog::note("field", name.as_str());
        let mut conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        let cache = repo_state.tree();
        let uuids = resolve_query_uuids(&conn, &cache, &query)?;

        let mut writer = repo_state.writer(&mut conn, None)?;
        let writing = slowlog::phase("write.fields");
        let mut updated = 0usize;
        for uuid in &uuids {
            if !write(&mut writer, *uuid)? {
                continue;
            }
            updated += 1;
            slowlog::timed("validate.schema", || {
                validate_schema(repo_state, writer.store(), *uuid, std::slice::from_ref(&name))
            })?;
        }
        drop(writing);
        let effects = writer.effects();
        slowlog::timed("commit", || writer.commit())?;
        repo_state.settle(&conn, &effects)?;
        slowlog::note("updated", updated.to_string());
        Ok(Json(json!({ "updated": updated })))
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
/// cross-repo sync propagation (doc "Conditional writes").
fn ensure_version(
    conn: &dyn crate::store::Store,
    uuid: Uuid,
    expected: Option<u64>,
) -> Result<(), ApiError> {
    if let Some(expected) = expected {
        let current = Rows::version(conn, uuid)?;
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
fn ensure_exists(conn: &dyn crate::store::Store, uuid: Uuid) -> Result<(), ApiError> {
    if Rows::version(conn, uuid)?.is_none() {
        return Err(ApiError::not_found(format!("Metarecord not found: {uuid}")));
    }
    Ok(())
}
