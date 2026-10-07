//! Revert (doc "Revert"): the plan, the one-shot revert and the
//! coordinated one.

use super::*;
use std::collections::HashSet;

/// The set of operations to revert: a revision's, or an explicit list.
#[derive(Deserialize)]
pub(super) struct RevertTarget {
    #[serde(default)]
    rev_id: Option<serde_json::Value>,
    #[serde(default)]
    op_ids: Option<Vec<i64>>,
}

#[derive(Deserialize)]
pub(super) struct RevertBody {
    target: RevertTarget,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    with_dependents: bool,
    #[serde(default)]
    skip_filesystem: bool,
}

#[derive(Deserialize)]
pub(super) struct RevertPlanParams {
    #[serde(default)]
    target_rev_id: Option<String>,
    #[serde(default)]
    target_op_ids: Option<String>,
    #[serde(default)]
    with_dependents: bool,
}

/// Resolves a revert target to its operations, oldest first. All of them must
/// lie on HEAD's ancestry: an operation on a branch a past rollback abandoned
/// is not part of the current history, so there is nothing there to undo.
fn resolve_revert_target(
    conn: &dyn crate::store::Store,
    head: Option<i64>,
    target: &RevertTarget,
) -> Result<Vec<crate::log::OpRow>, ApiError> {
    let ops = match (&target.rev_id, &target.op_ids) {
        (Some(rev), None) => {
            let rev_id = match rev {
                serde_json::Value::String(s) if s == "head" => {
                    let head = head.ok_or_else(|| ApiError::not_found("the log is empty"))?;
                    conn.op(head)?
                        .ok_or_else(|| ApiError::not_found("HEAD names no operation"))?
                        .rev_id
                }
                serde_json::Value::Number(n) => {
                    n.as_i64().ok_or_else(|| ApiError::bad_request("rev_id must be an integer"))?
                }
                _ => return Err(ApiError::bad_request("rev_id must be a number or \"head\"")),
            };
            let ops = crate::revert::revision_ops(conn, rev_id)?;
            if ops.is_empty() {
                return Err(ApiError::not_found(format!(
                    "revision {rev_id} does not exist, or was pruned or trimmed away"
                )));
            }
            ops
        }
        (None, Some(ids)) => {
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
        _ => return Err(ApiError::bad_request("the target needs exactly one of rev_id or op_ids")),
    };
    if let Some(head) = head {
        let ancestry: std::collections::HashSet<i64> = conn.ancestry(head)?.into_iter().collect();
        if let Some(off) = ops.iter().find(|o| !ancestry.contains(&o.id)) {
            return Err(ApiError::bad_request(format!(
                "operation {} is not on HEAD's ancestry: it sits on a branch a past rollback \
                 abandoned, so there is nothing there to undo",
                off.id
            )));
        }
    }
    Ok(ops)
}

fn op_brief(op: &crate::log::OpRow) -> serde_json::Value {
    json!({
        "op_id": op.id,
        "rev_id": op.rev_id,
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
    analysis: &crate::revert::Analysis,
    with_dependents: bool,
    mut fs: Option<(&crate::tree_cache::TreeCache, &std::path::Path)>,
) -> Result<serde_json::Value, ApiError> {
    let effective = analysis.effective(with_dependents);
    let requested: std::collections::HashSet<i64> =
        analysis.requested.iter().map(|o| o.id).collect();
    let mut operations = Vec::with_capacity(effective.len());
    let mut requires_lock = false;
    for op in &effective {
        let action = crate::revert::fs_action(conn, op)?;
        requires_lock |= action.is_some();
        let restores = snapshots_json(conn, op.id, 0)?;
        let blocked_by = analysis
            .blocked
            .iter()
            .find(|b| crate::revert::Cell::of(&b.op).intersects(&crate::revert::Cell::of(op)))
            .filter(|_| !with_dependents && requested.contains(&op.id))
            .map(|b| {
                let mut v = op_brief(&b.op);
                v["timestamp"] = json!(b.timestamp);
                v
            });
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
            "origin": if requested.contains(&op.id) { "requested" } else { "dependent" },
            "writes": crate::revert::written_as(op).as_str(),
            "restores": restores,
            "filesystem": filesystem,
            "blocked_by": blocked_by,
        });
        if matches!(action, Some(crate::revert::FsAction::RestoreContent)) {
            // The trash correlation key, as on a rollback step: the version the
            // record held before the whole revision, which is the only one
            // anything outside it observed (doc "Trash, undo and redo").
            if let Some(v) = op.entity_version_before {
                entry["entity_version_before"] = json!(v);
            }
            if let Some(v) = conn.version_before_revision(op.rev_id, op.entity_uuid)? {
                entry["entity_version_before_revision"] = json!(v);
            }
        }
        operations.push(entry);
    }
    let blocked: Vec<serde_json::Value> = if with_dependents {
        vec![]
    } else {
        analysis
            .blocked
            .iter()
            .map(|b| {
                let mut v = op_brief(&b.op);
                v["timestamp"] = json!(b.timestamp);
                v
            })
            .collect()
    };
    Ok(json!({
        "revertable": with_dependents || analysis.revertable(),
        "requires_lock": requires_lock,
        "operations": operations,
        "blocked": blocked,
        "dependents": analysis.dependents.iter().map(op_brief).collect::<Vec<_>>(),
    }))
}

pub(super) async fn revert_plan(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    Query(params): Query<RevertPlanParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let target = RevertTarget {
        // `head` is a legal rev_id here as it is in the body, so a plan and the
        // revert it describes take the same target spelling.
        rev_id: params.target_rev_id.as_ref().map(|s| match s.parse::<i64>() {
            Ok(n) => serde_json::Value::from(n),
            Err(_) => serde_json::Value::String(s.clone()),
        }),
        op_ids: params
            .target_op_ids
            .as_ref()
            .map(|s| s.split(',').filter_map(|p| p.trim().parse::<i64>().ok()).collect::<Vec<_>>()),
    };
    let with_dependents = params.with_dependents;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        let head = crate::store::Log::head(&*conn)?;
        let ops = resolve_revert_target(&conn, head, &target)?;
        let analysis = crate::revert::analyse(&*conn, head, ops)?;
        Ok(Json(revert_plan_json(&conn, &analysis, with_dependents, None)?))
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
        let ops = resolve_revert_target(&conn, head, &body.target)?;
        // The check runs here, inside the transaction that writes the revert,
        // so what is written was checked against the state it is written into.
        let analysis = crate::revert::analyse(&*conn, head, ops)?;
        if !body.with_dependents && !analysis.revertable() {
            let plan = revert_plan_json(&conn, &analysis, false, None)?;
            return Err(ApiError::conflict(format!(
                "the revert is blocked by {} later operation(s) on the same cells; \
                 pass with_dependents to revert those too",
                analysis.blocked.len()
            ))
            .with_field("blocked", plan["blocked"].clone()));
        }

        let mut effective = analysis.effective(body.with_dependents);
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
            // Nothing left to apply: an empty revision would be a log entry
            // claiming a change that did not happen.
            return Ok(Json(json!({
                "revision": serde_json::Value::Null,
                "head": head,
                "reverted_operations": [],
                "skipped_operations": skipped,
            })));
        }

        let mut writer = repo_state.writer(&mut conn, body.label.clone())?;
        let applied: Vec<i64> = effective.iter().map(|o| o.id).collect();
        crate::revert::apply(&mut writer, &effective)?;
        let rev_id = writer.rev_id();
        let effects = writer.effects();
        slowlog::timed("commit", || writer.commit())?;
        repo_state.settle(&conn, &effects)?;
        let new_head = crate::store::Log::head(&*conn)?;
        Ok(Json(json!({
            "revision": rev_id,
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
    #[serde(default)]
    with_dependents: bool,
}

/// `POST /revert/start`: enters the lock and returns the plan, with the paths
/// each `move` action needs. A blocked target is refused and the lock is *not*
/// entered.
pub(super) async fn revert_start(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<RevertStartBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        if repo_state.is_rollback_locked() {
            return Err(ApiError::conflict("a coordinated operation is already in progress"));
        }
        let conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        let head = crate::store::Log::head(&*conn)?;
        let ops = resolve_revert_target(&conn, head, &body.target)?;
        let analysis = crate::revert::analyse(&*conn, head, ops)?;
        if !body.with_dependents && !analysis.revertable() {
            let plan = revert_plan_json(&conn, &analysis, false, None)?;
            return Err(ApiError::conflict(format!(
                "the revert is blocked by {} later operation(s) on the same cells; \
                 pass with_dependents to revert those too",
                analysis.blocked.len()
            ))
            .with_field("blocked", plan["blocked"].clone()));
        }
        let effective = analysis.effective(body.with_dependents);
        let plan = {
            let cache = repo_state.tree();
            revert_plan_json(
                &conn,
                &analysis,
                body.with_dependents,
                Some((&cache, &repo_state.config.root)),
            )?
        };
        drop(conn);
        *repo_state.rollback_lock.lock_recover() =
            Some(RollbackLock::Revert { ops: effective.iter().map(|o| o.id).collect() });
        Ok(Json(plan))
    })
    .await
}

#[derive(Deserialize)]
pub(super) struct RevertCommitBody {
    #[serde(default)]
    apply: Vec<i64>,
    #[serde(default)]
    label: Option<String>,
}

/// `POST /revert/commit`: writes the revert as one revision, releases the lock
/// and replays the watcher buffer.
pub(super) async fn revert_commit(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<RevertCommitBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let locked: Vec<i64> = match repo_state.rollback_lock.lock_recover().as_ref() {
            Some(RollbackLock::Revert { ops }) => ops.clone(),
            Some(RollbackLock::Navigate { .. }) => {
                return Err(ApiError::conflict(
                    "a rollback navigation is in progress, not a revert",
                ))
            }
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
            // Re-run the check inside the writing transaction: under the lock it
            // cannot have changed — HEAD stands still — but the guarantee is the
            // daemon's, not a client convention.
            let analysis = crate::revert::analyse(&conn, head, effective.clone())?;
            if !analysis.revertable() {
                let plan = revert_plan_json(&conn, &analysis, false, None)?;
                return Err(ApiError::conflict("the revert became blocked")
                    .with_field("blocked", plan["blocked"].clone()));
            }
            let (kept, skipped) = narrow_to_applied(effective, &body.apply)?;
            if kept.is_empty() {
                return Ok(json!({
                    "revision": serde_json::Value::Null,
                    "head": head,
                    "reverted_operations": [],
                    "skipped_operations": skipped,
                }));
            }
            let applied: Vec<i64> = kept.iter().map(|o| o.id).collect();
            let mut writer = repo_state.writer(&mut conn, body.label.clone())?;
            crate::revert::apply(&mut writer, &kept)?;
            let rev_id = writer.rev_id();
            let effects = writer.effects();
            slowlog::timed("commit", || writer.commit())?;
            repo_state.settle(&conn, &effects)?;
            let new_head = crate::store::Log::head(&*conn)?;
            Ok(json!({
                "revision": rev_id,
                "head": new_head,
                "reverted_operations": applied,
                "skipped_operations": skipped,
            }))
        })();

        // The lock is released by a *successful* commit only. A refused one
        // leaves it standing: the client may have moved files already, and it
        // is the one that knows whether to retry with a corrected `apply` list
        // or to abort — exactly as a rollback step leaves the lock to `abort`.
        let value = result?;
        *repo_state.rollback_lock.lock_recover() = None;
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
            let mut guard = repo_state.rollback_lock.lock_recover();
            match guard.as_ref() {
                Some(RollbackLock::Revert { .. }) => {}
                Some(RollbackLock::Navigate { .. }) => {
                    return Err(ApiError::conflict(
                        "a rollback navigation is in progress; use rollback/abort",
                    ))
                }
                None => return Err(ApiError::conflict("no revert in progress")),
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

#[derive(Deserialize, Default)]
pub(super) struct StepBody {
    #[serde(default)]
    skip: bool,
}

pub(super) async fn rollback_step(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<StepBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // The body is optional: `{}` and an empty body both mean "apply inverse".
    let skip = payload.map(|Json(b)| b.skip).unwrap_or(false);
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        // The plan is taken out for the step and put back after it, so the
        // rollback lock is not held across the connection's. The lock stays
        // entered meanwhile, with an empty plan: a navigation's plan is empty
        // only while one of its steps runs (a finished one leaves the lock).
        let mut plan = {
            let mut guard = repo_state.rollback_lock.lock_recover();
            match guard.as_mut() {
                Some(RollbackLock::Navigate { plan }) if plan.is_empty() => {
                    return Err(ApiError::conflict("a rollback step is already running"))
                }
                Some(RollbackLock::Navigate { plan }) => std::mem::take(plan),
                Some(RollbackLock::Revert { .. }) => {
                    return Err(ApiError::conflict(
                        "a coordinated revert is in progress; finish it with revert/commit \
                         or revert/abort",
                    ))
                }
                None => {
                    return Err(ApiError::conflict(
                        "no rollback navigation in progress; call start first",
                    ))
                }
            }
        };

        // An error leaves the plan where it was — a failed step applied
        // nothing, and a failed description after it applied its operation and
        // took it off the plan — so the client may retry or abort.
        let put_back = |plan: crate::log::NavPlan| {
            *repo_state.rollback_lock.lock_recover() = Some(RollbackLock::Navigate { plan });
        };
        let done = {
            let mut conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
            if let Err(e) = plan.step(&mut *conn, skip) {
                put_back(plan);
                return Err(e.into());
            }
            if let Some((id, dir)) = plan.next() {
                let described = crate::store::Log::op(&*conn, id)
                    .map_err(ApiError::from)
                    .and_then(|op| {
                        op.ok_or_else(|| ApiError::internal("operation vanished during navigation"))
                    })
                    .and_then(|op| {
                        let cache = repo_state.tree();
                        action_op_json(&conn, &cache, &repo_state.config.root, &op, dir)
                    });
                let remaining = plan.len() - 1;
                put_back(plan);
                return Ok(Json(json!({"op": described?, "remaining": remaining})));
            }
            true
        };

        if done {
            // HEAD reached the target: release the lock, replay the buffer.
            *repo_state.rollback_lock.lock_recover() = None;
            crate::executor::flush_pending(repo_state)?;
            // The navigation restored (or took away) `mf_watch`/`mf_ignore`
            // rows; the watch set follows the state it landed on, exactly as
            // after a write that touched those fields (doc "Navigation"). Not per step: the states
            // in between need
            // not be consistent even when the final one is.
            if plan.touches_watch() {
                let conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
                let _phase = slowlog::phase("settle.watches");
                repo_state.refresh_watches(&conn);
            }
        }
        Ok(Json(json!({"op": null, "remaining": 0})))
    })
    .await
}

pub(super) async fn rollback_abort(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let touches_watch = {
            let mut guard = repo_state.rollback_lock.lock_recover();
            let touches_watch = match guard.as_ref() {
                None => return Err(ApiError::conflict("no rollback navigation in progress")),
                // A plan taken out by a step still running says nothing yet:
                // that step may move a rule.
                Some(RollbackLock::Navigate { plan }) => plan.is_empty() || plan.touches_watch(),
                Some(RollbackLock::Revert { .. }) => true,
            };
            *guard = None;
            touches_watch
        };
        crate::executor::flush_pending(repo_state)?;
        let conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        // An abort keeps the state it stopped at, mid-navigation: the watch set
        // follows that state too (doc "Navigation"), when a step moved a rule.
        if touches_watch {
            let _phase = slowlog::phase("settle.watches");
            repo_state.refresh_watches(&conn);
        }
        let head = crate::store::Log::head(&*conn)?;
        Ok(Json(json!({"head": head})))
    })
    .await
}
