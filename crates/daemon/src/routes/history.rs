//! The event log (doc "Event log"): reading it, labelling revisions, pruning,
//! the atomic rollback and the coordinated navigation.

use super::*;

/// Serializes one operation row, optionally with its snapshots.
fn op_json(
    conn: &dyn crate::store::Store,
    op: &crate::log::OpRow,
    include_snapshots: bool,
) -> Result<serde_json::Value, ApiError> {
    Ok(crate::log_view::op_json(conn, op, include_snapshots)?)
}

pub(super) fn snapshots_json(
    conn: &dyn crate::store::Store,
    op_id: i64,
    is_new: i64,
) -> Result<serde_json::Value, ApiError> {
    Ok(crate::log_view::snapshots_json(conn, op_id, is_new != 0)?)
}

fn revision_json(log: &dyn crate::store::Log, rev_id: i64) -> Result<serde_json::Value, ApiError> {
    let meta = log.revisions(&[rev_id])?;
    let m = meta
        .get(&rev_id)
        .ok_or_else(|| ApiError::not_found(format!("revision {rev_id} not found")))?;
    Ok(json!({"id": rev_id, "timestamp": m.timestamp, "label": m.label, "origin": m.origin}))
}

#[derive(Deserialize)]
pub(super) struct LogParams {
    #[serde(default)]
    mode: Option<String>,
    /// Cap on the number of *revisions* returned (whole ones), for a client
    /// that displays a history by revision rather than by operation.
    #[serde(default)]
    revisions: Option<usize>,
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

pub(super) async fn get_log(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    Query(params): Query<LogParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let mode_name = params.mode.as_deref().unwrap_or("linear");
    let mode = crate::log_view::Mode::parse(mode_name).ok_or_else(|| {
        ApiError::bad_request(format!(
            "invalid mode '{mode_name}' (expected 'linear', 'active' or 'tree')"
        ))
    })?;
    let query = crate::log_view::LogQuery {
        mode,
        limit: params.limit,
        revisions: params.revisions,
        entity: params.metarecord_uuid.as_deref().map(parse_uuid).transpose()?,
        since: params.since,
        until: params.until,
        include_snapshots: params.include_snapshots.unwrap_or(false),
    };
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        Ok(Json(crate::log_view::listing(&*conn, &query)?))
    })
    .await
}

#[derive(Deserialize)]
pub(super) struct SinceParams {
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
pub(super) async fn get_log_since(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    Query(params): Query<SinceParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        let head = crate::store::Log::head(&*conn)?;
        let limit = params.limit.unwrap_or(SINCE_DEFAULT_LIMIT).max(0);
        let mut truncated = false;
        let operations = match params.op {
            Some(since) => {
                if crate::store::Log::ops_after_count(&*conn, since)? > limit {
                    // Oversized delta: signal a coarse refresh instead of
                    // streaming every operation (a large reconcile would flood
                    // the client).
                    truncated = true;
                    Vec::new()
                } else {
                    let ops = crate::store::Log::ops_after(&*conn, since)?;
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

/// Which operations of a revision `GET /log/revisions/:rev_id` details: a
/// window (`offset`/`limit`) or one operation by id (`op`). A revision can hold
/// every operation of a reconcile, and each carries its snapshots.
#[derive(Deserialize)]
pub(super) struct RevisionParams {
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    op: Option<i64>,
}

pub(super) async fn get_revision(
    State(state): State<Arc<AppState>>,
    Path((repo, rev_id)): Path<(String, String)>,
    Query(params): Query<RevisionParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        let head = crate::store::Log::head(&*conn)?;
        let rev_id: i64 = if rev_id == "head" {
            let head =
                head.ok_or_else(|| ApiError::not_found("the history is empty (no HEAD revision)"))?;
            crate::store::Log::op(&*conn, head)?
                .ok_or_else(|| ApiError::internal("HEAD operation vanished"))?
                .rev_id
        } else {
            rev_id
                .parse()
                .map_err(|_| ApiError::bad_request(format!("invalid revision id '{rev_id}'")))?
        };

        let mut revision = revision_json(&*conn, rev_id)?;
        let all = crate::store::Log::revision_ops(&*conn, rev_id)?;
        // HEAD and the count are the whole revision's, whatever the window.
        revision["is_head"] = json!(all.iter().any(|op| Some(op.id) == head));
        revision["operation_count"] = json!(all.len());
        let selected: Vec<&crate::log::OpRow> = match params.op {
            Some(id) => {
                let op = all.iter().find(|op| op.id == id).ok_or_else(|| {
                    ApiError::not_found(format!("operation {id} is not in revision {rev_id}"))
                })?;
                vec![op]
            }
            None => all
                .iter()
                .skip(params.offset.unwrap_or(0))
                .take(params.limit.unwrap_or(usize::MAX))
                .collect(),
        };
        let mut ops = Vec::with_capacity(selected.len());
        for op in selected {
            ops.push(op_json(&conn, op, true)?);
        }
        Ok(Json(json!({"revision": revision, "operations": ops})))
    })
    .await
}

#[derive(Deserialize)]
pub(super) struct LabelBody {
    label: Option<String>,
}

pub(super) async fn patch_revision(
    State(state): State<Arc<AppState>>,
    Path((repo, rev_id)): Path<(String, i64)>,
    payload: Result<Json<LabelBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let mut conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        let tx = crate::store::Begin::begin_write(&mut *conn)?;
        if !tx.set_revision_label(rev_id, body.label.as_deref())? {
            return Err(ApiError::not_found(format!("revision {rev_id} not found")));
        }
        tx.commit()?;
        Ok(Json(revision_json(&*conn, rev_id)?))
    })
    .await
}

/// A rollback/prune target: exactly one of the four forms.
#[derive(Deserialize)]
pub(super) struct TargetBody {
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
pub(super) struct RollbackBody {
    target: TargetBody,
}

/// Metadata-only atomic rollback (doc "Rollback endpoints").
pub(super) async fn rollback(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<RollbackBody>, JsonRejection>,
) -> Result<Json<crate::log::NavResult>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    let target = body.target.into_target()?;
    with_repo(&state, repo_uuid, move |repo_state| {
        // Observation-only task (doc "Tasks"), like prune: rollback rewrites
        // arbitrary state under the connection lock.
        observed(repo_state, TaskKind::Rollback, "rolling back", |repo_state| {
            repo_state.ensure_writable()?;
            let mut conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
            let resolved = crate::log::resolve_target(&*conn, &target)?;
            let result = crate::log::navigate(&mut *conn, resolved)?;
            // And the watch set with it: navigation restores `mf_watch`/
            // `mf_ignore` rows like any other, so the live watches must follow
            // the state HEAD landed on (doc "Navigation").
            repo_state.refresh_watches(&conn);
            Ok(Json(result))
        })
    })
    .await
}

/// Runs `f` as an observation-only task (doc "Tasks"): registers a task of
/// `kind`, marks it running with `phase`, and records its terminal state. Like
/// `query`, the operation's result travels with the HTTP response, so the task
/// carries no result payload and its counts stay unknown. Used for the
/// synchronous, connection-lock-holding log operations (prune, rollback) so
/// other clients can see why their work is queued.
pub(super) fn observed<T>(
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
pub(super) struct PruneBody {
    mode: String,
    target: TargetBody,
}

pub(super) async fn prune_log(
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
        // Observation-only task (doc "Tasks"): the result travels with this
        // response, so the task carries no result payload and its counts stay
        // unknown. Registered because prune holds the connection lock and can be
        // long on a large log, so other clients see why their work is queued.
        observed(repo_state, TaskKind::Prune, "pruning", |repo_state| {
            repo_state.ensure_writable()?;
            let mut conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
            let resolved = crate::log::resolve_target(&*conn, &target)?
                .ok_or_else(|| ApiError::bad_request("cannot prune to the empty state"))?;
            let (ops, revisions) = crate::log::prune(&mut *conn, mode, resolved)
                .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
            Ok(Json(json!({"pruned_operations": ops, "pruned_revisions": revisions})))
        })
    })
    .await
}

// ── Coordinated navigation (doc "Filesystem coordination") ────────────

/// Query-parameter target form for the plan endpoints.
#[derive(Deserialize)]
pub(super) struct PlanParams {
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
pub(super) fn snapshot_abs_path(
    conn: &dyn crate::store::Store,
    cache: &crate::tree_cache::TreeCache,
    root: &std::path::Path,
    op_id: i64,
    is_new: i64,
) -> Result<Option<String>, ApiError> {
    for row in conn.snapshots(op_id, is_new != 0)? {
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

/// Builds the action JSON for one navigation step (doc "Rollback endpoints": the
/// response `op_type` reflects the *action to execute* — a stored `file_moved`
/// becomes `move_file` with `from`/`to`; everything else is unchanged).
pub(super) fn action_op_json(
    conn: &dyn crate::store::Store,
    cache: &crate::tree_cache::TreeCache,
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
    // Who wrote the revision this operation belongs to. Spelled out in full:
    // the revert plan already uses `origin` for whether an operation was
    // requested or dragged in as a dependent.
    if let Some(origin) = &op.origin {
        value["revision_origin"] = json!(origin);
    }
    // What the client has to do on disk for this step, in the revert plan's
    // shape. The op type alone cannot say: a trashing's `delete_metarecord`
    // moves bytes out of the trash-bin when it is undone and back into it when
    // it is redone, an ordinary one touches no file, and the undo a revert
    // wrote of a trashing is an ordinary-looking `create_metarecord`
    // (doc "Trash, undo and redo").
    value["filesystem"] = match crate::revert::nav_fs_action(conn, op, dir)? {
        None => serde_json::Value::Null,
        Some(crate::revert::FsAction::Move) => json!({"action": "move"}),
        Some(crate::revert::FsAction::RestoreContent) => json!({"action": "restore_content"}),
        Some(crate::revert::FsAction::TrashContent) => json!({"action": "trash_content"}),
    };
    // For an inverse (rollback) step, expose the metarecord version this step
    // restores to (`entity_version_before`). The CLI matches it against a trash
    // entry's recorded version to auto-restore the exact file the deletion
    // displaced. Omitted on forward (redo) steps, so auto-restore never fires
    // while re-applying a deletion.
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
        if let Some(v) = conn.version_before_revision(op.rev_id, op.entity_uuid)? {
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

pub(super) async fn rollback_plan(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    Query(params): Query<PlanParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let target = params.into_target()?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        let head = crate::store::Log::head(&*conn)?;
        let resolved = crate::log::resolve_target(&*conn, &target)?;
        let path = crate::log::nav_path(&*conn, head, resolved)?;
        let cache = repo_state.tree();
        let mut ops = Vec::with_capacity(path.len());
        for (op, dir) in &path {
            ops.push(action_op_json(&conn, &cache, &repo_state.config.root, op, *dir)?);
        }
        let total = ops.len();
        Ok(Json(json!({"operations": ops, "total": total})))
    })
    .await
}

pub(super) async fn rollback_plan_summary(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    Query(params): Query<PlanParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let target = params.into_target()?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        let head = crate::store::Log::head(&*conn)?;
        let resolved = crate::log::resolve_target(&*conn, &target)?;
        let path = crate::log::nav_path(&*conn, head, resolved)?;
        let mut by_type: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        let mut revs = std::collections::HashSet::new();
        // The steps with something to do on disk: none, and the navigation can
        // rewind in the database alone, in one call (`POST /rollback`).
        let mut filesystem_steps = 0usize;
        for (op, dir) in &path {
            *by_type.entry(op.op_type.clone()).or_insert(0) += 1;
            revs.insert(op.rev_id);
            if crate::revert::nav_fs_action(&*conn, op, *dir)?.is_some() {
                filesystem_steps += 1;
            }
        }
        Ok(Json(json!({
            "total_operations": path.len(),
            "by_type": by_type,
            "revisions_affected": revs.len(),
            "filesystem_steps": filesystem_steps,
        })))
    })
    .await
}

pub(super) async fn rollback_start(
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
        let conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        let head = crate::store::Log::head(&*conn)?;
        let resolved = crate::log::resolve_target(&*conn, &target)?;
        if resolved == head {
            // Nothing to do: the lock is not entered.
            return Ok(Json(json!({"op": null, "remaining": 0})));
        }
        let plan = crate::log::NavPlan::new(&*conn, resolved)?;
        let (id, dir) = plan.next().expect("a non-empty plan when head != target");
        let op = crate::store::Log::op(&*conn, id)?
            .ok_or_else(|| ApiError::internal("operation vanished during navigation"))?;
        let cache = repo_state.tree();
        let first = action_op_json(&conn, &cache, &repo_state.config.root, &op, dir)?;
        let remaining = plan.len() - 1;
        drop(conn);
        *repo_state.rollback_lock.lock_recover() = Some(RollbackLock::Navigate { plan });
        Ok(Json(json!({"op": first, "remaining": remaining})))
    })
    .await
}
