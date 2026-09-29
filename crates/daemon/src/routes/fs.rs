//! The filesystem side: reconcile and track, mounts, watch status and
//! activity, orphans, duplicates, eligibility and ignore rules.

use super::*;

fn default_true() -> bool {
    true
}

#[derive(Deserialize)]
pub(super) struct ReconcileBody {
    /// Optional scope: when present, reconcile only the subtree rooted at this
    /// metarecord (32-char hex); absent reconciles the whole repository
    /// (doc "Task kinds"). The similarity `threshold` applies
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
/// (doc "Tasks"). Returns `202 Accepted` with the task id immediately; progress
/// and the final `ReconcileResult` are observed via `GET …/tasks/:id`. A
/// concurrent reconcile is rejected with `409`. With `metarecord` in the body
/// the reconcile is scoped to that metarecord's subtree; absent, it covers the
/// whole repository.
/// `GET /repos/:repo/mounts`: the repository's declared mount points — every
/// metarecord carrying `mfr_mount` — with the state read from disk right now
/// (spec-file-tracking "Mount status"). Read-only and cheap: one stat pair per
/// mount point, no walk. It is how a client explains a subtree that looks empty
/// or stale ("volume not mounted") instead of showing it as deleted.
///
/// The connection is not waited for. A long write holds it for its whole
/// transaction — a reconcile: minutes on a large repository
/// — and the GUI asks for the mount points on *every* directory listing, so
/// queueing turned a 2 ms answer into a measured 151 s one. The wait would buy
/// no freshness: the database half of the answer is the declared set, and a
/// writer in flight has committed none of its changes, so the resident set
/// ([`RepoState::declared_mounts`], filled by the load and refreshed by every
/// unblocked call) *is* the committed one. The half that must be current — is
/// the volume plugged in? — comes from the disk on every request either way.
pub(super) async fn mounts(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = slowlog::timed("wait:conn", || repo_state.conn.try_lock_recover());
        let declared = match conn {
            Some(conn) => {
                let set = Arc::new(crate::mount::declared_set(&conn, &repo_state.tree())?);
                repo_state.set_declared_mounts(Arc::clone(&set));
                set
            }
            None => repo_state.declared_mounts(),
        };
        let mounts = crate::mount::states(&declared, &repo_state.config.root);
        Ok(Json(json!({ "mounts": mounts })))
    })
    .await
}

/// The body all three `watch` routes answer with (spec-file-tracking "Watch
/// status, pause and resume"): whether ingestion is paused, how many
/// filesystem events are waiting to be applied, how long the executor waits
/// before applying them (`quiet_period_ms`), which watch source is active
/// (`backend`, [[spec-file-tracking "Watch sources and regimes"]]), and —
/// budget regime only — the two budget fields, which answer `null` under
/// coverage: no per-directory state, nothing to run out of.
///
/// The count is exact and always available: the buffer is in memory, so reading
/// it never queues behind the flush the caller may be trying to stop.
fn watch_view(repo_state: &RepoState) -> serde_json::Value {
    let share = repo_state.watch_budget_share();
    let limit = crate::watcher::kernel_watch_limit();
    let (watched_dirs, watch_budget) = if repo_state.watch_budget_regime() {
        (
            json!(repo_state.watched_dirs()),
            json!({
                "limit": limit,
                "share": share,
                "cap": crate::watcher::budget_cap_for(share),
                "starved": repo_state.starved_watches(),
                "exceeded_dirs": repo_state.exceeded_dirs(),
            }),
        )
    } else {
        (json!(null), json!(null))
    };
    json!({
        "paused": repo_state.is_ingestion_paused(),
        "pending_events": crate::executor::pending_count(repo_state),
        // How long after the filesystem goes quiet a change is recorded — what
        // a client re-reads after, having just changed the disk itself.
        "quiet_period_ms": repo_state.watch_quiet_period().as_millis() as u64,
        "backend": repo_state.watch_backend(),
        "backend_reason": repo_state.watch_backend_reason(),
        "watched_dirs": watched_dirs,
        "watch_budget": watch_budget,
    })
}

/// `GET /repos/:repo/watch`: whether the executor is ingesting filesystem
/// events for this repository, and how many are buffered.
pub(super) async fn watch_status(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let repo_state = state.repo(repo_uuid)?;
    Ok(Json(watch_view(&repo_state)))
}

/// `POST /repos/:repo/watch/pause`: stops the flush in progress (if any) and
/// keeps the executor from starting another (spec-file-tracking "Pausing
/// ingestion"). The buffered events are left in place; a resume applies them.
///
/// Deliberately *not* run through `with_repo`: it must answer while a flush
/// holds the connection — that is the whole point — so it touches only the
/// in-memory pause flag and the task registry.
pub(super) async fn watch_pause(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let repo_state = state.repo(repo_uuid)?;
    repo_state.pause_ingestion();
    Ok(Json(watch_view(&repo_state)))
}

/// `POST /repos/:repo/watch/resume`: resumes ingestion and pings the executor,
/// so the buffered events are applied after the usual quiet period.
pub(super) async fn watch_resume(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let repo_state = state.repo(repo_uuid)?;
    repo_state.resume_ingestion();
    Ok(Json(watch_view(&repo_state)))
}

/// The most children `GET /watch/activity` lists when no `limit` is given.
const ACTIVITY_DEFAULT_CHILDREN: usize = 50;

/// A repo-root-relative path as the watch routes take it: `""` for the root,
/// a leading slash otherwise.
fn watch_rel_path(path: &str) -> Result<crate::relpath::RelPath, ApiError> {
    if !path.is_empty() && !path.starts_with('/') {
        return Err(ApiError::bad_request(format!(
            "path must be repo-root-relative with a leading slash: {path:?}"
        )));
    }
    Ok(crate::relpath::RelPath::from_display(path))
}

fn activity_entry(path: &crate::relpath::RelPath, events: u64) -> serde_json::Value {
    json!({ "path": path.display(), "events": events })
}

#[derive(Deserialize)]
pub(super) struct WatchActivityBody {
    paths: Vec<String>,
}

/// `POST /repos/:repo/watch/activity`: how many watcher events were delivered
/// under each of the given paths (recursive; spec-file-tracking "Watch
/// activity"). In memory: never waits for the connection.
pub(super) async fn watch_activity_of(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<WatchActivityBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(body) = payload?;
    let repo_state = state.repo(parse_uuid(&repo)?)?;
    if body.paths.len() > ELIGIBILITY_MAX_PATHS {
        return Err(ApiError::bad_request(format!(
            "at most {ELIGIBILITY_MAX_PATHS} paths per call, got {}",
            body.paths.len()
        )));
    }
    let paths = body.paths.iter().map(|p| watch_rel_path(p)).collect::<Result<Vec<_>, _>>()?;
    let activity = repo_state.watch_activity.lock_recover();
    let results: Vec<_> = paths.iter().map(|p| activity_entry(p, activity.count(p))).collect();
    Ok(Json(json!({
        "since_ms": activity.since_ms(),
        "total": activity.total(),
        "results": results,
    })))
}

#[derive(Deserialize)]
pub(super) struct WatchActivityParams {
    #[serde(default)]
    path: String,
    limit: Option<usize>,
}

/// `GET /repos/:repo/watch/activity?path=&limit=`: one path's count and its
/// busiest direct children — one step of the walk down from the root to where
/// the events come from.
pub(super) async fn watch_activity_children(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    Query(params): Query<WatchActivityParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_state = state.repo(parse_uuid(&repo)?)?;
    let path = watch_rel_path(&params.path)?;
    let limit = params.limit.unwrap_or(ACTIVITY_DEFAULT_CHILDREN);
    let activity = repo_state.watch_activity.lock_recover();
    let children: Vec<_> =
        activity.children(&path, limit).iter().map(|(p, n)| activity_entry(p, *n)).collect();
    Ok(Json(json!({
        "since_ms": activity.since_ms(),
        "total": activity.total(),
        "path": path.display(),
        "events": activity.count(&path),
        "children": children,
    })))
}

/// `POST /repos/:repo/watch/activity/reset`: forgets every count and restarts
/// the clock.
pub(super) async fn watch_activity_reset(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_state = state.repo(parse_uuid(&repo)?)?;
    let mut activity = repo_state.watch_activity.lock_recover();
    activity.reset(metafolder_core::date::now_ms());
    Ok(Json(json!({ "since_ms": activity.since_ms(), "total": activity.total() })))
}

#[derive(Deserialize)]
pub(super) struct WatchCheckBody {
    paths: Vec<String>,
}

/// `POST /repos/:repo/watch/check`: the watcher's own answer for a batch of
/// repo-root-relative paths — would a change at each be recorded?
/// (spec-file-tracking "Watch check"). Where `POST /eligibility` explains the
/// tracking algorithm, this consults the *live watch set*: a tracked path
/// inside a watch-excluded subtree, under `.metafolder/internal/`, or on an
/// unplugged volume is still not watched, and the response names why.
pub(super) async fn watch_check(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<WatchCheckBody>, JsonRejection>,
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
        // What the answer is computed against is regime-specific: the live
        // watch set in the budget regime, the tree itself under coverage
        // (spec-file-tracking "Watch check").
        let watched = repo_state.watched_dir_set();
        let coverage = if repo_state.watch_budget_regime() {
            crate::watcher::Coverage::Watches(&watched)
        } else {
            crate::watcher::Coverage::Tree
        };
        let conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        let rules = repo_state.watch_rules(&conn)?;
        let cache = repo_state.tree();
        let statuses = crate::watcher::explain_watched(
            &conn,
            &cache,
            &rules,
            &repo_state.config.root,
            repo_state.internal_dir().as_path(),
            coverage,
            &body.paths,
        )?;
        let mut results = Vec::with_capacity(statuses.len());
        for (path, s) in body.paths.iter().zip(&statuses) {
            let e = &s.eligibility;
            let d = &s.dir_eligibility;
            results.push(json!({
                "path": path,
                "watched": s.watched,
                "reason": s.reason.as_str(),
                "watched_dir": s.watched_dir,
                "eligible": e.eligible,
                "eligibility_reason": e.reason.as_str(),
                "watch_scope": e.watch_scope,
                "ignore_source": e.ignore_source,
                "pattern": e.pattern,
                "dir_eligible": d.eligible,
                "dir_eligibility_reason": d.reason.as_str(),
                "dir_watch_scope": d.watch_scope,
                "dir_ignore_source": d.ignore_source,
                "dir_pattern": d.pattern,
                "excluded_by": s.excluded_by,
                "offline_mount": s.offline_mount,
            }));
        }
        Ok(Json(json!({ "results": results })))
    })
    .await
}

/// `POST /repos/:repo/orphans/scan`: read-only disk scan for tracked
/// metarecords whose `mfr_path` is definitely gone (spec-file-tracking "Orphan
/// scan"). Returns `{count, orphans: [{uuid, stale_path}]}`. Unlike reconcile it
/// writes nothing; unlike a query it consults the filesystem, so it is a
/// distinct operation rather than a predicate.
pub(super) async fn orphans_scan(
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
pub(super) struct OrphansClearBody {
    /// The metarecords to orphan — typically the uuids a prior scan returned.
    #[serde(default)]
    uuids: Vec<String>,
}

/// `POST /repos/:repo/orphans/clear`: orphan the given metarecords whose file is
/// still gone — snapshot `mfr_path_old`, set `mfr_path` to `Nothing`, cascade to
/// descendants (spec-file-tracking "Orphan scan"). Re-verifies each against the
/// disk, so a since-recreated file is skipped. Returns `{cleared}`.
pub(super) async fn orphans_clear(
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

/// `POST /repos/:repo/orphans/mark`: flag every orphaned metarecord with
/// `orphan = true` and take the flag back from records that are not orphaned
/// any more, in one revision (spec-file-tracking "Marking orphans"). Both
/// populations count: a stale `mfr_path` the disk scan proves gone, and one
/// already `Nothing`. Returns `{orphans, marked, unmarked}`.
pub(super) async fn orphans_mark(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        repo_state.ensure_writable()?;
        let result = orphans::mark_orphans(repo_state)?;
        Ok(Json(serde_json::to_value(result).unwrap_or_default()))
    })
    .await
}

pub(super) async fn full_reconcile(
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
        // Cooperative cancellation (doc "Tasks"): the reconcile polls this at its
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
pub(super) struct DuplicatesBody {
    /// Files smaller than this are skipped; `1` (the default) excludes
    /// zero-length files, which free nothing when removed.
    #[serde(default = "default_min_size")]
    min_size: i64,
    /// Ignore every stored hash and recompute.
    #[serde(default)]
    rehash: bool,
    /// Restrict the scan to this metarecord's subtree (32-char hex).
    #[serde(default)]
    metarecord: Option<String>,
}

fn default_min_size() -> i64 {
    1
}

impl Default for DuplicatesBody {
    fn default() -> Self {
        Self { min_size: default_min_size(), rehash: false, metarecord: None }
    }
}

/// `POST /repos/:repo/duplicates/scan`: starts a duplicate scan as a background
/// task (doc "Duplicates", doc "Task kinds"). Returns
/// `202 Accepted` with the task id immediately; the summary is read from
/// `GET …/tasks/:id`. A concurrent scan is rejected with `409`.
///
/// There is deliberately no listing endpoint: the scan *writes* its conclusion,
/// so reading it back is an ordinary query over the fields it wrote.
pub(super) async fn duplicates_scan(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Option<Json<DuplicatesBody>>,
) -> Result<Response, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let body = payload.map(|Json(b)| b).unwrap_or_default();
    if body.min_size < 0 {
        return Err(ApiError::bad_request("min_size must not be negative"));
    }
    let scope = body.metarecord.as_deref().map(parse_uuid).transpose()?;
    let repo_state = state.ready_repo(repo_uuid)?;
    repo_state.ensure_writable()?;
    let task_id = repo_state.tasks.start_unique(TaskKind::Duplicates).ok_or_else(|| {
        ApiError::conflict("a duplicate scan is already in progress for this repository")
    })?;

    let opts =
        crate::duplicates::ScanOptions { min_size: body.min_size, rehash: body.rehash, scope };
    tokio::task::spawn_blocking(move || {
        repo_state.tasks.mark_running(task_id);
        let progress = |phase: &str, done: Option<u64>, total: Option<u64>| {
            repo_state.tasks.set_progress(task_id, phase, done, total);
        };
        let cancel = || repo_state.tasks.is_cancel_requested(task_id);
        let outcome = crate::duplicates::scan_reported(
            &repo_state,
            &opts,
            &crate::tasks::Reporter::new(&progress, &cancel),
        );
        match outcome {
            Ok(result) => {
                let value = serde_json::to_value(result).expect("scan result serialization");
                repo_state.tasks.finish(task_id, Some(value));
            }
            Err(_) if cancel() => repo_state.tasks.mark_cancelled(task_id),
            Err(e) => repo_state.tasks.fail(task_id, &e.message),
        }
    });

    Ok((StatusCode::ACCEPTED, Json(json!({"task_id": hex(task_id)}))).into_response())
}

#[derive(Deserialize)]
pub(super) struct WatchExceededBody {
    /// Repo-root-relative path of the directory, leading `/`.
    path: String,
    /// `true` stops watching the subtree, `false` watches it again.
    exceeded: bool,
}

/// `GET /repos/:repo/watch/exceeded`: the subtree roots left unwatched for want
/// of budget (spec-file-tracking "The watch budget").
pub(super) async fn watch_exceeded_list(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        let cache = repo_state.tree();
        let uuids = crate::store::Questions::holding(
            &conn,
            crate::eligibility::WATCH_EXCEEDED,
            &metafolder_core::metarecord::Value::Bool(true),
        )?;
        let mut paths: Vec<String> = uuids
            .into_iter()
            .filter_map(|uuid| cache.path_of(&conn, "mfr_path", uuid).ok().flatten())
            .collect();
        paths.sort();
        Ok(Json(json!({ "count": paths.len(), "exceeded": paths })))
    })
    .await
}

/// `POST /repos/:repo/watch/exceeded`: sets or clears `mfr_watch_exceeded` on
/// one directory.
///
/// Setting it always succeeds — it only ever returns watches to the budget.
/// Clearing it is refused with `409` when the budget has no headroom, naming
/// what to give up first: watching that subtree would need watches that are not
/// there, and silently doing nothing would be worse than saying so. The
/// coverage regime has no headroom to run out of and always allows the clear
/// (spec-file-tracking "Watch sources and regimes").
pub(super) async fn watch_exceeded_set(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<WatchExceededBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let body = payload?.0;
    with_repo(&state, repo_uuid, move |repo_state| {
        repo_state.ensure_writable()?;
        if !body.exceeded && repo_state.watch_budget_regime() {
            let cap = crate::watcher::budget_cap_for(repo_state.watch_budget_share());
            let watched = repo_state.watched_dirs();
            if cap.is_some_and(|cap| watched >= cap) {
                return Err(ApiError::conflict(format!(
                    "the watch budget is full ({watched} of {} directories): \
                     give up another subtree first (`mf watch exceeded set <dir>`)",
                    cap.unwrap_or(watched)
                )));
            }
        }
        let uuid = {
            let conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
            let cache = repo_state.tree();
            cache
                .resolve_path(&conn, "mfr_path", &body.path)?
                .ok_or_else(|| ApiError::not_found(format!("No metarecord at {}", body.path)))?
        };
        {
            let mut conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
            let mut writer = repo_state.writer(&mut conn, None)?;
            if body.exceeded {
                writer.set_field(
                    uuid,
                    crate::eligibility::WATCH_EXCEEDED,
                    metafolder_core::metarecord::Value::Bool(true),
                )?;
            } else {
                writer.delete_fields_named(uuid, crate::eligibility::WATCH_EXCEEDED)?;
            }
            slowlog::timed("commit", || writer.commit())?;
        }
        // The watch set follows immediately, as it does for `mf_watch`.
        let conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        let watched = repo_state.refresh_watches(&conn);
        Ok(Json(json!({ "path": body.path, "exceeded": body.exceeded, "watched_dirs": watched })))
    })
    .await
}

/// `POST /repos/:repo/orphans/relink`: re-home orphaned metarecords onto the
/// files that carry their content (spec-file-tracking "Relinking orphans").
///
/// Asynchronous like the duplicate scan, and for the same reason: it hashes
/// candidate files. `202` with the task id; the result travels with the task.
pub(super) async fn orphans_relink(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
) -> Result<Response, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let repo_state = state.ready_repo(repo_uuid)?;
    repo_state.ensure_writable()?;
    let task_id = repo_state
        .tasks
        .start_unique(TaskKind::Relink)
        .ok_or_else(|| ApiError::conflict("a relink is already in progress for this repository"))?;

    tokio::task::spawn_blocking(move || {
        repo_state.tasks.mark_running(task_id);
        let progress = |phase: &str, done: Option<u64>, total: Option<u64>| {
            repo_state.tasks.set_progress(task_id, phase, done, total);
        };
        let cancel = || repo_state.tasks.is_cancel_requested(task_id);
        let outcome =
            orphans::relink_reported(&repo_state, &crate::tasks::Reporter::new(&progress, &cancel));
        match outcome {
            Ok(result) => {
                let value = serde_json::to_value(result).expect("relink result serialization");
                repo_state.tasks.finish(task_id, Some(value));
            }
            Err(_) if cancel() => repo_state.tasks.mark_cancelled(task_id),
            Err(e) => repo_state.tasks.fail(task_id, &e.message),
        }
    });

    Ok((StatusCode::ACCEPTED, Json(json!({"task_id": hex(task_id)}))).into_response())
}

#[derive(Deserialize)]
pub(super) struct TrackBody {
    path: PathBuf,
}

#[derive(Deserialize)]
pub(super) struct EligibilityBody {
    paths: Vec<String>,
}

/// The most paths one `POST /eligibility` call may explain. A directory
/// listing is the intended unit; a bigger batch is a client bug, answered while
/// holding the repository's connection.
pub(super) const ELIGIBILITY_MAX_PATHS: usize = 1000;

/// `POST /repos/:repo/eligibility`: read-only dry run of the watch/ignore
/// algorithm for a batch of repo-root-relative paths, each with the reason it
/// was decided (spec-file-tracking "Eligibility explain"). Answered from the
/// rule index: no path costs a store read.
pub(super) async fn eligibility_explain(
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
        let conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        let rules = repo_state.watch_rules(&conn)?;
        let mut results = Vec::with_capacity(body.paths.len());
        for path in &body.paths {
            let e = rules.explain(&rules.rel_of_text(path))?;
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
pub(super) struct EffectiveIgnoreParams {
    #[serde(default)]
    path: String,
}

/// `GET /repos/:repo/ignore/effective?path=<rel>`: the `mf_ignore` set that
/// governs a directory and where it comes from (spec-file-tracking "Effective
/// ignore set") — what a client needs to warn that writing here would shadow an
/// inherited set rather than extend it.
pub(super) async fn effective_ignore(
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
        let conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        let rules = repo_state.watch_rules(&conn)?;
        let e = rules.effective_ignore(&rules.rel_of_text(&params.path));
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
pub(super) async fn track(
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
        // Each component keeps its exact bytes: a POSIX name need not be UTF-8,
        // and such a file is tracked like any other (spec-data-model
        // "Tree names").
        let mut rel = crate::relpath::RelPath::root();
        for comp in rel_path.components() {
            let std::path::Component::Normal(name) = comp else {
                return Err(ApiError::bad_request(format!(
                    "unsupported path component in {abs:?}"
                )));
            };
            rel = rel.child(metafolder_core::metarecord::TreeName::from_bytes(
                crate::relpath::file_name_bytes(name),
            ));
        }
        if rel.is_root() {
            return Err(ApiError::bad_request("cannot track the repository root itself"));
        }

        let mut conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        let cache = repo_state.tree();
        // Idempotent: a path already tracked returns its existing metarecord
        // uuid rather than an error, so callers can `track` without first
        // checking (spec-file-tracking "Single-metarecord track").
        if let Some(existing) = cache.resolve_path(&conn, "mfr_path", &rel.display())? {
            return Ok(Json(json!({"uuid": hex(existing)})));
        }
        let untracked = [Field::new("mf_watch", Value::Bool(false))];
        let mut writer = repo_state.writer(&mut conn, None)?;
        let uuid = crate::reconcile::create_record_for(
            &mut writer,
            &cache,
            &repo_state.config.root,
            &rel,
            &untracked,
            false,
        )?;
        slowlog::timed("commit", || writer.commit())?;
        Ok(Json(json!({"uuid": hex(uuid)})))
    })
    .await
}
