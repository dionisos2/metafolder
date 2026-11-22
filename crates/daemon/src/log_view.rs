//! Reading the event log back out: the listing `GET /repos/:repo/log` serves,
//! and the JSON shape of one operation (doc "Log endpoints").
//!
//! It lives beside the log rather than inside the route for one reason: the
//! cost of a *bounded* read is an invariant worth testing on its own
//! (doc "Performance testing"). A window of fifty operations must cost the
//! same on a log of five thousand and one of five hundred thousand, and that is
//! a property of this function, not of Axum.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use serde_json::json;
use uuid::Uuid;

use crate::log::OpRow;
use crate::rows;
use crate::store::{Log, LogContextMeta};

/// The operation that created the repository's root — the log's first, written
/// at init —, while the log holds it: what undo never takes back (doc "The
/// undo selection"). `None` once a prune or the conversion to layout 11 took
/// it away: the oldest operation left is then a change like another.
fn root_op(store: &dyn crate::store::Store) -> Result<Option<i64>> {
    let Some(first) = store.first_op()? else { return Ok(None) };
    let Some(op) = store.op(first)? else { return Ok(None) };
    if op.op_type != "create_metarecord" || op.reverts_op_id.is_some() {
        return Ok(None);
    }
    let is_root = store.rows(op.entity_uuid)?.iter().any(|row| {
        row.name == "mfr_path"
            && matches!(&row.value, metafolder_core::metarecord::Value::TreeRef { parent: None, name } if name.as_bytes().is_empty())
    });
    Ok(is_root.then_some(first))
}

/// One log listing request.
#[derive(Debug, Clone, Default)]
pub struct LogQuery {
    /// Cap on the number of *operations* returned (the most recent ones).
    pub limit: Option<usize>,
    /// Cap on the number of *log contexts* returned, counted by the operations'
    /// innermost ones — whole ones, unless `limit` cuts the oldest (then
    /// marked `partial`).
    pub log_contexts: Option<usize>,
    pub entity: Option<Uuid>,
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub include_snapshots: bool,
}

/// The body of `GET /repos/:repo/log`.
pub fn listing(store: &dyn crate::store::Store, q: &LogQuery) -> Result<serde_json::Value> {
    let log: &dyn Log = store;
    let head = log.head()?;
    let ops = select_ops(log, q, head)?;

    // The log contexts of the operations in hand and their ancestors — never the
    // whole table. A window of fifty operations must not pay for the log
    // behind it (doc "Performance testing").
    let log_contexts = log_contexts_with_ancestors(log, ops.iter().map(|op| op.log_context_id))?;

    // A log context whose oldest operation of its own is not in the window was cut
    // by the operation bound (only the oldest can be: the window keeps the
    // newest). Not under a metarecord filter, which leaves out most of every
    // log context on purpose. One key per log context.
    let mut oldest_shown: HashMap<i64, i64> = HashMap::new();
    for op in ops.iter().filter(|_| q.entity.is_none()) {
        let first = oldest_shown.entry(op.log_context_id).or_insert(op.id);
        *first = (*first).min(op.id);
    }
    let mut partial = HashSet::new();
    for (&log_context, &shown) in &oldest_shown {
        if log.log_context_first_op(log_context)?.is_some_and(|first| first < shown) {
            partial.insert(log_context);
        }
    }

    let mut op_values = Vec::with_capacity(ops.len());
    for op in &ops {
        op_values.push(op_json(log, op, q.include_snapshots)?);
    }
    let mut log_context_values = Vec::with_capacity(log_contexts.len());
    for (id, meta) in &log_contexts {
        let mut value = log_context_json(*id, meta);
        if partial.contains(id) {
            value["partial"] = json!(true);
        }
        log_context_values.push(value);
    }

    // Repository-wide totals, so a client showing a bounded window (the GUI log
    // panel fetches only the most recent `limit` operations) can still report
    // how much log there is. Two counts off the primary keys — not the size of
    // the returned window, and unaffected by `limit`.
    let (total_entries, total_log_contexts) = log.counts()?;

    Ok(json!({
        "head": head,
        // Where undo stops: the creation of the repository's root, while the
        // log holds it (doc "The undo selection").
        "root_op": root_op(store)?,
        "operations": op_values,
        "log_contexts": log_context_values,
        "total_entries": total_entries,
        "total_log_contexts": total_log_contexts,
    }))
}

/// One log context, in the shape every response uses (doc "Log endpoints").
pub fn log_context_json(id: i64, meta: &LogContextMeta) -> serde_json::Value {
    json!({
        "id": id,
        "name": meta.name,
        "description": meta.description,
        "parent": meta.parent,
        "created": meta.created,
    })
}

/// The log contexts `ids` name and all their ancestors, by id — each read once.
pub fn log_contexts_with_ancestors(
    log: &dyn Log,
    ids: impl Iterator<Item = i64>,
) -> Result<std::collections::BTreeMap<i64, LogContextMeta>> {
    let mut out = std::collections::BTreeMap::new();
    let mut todo: Vec<i64> = ids.collect::<HashSet<_>>().into_iter().collect();
    while !todo.is_empty() {
        let wanted: Vec<i64> = todo.drain(..).filter(|id| !out.contains_key(id)).collect();
        for (id, meta) in log.log_contexts(&wanted)? {
            if let Some(parent) = meta.parent {
                if !out.contains_key(&parent) {
                    todo.push(parent);
                }
            }
            out.insert(id, meta);
        }
    }
    Ok(out)
}

/// The operations the request selects: the log, filtered, and bounded to what
/// the caller asked for — oldest first.
///
/// A bound is a bound on what is *returned*, not a promise to stop looking: a
/// request filtered on one metarecord reads further back until it has what it
/// asked for, or reaches the oldest operation. The read grows geometrically, so
/// finding nothing costs one full read and finding it quickly costs almost
/// nothing — the common case, a window of the most recent operations, is one
/// bounded read (doc "Log endpoints").
fn select_ops(log: &dyn Log, q: &LogQuery, head: Option<i64>) -> Result<Vec<OpRow>> {
    let Some(head) = head else { return Ok(vec![]) };
    let Some(initial) = walk_budget(q) else {
        // Unbounded: read the whole log, filter it, and answer.
        let mut ops = log.all_ops()?;
        filter_ops(q, &mut ops);
        return Ok(ops);
    };
    let mut budget = initial;
    loop {
        let mut ops = log.ops_back(head, Some(budget))?;
        let exhausted = ops.len() < budget;
        ops.reverse(); // oldest first
        filter_ops(q, &mut ops);
        if exhausted || satisfied(q, &ops) {
            bound_ops(q, &mut ops);
            return Ok(ops);
        }
        budget = budget.saturating_mul(GROWTH);
    }
}

/// Drops the operations the request does not select: another metarecord's, or
/// one written outside the time window.
fn filter_ops(q: &LogQuery, ops: &mut Vec<OpRow>) {
    if let Some(filter) = q.entity {
        ops.retain(|op| op.entity_uuid == filter);
    }
    ops.retain(|op| {
        q.since.is_none_or(|s| op.timestamp >= s) && q.until.is_none_or(|u| op.timestamp <= u)
    });
}

/// Whether the walk so far already holds everything the request will return:
/// either bound reached is enough, the answer being trimmed by both.
///
/// A log context bound alone needs one log context more than it returns: the oldest one
/// in the walk may be cut in half, and without an operation bound a log context is
/// shown whole. With one, the operations are what is bounded — a reconcile's
/// log context holds tens of thousands — and the log context it cuts is marked `partial`.
fn satisfied(q: &LogQuery, ops: &[OpRow]) -> bool {
    let by_limit = q.limit.is_some_and(|l| ops.len() >= l);
    let by_log_contexts = q.log_contexts.is_some_and(|r| distinct_log_contexts(ops) > r);
    by_limit || by_log_contexts
}

fn distinct_log_contexts(ops: &[OpRow]) -> usize {
    ops.iter().map(|op| op.log_context_id).collect::<HashSet<_>>().len()
}

/// Trims the selection to what was asked for: the most recent `limit`
/// operations, then the most recent `log_contexts` log contexts — whole ones, but for
/// the oldest, which `limit` may have cut (and `listing` marks).
fn bound_ops(q: &LogQuery, ops: &mut Vec<OpRow>) {
    if let Some(limit) = q.limit {
        if ops.len() > limit {
            ops.drain(..ops.len() - limit);
        }
    }
    if let Some(max) = q.log_contexts {
        let kept = newest_log_contexts(ops, max);
        ops.retain(|op| kept.contains(&op.log_context_id));
    }
}

/// The ids of the `max` most recent log contexts present in `ops`.
fn newest_log_contexts(ops: &[OpRow], max: usize) -> HashSet<i64> {
    let mut kept = HashSet::new();
    for op in ops.iter().rev() {
        if !kept.contains(&op.log_context_id) {
            if kept.len() == max {
                break;
            }
            kept.insert(op.log_context_id);
        }
    }
    kept
}

/// How much the backward read may take on its first attempt, or `None` when the
/// request is unbounded.
fn walk_budget(q: &LogQuery) -> Option<usize> {
    match (q.limit, q.log_contexts) {
        (None, None) => None,
        (Some(l), None) => Some(l.max(1)),
        (None, Some(r)) => Some(r.saturating_mul(OPS_PER_LABEL).max(1)),
        (Some(l), Some(r)) => Some(l.max(r.saturating_mul(OPS_PER_LABEL)).max(1)),
    }
}

/// Operations assumed per log context when a request is bounded by log contexts. A log context
/// that holds more (a reconcile writes thousands) simply costs another round
/// of the walk.
const OPS_PER_LABEL: usize = 8;

/// How much wider each round of the walk is than the one before.
const GROWTH: usize = 8;

/// One operation, in the shape every `/log` response uses (doc "Log endpoints").
pub fn op_json(log: &dyn Log, op: &OpRow, include_snapshots: bool) -> Result<serde_json::Value> {
    let mut value = json!({
        "id": op.id,
        "log_context_id": op.log_context_id,
        "timestamp": op.timestamp,
        "origin": op.origin,
        "op_type": op.op_type,
        "entity_uuid": op.entity_uuid.as_simple().to_string(),
        "field_name": op.field_name,
        "reverts_op_id": op.reverts_op_id,
    });
    if include_snapshots {
        value["snapshots_before"] = snapshots_json(log, op.id)?;
    }
    Ok(value)
}

/// Snapshot rows in their raw column form (doc "Log endpoints" examples).
pub fn snapshots_json(log: &dyn Log, op_id: i64) -> Result<serde_json::Value> {
    let blob_hex = |b: Vec<u8>| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    let mut out = Vec::new();
    for row in log.snapshots(op_id)? {
        // Raw column form (doc "Log endpoints" examples), null columns omitted.
        let encoded = rows::encode_value(&row.value);
        let mut snapshot = json!({
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

// ── Log context summaries (doc "Log context summaries") ──────────────────────────────────

/// Which log contexts `GET /repos/:repo/log/contexts` lists.
#[derive(Debug, Clone, Copy)]
pub enum Under {
    /// The units of work: the log contexts that are nested in none.
    Roots,
    /// The log contexts nested directly in one.
    LogContext(i64),
}

/// One request for log context summaries.
#[derive(Debug, Clone, Copy)]
pub struct SummaryQuery {
    pub under: Under,
    /// Only the log contexts older than this one: where the last page stopped.
    pub before: Option<i64>,
    pub limit: usize,
}

/// The most summaries one request returns.
pub const SUMMARIES_MAX: usize = 500;

/// Whether a log context is standing or has been undone, whole or in part.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Undone {
    No,
    Partly,
    Fully,
}

/// Whether the operations the watcher wrote on the filesystem's behalf: its
/// origin, or the types it records when no origin says.
fn is_watcher_op(op: &OpRow) -> bool {
    op.origin.as_deref() == Some(crate::log::ORIGIN_WATCHER)
        || (op.reverts_op_id.is_none()
            && matches!(op.op_type.as_str(), "file_deleted" | "file_moved" | "file_modified"))
}

/// Whether `log_context` stands, from what the log contexts say of each other and nothing
/// read of the operations: a revert that undid it whole (or one that undid the
/// log context it is nested in whole) makes it undone unless that revert is itself
/// undone whole — a redo — and one that undid some of its operations makes it
/// partly undone. Not exact when partial reverts overlap: it says `partly`
/// where it could say `fully`, never the other way.
fn undone_state(
    log: &dyn Log,
    log_context: i64,
    memo: &mut HashMap<i64, Undone>,
) -> Result<Undone> {
    if let Some(known) = memo.get(&log_context) {
        return Ok(*known);
    }
    // A cycle (a revert that names itself, a corrupted store) stands.
    memo.insert(log_context, Undone::No);
    let mut chain = vec![(log_context, true)];
    let mut next = log.log_contexts(&[log_context])?.remove(&log_context).and_then(|m| m.parent);
    while let Some(parent) = next {
        if chain.len() > crate::log::MAX_TREE_DEPTH {
            break;
        }
        chain.push((parent, false));
        next = log.log_contexts(&[parent])?.remove(&parent).and_then(|m| m.parent);
    }
    let mut state = Undone::No;
    for (of, own) in chain {
        for (reverter, whole) in log.log_context_reverted_by(of)? {
            if undone_state(log, reverter, memo)? == Undone::Fully {
                continue;
            }
            if whole {
                state = Undone::Fully;
            } else if own && state == Undone::No {
                state = Undone::Partly;
            }
        }
    }
    memo.insert(log_context, state);
    Ok(state)
}

/// Whether anything is written in `log_context` or under it: an empty context (a
/// command that wrote nothing, an answer left empty) is no unit of work, and
/// is not listed — it stays until a reindex (doc "Log contexts"). Stops at the
/// first operation found, the log context's own almost always.
fn holds_anything(log: &dyn Log, log_context: i64) -> Result<bool> {
    let mut todo = vec![log_context];
    let mut seen = 0;
    while let Some(id) = todo.pop() {
        if log.log_context_first_op(id)?.is_some() {
            return Ok(true);
        }
        seen += 1;
        if seen > 100_000 {
            // A corrupted store's cycle: listed rather than looped over.
            return Ok(true);
        }
        todo.extend(log.log_context_children(id)?);
    }
    Ok(false)
}

/// The body of `GET /repos/:repo/log/contexts`: log contexts, newest first, each with
/// what the log knows of it without reading an operation — how many it holds,
/// its first and its last, who wrote it, what it reverts and is reverted by.
/// The cost is a few keys per log context, whatever the number of operations in it.
pub fn log_context_summaries(log: &dyn Log, q: &SummaryQuery) -> Result<serde_json::Value> {
    let limit = q.limit.clamp(1, SUMMARIES_MAX);
    let mut ids: Vec<i64> = Vec::new();
    let mut more = false;
    match q.under {
        Under::LogContext(parent) => {
            let mut kids = log.log_context_children(parent)?;
            kids.reverse();
            kids.retain(|id| q.before.is_none_or(|b| *id < b));
            for id in kids {
                if !holds_anything(log, id)? {
                    continue;
                }
                if ids.len() == limit {
                    more = true;
                    break;
                }
                ids.push(id);
            }
        }
        Under::Roots => {
            const CHUNK: usize = 256;
            let mut before = q.before;
            'scan: loop {
                let chunk = log.log_context_ids_before(before, CHUNK)?;
                if chunk.is_empty() {
                    break;
                }
                before = chunk.last().copied();
                let metas = log.log_contexts(&chunk)?;
                for id in &chunk {
                    if metas.get(id).is_some_and(|m| m.parent.is_none())
                        && holds_anything(log, *id)?
                    {
                        if ids.len() == limit {
                            more = true;
                            break 'scan;
                        }
                        ids.push(*id);
                    }
                }
                if chunk.len() < CHUNK {
                    break;
                }
            }
        }
    }
    let metas = log.log_contexts(&ids)?;
    let head = log.head()?;
    let head_log_context = match head {
        Some(h) => log.op(h)?.map(|op| op.log_context_id),
        None => None,
    };
    let mut memo = HashMap::new();
    let mut log_contexts = Vec::with_capacity(ids.len());
    for id in &ids {
        let Some(meta) = metas.get(id) else { continue };
        let first = log.log_context_first_op(*id)?;
        let first_op = match first {
            Some(f) => log.op(f)?,
            None => None,
        };
        let undone = match undone_state(log, *id, &mut memo)? {
            Undone::No => serde_json::Value::Null,
            Undone::Partly => json!("partly"),
            Undone::Fully => json!("fully"),
        };
        let marks = |pairs: Vec<(i64, bool)>| -> serde_json::Value {
            pairs
                .into_iter()
                .map(|(log_context, whole)| json!({"log_context": log_context, "whole": whole}))
                .collect()
        };
        let mut value = log_context_json(*id, meta);
        value["operations"] = json!(log.log_context_op_count(*id)?);
        let last = log.log_context_last_op(*id)?;
        value["first_op"] = json!(first);
        value["last_op"] = json!(last);
        // When it was last written in: a context is older than most of what is
        // written under it.
        value["updated"] = json!(match last {
            Some(l) => log.op(l)?.map(|o| o.timestamp),
            None => None,
        });
        value["origin"] = json!(first_op.as_ref().and_then(|o| o.origin.clone()));
        value["watcher"] = json!(first_op.as_ref().is_some_and(is_watcher_op));
        value["children"] = json!(log.log_context_children(*id)?.len());
        value["reverts"] = marks(log.log_context_reverts(*id)?);
        value["reverted_by"] = marks(log.log_context_reverted_by(*id)?);
        value["undone"] = undone;
        value["is_head"] = json!(head_log_context == Some(*id));
        log_contexts.push(value);
    }
    Ok(json!({
        "head": head,
        "head_log_context": head_log_context,
        "log_contexts": log_contexts,
        "next_before": if more { ids.last().copied() } else { None },
        "total_log_contexts": log.counts()?.1,
    }))
}
