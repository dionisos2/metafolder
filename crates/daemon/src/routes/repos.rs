//! Health, diagnostics, repositories (list, info, rename, init, load, unload,
//! check, reindex, backup, restore) and tasks.

use super::*;

pub(super) async fn health(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
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

#[derive(Deserialize)]
pub(super) struct SlowLogParams {
    limit: Option<usize>,
    since_ms: Option<i64>,
}

/// `GET /repos/:repo/slow?limit=&since_ms=` — the repository's slow-operation
/// log (spec-slow-log), both sources merged, newest first.
pub(super) async fn slow_log(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    Query(params): Query<SlowLogParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let dir = slow_log_dir(&state, repo_uuid)?;
    let limit = params.limit.unwrap_or(slowlog::DEFAULT_READ_LIMIT).min(slowlog::MAX_READ_LIMIT);
    let (entries, truncated) =
        tokio::task::spawn_blocking(move || slowlog::read(&dir, limit, params.since_ms))
            .await
            .map_err(|e| ApiError::internal(format!("blocking task failed: {e}")))?;
    Ok(Json(json!({"entries": entries, "truncated": truncated})))
}

/// `DELETE /repos/:repo/slow` — empties the log. Starting from an empty one is
/// how a slowdown is reproduced deliberately.
pub(super) async fn clear_slow_log(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let dir = slow_log_dir(&state, repo_uuid)?;
    let cleared = tokio::task::spawn_blocking(move || slowlog::clear(&dir))
        .await
        .map_err(|e| ApiError::internal(format!("blocking task failed: {e}")))?;
    Ok(Json(json!({"cleared": cleared})))
}

/// The log's directory for a loaded repository. Reading it does not need the
/// repository to be *ready*: a repository still warming is exactly one whose
/// slowness someone may be trying to explain.
fn slow_log_dir(state: &AppState, repo_uuid: Uuid) -> Result<PathBuf, ApiError> {
    Ok(slowlog::slow_dir(&state.repo(repo_uuid)?.internal_dir()))
}

/// `GET /diagnostics?since=&limit=` — the warnings the daemon printed to
/// stderr, so a client that did not start the daemon (the GUI runs as its own
/// process) can show them. `since` is the last id seen, 0 for "everything the
/// ring still holds"; the reply says where to resume and how many entries fell
/// out before it could read them.
#[derive(Debug, serde::Deserialize)]
pub struct DiagnosticsQuery {
    #[serde(default)]
    since: u64,
    limit: Option<usize>,
}

/// Bounds one page, whatever the client asks for: the feed is a debugging aid,
/// not a bulk endpoint.
const DIAGNOSTICS_MAX_PAGE: usize = 500;

pub(super) async fn diagnostics_since(
    Query(query): Query<DiagnosticsQuery>,
) -> Json<crate::diagnostics::Page> {
    let limit = query.limit.unwrap_or(DIAGNOSTICS_MAX_PAGE).min(DIAGNOSTICS_MAX_PAGE);
    Json(crate::diagnostics::read(query.since, limit))
}

/// The optional `?all=true` query parameter on `GET /repos` (include system repos).
#[derive(serde::Deserialize, Default)]
pub(super) struct ListReposParams {
    #[serde(default)]
    all: bool,
}

pub(super) async fn list_repos(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ListReposParams>,
) -> Json<serde_json::Value> {
    Json(serde_json::to_value(state.list_repos(params.all)).expect("repo list serialization"))
}

/// `GET /repos/:repo` — one loaded repository's info (404 if not loaded).
pub(super) async fn get_repo(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let info = state.repo_info(repo_uuid)?;
    Ok(Json(serde_json::to_value(info).expect("repo info serialization")))
}

#[derive(Deserialize)]
pub(super) struct RenameBody {
    name: String,
}

/// `PATCH /repos/:repo` — rename a loaded repository (409 on name clash,
/// persisted to config.json).
pub(super) async fn rename_repo(
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
pub(super) async fn list_all_tasks(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::to_value(state.all_tasks()).expect("tasks serialization"))
}

/// `GET /repos/:repo/tasks`: the repository's currently retained tasks.
pub(super) async fn list_repo_tasks(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo = state.repo(parse_uuid(&repo)?)?;
    Ok(Json(serde_json::to_value(repo.tasks.list()).expect("tasks serialization")))
}

/// `GET /repos/:repo/tasks/:task`: one task by id (404 if unknown or evicted).
pub(super) async fn get_task(
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
/// its transaction back); a running `query` stops inside its loops
/// (`crate::interrupt`); a `flush` stops and pauses ingestion. The task
/// transitions to `cancelled` once its worker unwinds; this returns the task's
/// current view. `load`, `prune` and `rollback` are not cancellable (400); a
/// terminal task is a 409; an unknown id a 404.
pub(super) async fn cancel_task(
    State(state): State<Arc<AppState>>,
    Path((repo, task)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    use crate::tasks::{CancelOutcome, TaskKind};
    let task_uuid = parse_uuid(&task)?;
    let repo = state.repo(parse_uuid(&repo)?)?;
    let kind = repo.tasks.get(task_uuid).map(|t| t.kind);
    match repo.tasks.request_cancel(task_uuid) {
        CancelOutcome::Requested => {
            // Stopping a flush is the same operation as pausing ingestion
            // (spec-file-tracking "Pausing ingestion"): set the pause here
            // rather than leaving it to the worker, so it holds even if that
            // flush happened to finish just before it saw the request —
            // otherwise the user asked for a stop and tracking carried on.
            if kind == Some(TaskKind::Flush) {
                repo.pause_ingestion();
            }
            repo.tasks
                .get(task_uuid)
                .map(|t| Json(serde_json::to_value(t).expect("task serialization")))
                .ok_or_else(|| ApiError::not_found(format!("Task not found: {task_uuid}")))
        }
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
pub(super) struct InitBody {
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

pub(super) async fn init_repo(
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

/// `POST /repos/:repo/check` — what no longer holds together in the store
/// (spec-storage increment 5): `{"problems": [...]}`.
pub(super) async fn check_repo(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let problems = repo_state.check_store()?;
        Ok(Json(json!({"problems": problems})))
    })
    .await
}

#[derive(Deserialize, Default)]
pub(super) struct BackupBody {
    /// A new directory for the backup; by default one under
    /// `internal/backups/`.
    #[serde(default)]
    to: Option<std::path::PathBuf>,
}

/// `POST /repos/:repo/backup` — a verified backup of the repository's store,
/// with its config (spec-storage increment 5). The body is optional.
pub(super) async fn backup_repo(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    body: axum::body::Bytes,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let body: BackupBody = if body.is_empty() {
        BackupBody::default()
    } else {
        serde_json::from_slice(&body)
            .map_err(|e| ApiError::bad_request(format!("invalid body: {e}")))?
    };
    with_repo(&state, repo_uuid, move |repo_state| {
        let info = repo_state.backup(body.to)?;
        Ok(Json(json!({
            "path": info.path,
            "created_at_ms": info.created_at_ms,
            "metarecords": info.metarecords,
        })))
    })
    .await
}

/// `POST /repos/:repo/reindex` — derives again what the store derives
/// (`mf repo reindex`).
pub(super) async fn reindex_repo(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        repo_state.reindex_store()?;
        Ok(Json(json!({})))
    })
    .await
}

#[derive(Deserialize, Default)]
pub(super) struct RestoreBody {
    /// The repository, when it is named by its path (`POST /repos/restore`).
    #[serde(default)]
    root: Option<PathBuf>,
    #[serde(default)]
    metafolder: Option<PathBuf>,
    /// The backup; by default the most recent one under `internal/backups/`.
    #[serde(default)]
    from: Option<PathBuf>,
}

fn restore_body(body: &[u8]) -> Result<RestoreBody, ApiError> {
    if body.is_empty() {
        return Ok(RestoreBody::default());
    }
    serde_json::from_slice(body).map_err(|e| ApiError::bad_request(format!("invalid body: {e}")))
}

fn restored_json(uuid: Uuid, restored: crate::backup::Restored) -> Json<serde_json::Value> {
    let info = restored.backup;
    Json(json!({
        "repo_uuid": hex(uuid),
        "backup": {
            "path": info.path,
            "created_at_ms": info.created_at_ms,
            "metarecords": info.metarecords,
        },
        "old_store": restored.old_store,
    }))
}

/// `POST /repos/:repo/restore` — restores a loaded repository from a backup
/// (spec-storage increment 5) and loads it back. The body (`from`) is
/// optional.
pub(super) async fn restore_loaded_repo(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    body: axum::body::Bytes,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let body = restore_body(&body)?;
    let metafolder = state.repo(repo_uuid)?.metafolder_dir.clone();
    let (uuid, restored) = tokio::task::spawn_blocking(move || {
        state.restore_repo(RepoLocator::Metafolder(metafolder), body.from)
    })
    .await
    .map_err(|e| ApiError::internal(format!("blocking task failed: {e}")))??;
    Ok(restored_json(uuid, restored))
}

/// `POST /repos/restore {root | metafolder, from?}` — restores a repository
/// named by its path, loaded or not (its store may be what no longer loads),
/// and loads it.
pub(super) async fn restore_repo(
    State(state): State<Arc<AppState>>,
    body: axum::body::Bytes,
) -> Result<Json<serde_json::Value>, ApiError> {
    let body = restore_body(&body)?;
    let locator = match (body.root, body.metafolder) {
        (Some(root), None) => RepoLocator::Root(root),
        (None, Some(dir)) => RepoLocator::Metafolder(dir),
        _ => return Err(ApiError::bad_request("exactly one of root or metafolder is required")),
    };
    let (uuid, restored) =
        tokio::task::spawn_blocking(move || state.restore_repo(locator, body.from))
            .await
            .map_err(|e| ApiError::internal(format!("blocking task failed: {e}")))??;
    Ok(restored_json(uuid, restored))
}

#[derive(Deserialize)]
pub(super) struct LoadBody {
    #[serde(default)]
    root: Option<PathBuf>,
    #[serde(default)]
    metafolder: Option<PathBuf>,
}

pub(super) async fn load_repo(
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
    // Finish the load in the background, as an observable `load` task so the
    // GUI shows a progress bar (spec-tasks). The repository is registered and
    // reports its state meanwhile, but answers its data routes with `503`
    // until the task is done; the response returns its uuid immediately, plus
    // the task's id (null when already warm) so the CLI can wait on it.
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
    if repo_state.is_ready() {
        return None; // already warm (e.g. re-load of a loaded repo)
    }
    let Some(task_id) = repo_state.tasks.start_unique(TaskKind::Load) else {
        // A warmup is already in progress: hand back its id.
        return repo_state.tasks.active_id(TaskKind::Load);
    };
    tokio::task::spawn_blocking(move || {
        repo_state.tasks.mark_running(task_id);
        let outcome = repo_state.warm(&|phase, done, total| {
            repo_state.tasks.set_progress(task_id, phase, done, total);
        });
        match outcome {
            Ok(()) => repo_state.tasks.finish(task_id, None),
            // The repository stays registered but never becomes ready: its data
            // endpoints keep answering 503, and this task says why.
            Err(e) => repo_state.tasks.fail(task_id, &e.message),
        }
    });
    Some(task_id)
}

/// `POST /repos/:repo/unload`: stops the repository's watcher/executor and
/// releases its database lock, removing it from the loaded set (spec-main
/// "Repository management"). 404 if not loaded; 409 if a rollback navigation is
/// in progress. Runs on a blocking thread because dropping the state joins the
/// executor thread.
pub(super) async fn unload_repo(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    tokio::task::spawn_blocking(move || state.unload_repo(repo_uuid))
        .await
        .map_err(|e| ApiError::internal(format!("blocking task failed: {e}")))??;
    Ok(Json(json!({"repo_uuid": hex(repo_uuid)})))
}
