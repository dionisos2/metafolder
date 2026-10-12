//! The event log (doc "Event log"): reading it, labelling revisions and
//! pruning. The log is a sequence: nothing navigates over it (doc "The
//! operation log").

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
    let query = crate::log_view::LogQuery {
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
/// created after `?op=<id>` (each names its `entity_uuid`).
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
                if crate::store::Log::ops_after_count(&*conn, since, limit.saturating_add(1))?
                    > limit
                {
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

/// A prune target: exactly one of the three forms.
#[derive(Deserialize)]
pub(super) struct TargetBody {
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    timestamp: Option<i64>,
    #[serde(default)]
    label: Option<String>,
}

impl TargetBody {
    fn into_target(self) -> Result<crate::log::Target, ApiError> {
        match (self.id, self.timestamp, self.label) {
            (Some(id), None, None) => Ok(crate::log::Target::Id(id)),
            (None, Some(ts), None) => Ok(crate::log::Target::Timestamp(ts)),
            (None, None, Some(label)) => Ok(crate::log::Target::Label(label)),
            _ => Err(ApiError::bad_request(
                "target must be exactly one of {id}, {timestamp}, {label}",
            )),
        }
    }
}

/// Runs `f` as an observation-only task (doc "Tasks"): registers a task of
/// `kind`, marks it running with `phase`, and records its terminal state. Like
/// `query`, the operation's result travels with the HTTP response, so the task
/// carries no result payload and its counts stay unknown. Used for the
/// synchronous, connection-lock-holding log operations (prune) so
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
    target: TargetBody,
}

pub(super) async fn prune_log(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<PruneBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    let target = body.target.into_target()?;
    with_repo(&state, repo_uuid, move |repo_state| {
        // Observation-only task (doc "Tasks"): the result travels with this
        // response, so the task carries no result payload and its counts stay
        // unknown. Registered because prune holds the connection lock and can be
        // long on a large log, so other clients see why their work is queued.
        observed(repo_state, TaskKind::Prune, "pruning", |repo_state| {
            repo_state.ensure_writable()?;
            let mut conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
            let resolved = crate::log::resolve_target(&*conn, &target)?;
            let (ops, revisions) = crate::log::prune(&mut *conn, resolved)
                .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
            Ok(Json(json!({"pruned_operations": ops, "pruned_revisions": revisions})))
        })
    })
    .await
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
