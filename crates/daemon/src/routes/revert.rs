//! Revert (doc "Revert"): the plan, the one-shot revert and the
//! coordinated one.

use super::*;
use std::collections::HashSet;

/// The set of operations to revert: a label's — its children's included —
/// or an explicit list.
#[derive(Deserialize)]
pub(super) struct RevertTarget {
    #[serde(default)]
    label_id: Option<serde_json::Value>,
    #[serde(default)]
    op_ids: Option<Vec<i64>>,
    /// With `label_id`: only its standing `changes` (an undo) or its standing
    /// `undos` (a redo) — doc "Undo and redo by label".
    #[serde(default)]
    only: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct RevertBody {
    target: RevertTarget,
    #[serde(default)]
    skip_filesystem: bool,
}

#[derive(Deserialize)]
pub(super) struct RevertPlanParams {
    #[serde(default)]
    target_label_id: Option<String>,
    #[serde(default)]
    target_op_ids: Option<String>,
    #[serde(default)]
    only: Option<String>,
}

/// Resolves a revert target to its operations, oldest first.
fn resolve_revert_target(
    conn: &dyn crate::store::Store,
    head: Option<i64>,
    target: &RevertTarget,
) -> Result<Vec<crate::log::OpRow>, ApiError> {
    let ops = match (&target.label_id, &target.op_ids) {
        (Some(label), None) => {
            let label_id = match label {
                serde_json::Value::String(s) if s == "head" => {
                    let head = head.ok_or_else(|| ApiError::not_found("the log is empty"))?;
                    conn.op(head)?
                        .ok_or_else(|| ApiError::not_found("HEAD names no operation"))?
                        .label_id
                }
                serde_json::Value::Number(n) => n
                    .as_i64()
                    .ok_or_else(|| ApiError::bad_request("label_id must be an integer"))?,
                _ => return Err(ApiError::bad_request("label_id must be a number or \"head\"")),
            };
            // A label nothing was written in reverts nothing; one that does not
            // exist (or was dropped with its last operation) is a 404.
            if conn.labels(&[label_id])?.is_empty() {
                return Err(ApiError::not_found(format!(
                    "no label {label_id}: it never existed, or what it held was pruned or \
                     trimmed away"
                )));
            }
            let ops = crate::revert::label_ops(conn, label_id)?;
            match target.only.as_deref() {
                None => ops,
                Some(only) => {
                    let only = crate::revert::Only::parse(only).ok_or_else(|| {
                        ApiError::bad_request(format!(
                            "only must be \"changes\" or \"undos\", not \"{only}\""
                        ))
                    })?;
                    crate::revert::narrow(conn, ops, only)?
                }
            }
        }
        (None, Some(ids)) => {
            if target.only.is_some() {
                return Err(ApiError::bad_request("only narrows a label_id target"));
            }
            if ids.is_empty() {
                return Err(ApiError::bad_request("op_ids must not be empty"));
            }
            let mut ops = Vec::with_capacity(ids.len());
            for id in ids {
                ops.push(conn.op(*id)?.ok_or_else(|| {
                    ApiError::not_found(format!(
                        "operation {id} does not exist, or was pruned or trimmed away"
                    ))
                })?);
            }
            ops.sort_by_key(|o| o.id);
            ops
        }
        _ => {
            return Err(ApiError::bad_request("the target needs exactly one of label_id or op_ids"))
        }
    };
    Ok(ops)
}

/// The name of the label a revert opens when the request names none.
fn revert_name(target: &RevertTarget) -> String {
    match (&target.label_id, &target.op_ids) {
        (Some(label), _) => {
            let what = match target.only.as_deref() {
                Some("changes") => "undo",
                Some("undos") => "redo",
                _ => "revert",
            };
            format!("{what} of label {}", label.as_str().unwrap_or(&label.to_string()))
        }
        (_, Some(ids)) => revert_name_of(ids),
        _ => "revert".to_string(),
    }
}

/// "revert of operation 12", "revert of 3 operations".
fn revert_name_of(ids: &[i64]) -> String {
    match ids {
        [one] => format!("revert of operation {one}"),
        many => format!("revert of {} operations", many.len()),
    }
}

fn op_brief(op: &crate::log::OpRow) -> serde_json::Value {
    json!({
        "op_id": op.id,
        "label_id": op.label_id,
        "op_type": op.op_type,
        "entity_uuid": hex(op.entity_uuid),
        "field_name": op.field_name,
    })
}

/// Builds the plan body. `fs` is given by the coordinated form only: it
/// resolves the paths a `move` action needs, which costs a forest walk per
/// operation and is useless to a caller that will not touch the filesystem.
fn revert_plan_json(
    conn: &dyn crate::store::Store,
    ops: &[crate::log::OpRow],
    mut fs: Option<(&crate::tree_cache::TreeCache, &std::path::Path)>,
) -> Result<serde_json::Value, ApiError> {
    let mut operations = Vec::with_capacity(ops.len());
    let mut requires_lock = false;
    for op in ops {
        let action = crate::revert::fs_action(conn, op)?;
        requires_lock |= action.is_some();
        let restores = snapshots_json(conn, op.id, 0)?;
        // A warning, never a refusal (doc "Reverting onto the current state").
        let changed_since = match crate::revert::changed_since(conn, op)? {
            crate::revert::Changed::No => serde_json::Value::Null,
            crate::revert::Changed::By(by) => {
                let mut v = op_brief(&by);
                v["timestamp"] = json!(by.timestamp);
                v
            }
            crate::revert::Changed::Unlogged => json!({}),
        };
        let filesystem = match &action {
            None => serde_json::Value::Null,
            Some(crate::revert::FsAction::Move) => {
                let mut v = json!({"action": "move"});
                if let Some((cache, root)) = fs.as_mut() {
                    // Undoing a move: from where the file is recorded now
                    // (`is_new=1`) back to where it was (`is_new=0`).
                    let from = snapshot_abs_path(conn, cache, root, op.id, 1)?;
                    let to = snapshot_abs_path(conn, cache, root, op.id, 0)?;
                    if let (Some(from), Some(to)) = (from, to) {
                        v["from"] = json!(from);
                        v["to"] = json!(to);
                    }
                }
                v
            }
            Some(crate::revert::FsAction::RestoreContent) => json!({"action": "restore_content"}),
            Some(crate::revert::FsAction::TrashContent) => json!({"action": "trash_content"}),
        };
        let mut entry = json!({
            "id": op.id,
            "op_type": op.op_type,
            "entity_uuid": hex(op.entity_uuid),
            "field_name": op.field_name,
            "writes": crate::revert::written_as(op).as_str(),
            "restores": restores,
            "filesystem": filesystem,
            "changed_since": changed_since,
        });
        if matches!(action, Some(crate::revert::FsAction::RestoreContent)) {
            // The trash correlation key: the version the record held before
            // the whole label — one flush of the watcher — which is the only
            // one anything outside it observed (doc "Trash, undo and redo").
            if let Some(v) = op.entity_version_before {
                entry["entity_version_before"] = json!(v);
            }
            if let Some(v) = conn.version_before_label(op.label_id, op.entity_uuid)? {
                entry["entity_version_before_label"] = json!(v);
            }
        }
        operations.push(entry);
    }
    Ok(json!({
        "requires_lock": requires_lock,
        "operations": operations,
    }))
}

pub(super) async fn revert_plan(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    Query(params): Query<RevertPlanParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let target = RevertTarget {
        // `head` is a legal label_id here as it is in the body, so a plan and
        // the revert it describes take the same target spelling.
        label_id: params.target_label_id.as_ref().map(|s| match s.parse::<i64>() {
            Ok(n) => serde_json::Value::from(n),
            Err(_) => serde_json::Value::String(s.clone()),
        }),
        op_ids: params
            .target_op_ids
            .as_ref()
            .map(|s| s.split(',').filter_map(|p| p.trim().parse::<i64>().ok()).collect::<Vec<_>>()),
        only: params.only,
    };
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        let head = crate::store::Log::head(&*conn)?;
        let ops = resolve_revert_target(&conn, head, &target)?;
        Ok(Json(revert_plan_json(&conn, &ops, None)?))
    })
    .await
}

pub(super) async fn revert(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<RevertBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        repo_state.ensure_writable()?;
        let mut conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        let head = crate::store::Log::head(&*conn)?;
        let mut effective = resolve_revert_target(&conn, head, &body.target)?;
        let mut skipped = Vec::new();
        // Which operations need a file action: asked of the log once, since a
        // trashing's is read through its `reverts_op_id` chain.
        let mut needs_fs = HashSet::new();
        for op in &effective {
            if crate::revert::fs_action(&*conn, op)?.is_some() {
                needs_fs.insert(op.id);
            }
        }
        if body.skip_filesystem {
            effective.retain(|op| {
                if needs_fs.contains(&op.id) {
                    skipped.push(json!({"op_id": op.id, "reason": "requires_filesystem"}));
                    false
                } else {
                    true
                }
            });
        } else if let Some(op) = effective.iter().find(|o| needs_fs.contains(&o.id)) {
            return Err(ApiError::conflict(format!(
                "operation {} needs a filesystem action; use the coordinated revert, or pass \
                 skip_filesystem to leave it out",
                op.id
            )));
        }

        if effective.is_empty() {
            // Nothing left to apply: writing nothing records nothing.
            return Ok(Json(json!({
                "label": serde_json::Value::Null,
                "head": head,
                "reverted_operations": [],
                "skipped_operations": skipped,
            })));
        }

        let context = request_context().named(revert_name(&body.target));
        let mut writer = repo_state.writer(&mut conn, context)?;
        let applied: Vec<i64> = effective.iter().map(|o| o.id).collect();
        crate::revert::apply(&mut writer, &effective)?;
        let label_id = writer.label_id();
        let effects = writer.effects();
        slowlog::timed("commit", || writer.commit())?;
        repo_state.settle(&conn, &effects)?;
        let new_head = crate::store::Log::head(&*conn)?;
        Ok(Json(json!({
            "label": label_id,
            "head": new_head,
            "reverted_operations": applied,
            "skipped_operations": skipped,
        })))
    })
    .await
}

/// The set the revert actually writes, given the client's `apply` list.
fn narrow_to_applied(
    effective: Vec<crate::log::OpRow>,
    apply: &[i64],
) -> Result<(Vec<crate::log::OpRow>, Vec<serde_json::Value>), ApiError> {
    let wanted: std::collections::HashSet<i64> = apply.iter().copied().collect();
    if let Some(stray) = wanted.iter().find(|id| !effective.iter().any(|o| o.id == **id)) {
        return Err(ApiError::bad_request(format!(
            "operation {stray} is not part of the revert this lock was started for; \
             commit may only narrow the set start fixed"
        )));
    }
    let mut skipped = Vec::new();
    let kept: Vec<crate::log::OpRow> = effective
        .into_iter()
        .filter(|op| {
            if wanted.contains(&op.id) {
                true
            } else {
                skipped.push(json!({"op_id": op.id, "reason": "client_skipped"}));
                false
            }
        })
        .collect();
    Ok((kept, skipped))
}

#[derive(Deserialize)]
pub(super) struct RevertStartBody {
    target: RevertTarget,
}

/// `POST /revert/start`: enters the lock and returns the plan, with the paths
/// each `move` action needs.
pub(super) async fn revert_start(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<RevertStartBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        if repo_state.is_revert_locked() {
            return Err(ApiError::conflict("a coordinated revert is already in progress"));
        }
        let conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        let head = crate::store::Log::head(&*conn)?;
        let effective = resolve_revert_target(&conn, head, &body.target)?;
        let plan = {
            let cache = repo_state.tree();
            revert_plan_json(&conn, &effective, Some((&cache, &repo_state.config.root)))?
        };
        drop(conn);
        *repo_state.revert_lock.lock_recover() =
            Some(RevertLock { ops: effective.iter().map(|o| o.id).collect() });
        Ok(Json(plan))
    })
    .await
}

#[derive(Deserialize)]
pub(super) struct RevertCommitBody {
    #[serde(default)]
    apply: Vec<i64>,
}

/// `POST /revert/commit`: writes the revert as one write, releases the lock
/// and replays the watcher buffer.
pub(super) async fn revert_commit(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<RevertCommitBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let locked: Vec<i64> = match repo_state.revert_lock.lock_recover().as_ref() {
            Some(RevertLock { ops }) => ops.clone(),
            None => {
                return Err(ApiError::conflict("no revert in progress; call revert/start first"))
            }
        };

        let result = (|| -> Result<serde_json::Value, ApiError> {
            let mut conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
            let head = crate::store::Log::head(&*conn)?;
            let mut effective = Vec::with_capacity(locked.len());
            for id in &locked {
                effective.push(crate::store::Log::op(&*conn, *id)?.ok_or_else(|| {
                    ApiError::not_found(format!("operation {id} vanished during the revert"))
                })?);
            }
            let (kept, skipped) = narrow_to_applied(effective, &body.apply)?;
            if kept.is_empty() {
                return Ok(json!({
                    "label": serde_json::Value::Null,
                    "head": head,
                    "reverted_operations": [],
                    "skipped_operations": skipped,
                }));
            }
            let applied: Vec<i64> = kept.iter().map(|o| o.id).collect();
            let mut writer =
                repo_state.writer(&mut conn, request_context().named(revert_name_of(&locked)))?;
            crate::revert::apply(&mut writer, &kept)?;
            let label_id = writer.label_id();
            let effects = writer.effects();
            slowlog::timed("commit", || writer.commit())?;
            repo_state.settle(&conn, &effects)?;
            let new_head = crate::store::Log::head(&*conn)?;
            Ok(json!({
                "label": label_id,
                "head": new_head,
                "reverted_operations": applied,
                "skipped_operations": skipped,
            }))
        })();

        // The lock is released by a *successful* commit only. A refused one
        // leaves it standing: the client may have moved files already, and it
        // is the one that knows whether to retry with a corrected `apply` list
        // or to abort.
        let value = result?;
        *repo_state.revert_lock.lock_recover() = None;
        crate::executor::flush_pending(repo_state)?;
        Ok(Json(value))
    })
    .await
}

/// `POST /revert/abort`: releases the lock and replays the watcher buffer,
/// writing nothing.
pub(super) async fn revert_abort(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        {
            let mut guard = repo_state.revert_lock.lock_recover();
            if guard.is_none() {
                return Err(ApiError::conflict("no revert in progress"));
            }
            *guard = None;
        }
        crate::executor::flush_pending(repo_state)?;
        let conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        let head = crate::store::Log::head(&*conn)?;
        Ok(Json(json!({"head": head})))
    })
    .await
}
