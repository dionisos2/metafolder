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
use crate::store::{LabelMeta, Log};

/// One log listing request.
#[derive(Debug, Clone, Default)]
pub struct LogQuery {
    /// Cap on the number of *operations* returned (the most recent ones).
    pub limit: Option<usize>,
    /// Cap on the number of *labels* returned, counted by the operations'
    /// innermost ones — whole ones, unless `limit` cuts the oldest (then
    /// marked `partial`).
    pub labels: Option<usize>,
    pub entity: Option<Uuid>,
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub include_snapshots: bool,
}

/// The body of `GET /repos/:repo/log`.
pub fn listing(log: &dyn Log, q: &LogQuery) -> Result<serde_json::Value> {
    let head = log.head()?;
    let ops = select_ops(log, q, head)?;

    // The labels of the operations in hand and their ancestors — never the
    // whole table. A window of fifty operations must not pay for the log
    // behind it (doc "Performance testing").
    let labels = labels_with_ancestors(log, ops.iter().map(|op| op.label_id))?;

    // A label whose oldest operation of its own is not in the window was cut
    // by the operation bound (only the oldest can be: the window keeps the
    // newest). Not under a metarecord filter, which leaves out most of every
    // label on purpose. One key per label.
    let mut oldest_shown: HashMap<i64, i64> = HashMap::new();
    for op in ops.iter().filter(|_| q.entity.is_none()) {
        let first = oldest_shown.entry(op.label_id).or_insert(op.id);
        *first = (*first).min(op.id);
    }
    let mut partial = HashSet::new();
    for (&label, &shown) in &oldest_shown {
        if log.label_first_op(label)?.is_some_and(|first| first < shown) {
            partial.insert(label);
        }
    }

    let mut op_values = Vec::with_capacity(ops.len());
    for op in &ops {
        op_values.push(op_json(log, op, q.include_snapshots)?);
    }
    let mut label_values = Vec::with_capacity(labels.len());
    for (id, meta) in &labels {
        let mut value = label_json(*id, meta);
        if partial.contains(id) {
            value["partial"] = json!(true);
        }
        label_values.push(value);
    }

    // Repository-wide totals, so a client showing a bounded window (the GUI log
    // panel fetches only the most recent `limit` operations) can still report
    // how much log there is. Two counts off the primary keys — not the size of
    // the returned window, and unaffected by `limit`.
    let (total_operations, total_labels) = log.counts()?;

    Ok(json!({
        "head": head,
        // Where undo stops: the root metarecord written at init, or the oldest
        // operation a prune left (doc "The undo selection").
        "first": log.first_op()?,
        "operations": op_values,
        "labels": label_values,
        "total_operations": total_operations,
        "total_labels": total_labels,
    }))
}

/// One label, in the shape every response uses (doc "Log endpoints").
pub fn label_json(id: i64, meta: &LabelMeta) -> serde_json::Value {
    json!({
        "id": id,
        "name": meta.name,
        "description": meta.description,
        "parent": meta.parent,
        "created": meta.created,
    })
}

/// The labels `ids` name and all their ancestors, by id — each read once.
pub fn labels_with_ancestors(
    log: &dyn Log,
    ids: impl Iterator<Item = i64>,
) -> Result<std::collections::BTreeMap<i64, LabelMeta>> {
    let mut out = std::collections::BTreeMap::new();
    let mut todo: Vec<i64> = ids.collect::<HashSet<_>>().into_iter().collect();
    while !todo.is_empty() {
        let wanted: Vec<i64> = todo.drain(..).filter(|id| !out.contains_key(id)).collect();
        for (id, meta) in log.labels(&wanted)? {
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
/// A label bound alone needs one label more than it returns: the oldest one
/// in the walk may be cut in half, and without an operation bound a label is
/// shown whole. With one, the operations are what is bounded — a reconcile's
/// label holds tens of thousands — and the label it cuts is marked `partial`.
fn satisfied(q: &LogQuery, ops: &[OpRow]) -> bool {
    let by_limit = q.limit.is_some_and(|l| ops.len() >= l);
    let by_labels = q.labels.is_some_and(|r| distinct_labels(ops) > r);
    by_limit || by_labels
}

fn distinct_labels(ops: &[OpRow]) -> usize {
    ops.iter().map(|op| op.label_id).collect::<HashSet<_>>().len()
}

/// Trims the selection to what was asked for: the most recent `limit`
/// operations, then the most recent `labels` labels — whole ones, but for
/// the oldest, which `limit` may have cut (and `listing` marks).
fn bound_ops(q: &LogQuery, ops: &mut Vec<OpRow>) {
    if let Some(limit) = q.limit {
        if ops.len() > limit {
            ops.drain(..ops.len() - limit);
        }
    }
    if let Some(max) = q.labels {
        let kept = newest_labels(ops, max);
        ops.retain(|op| kept.contains(&op.label_id));
    }
}

/// The ids of the `max` most recent labels present in `ops`.
fn newest_labels(ops: &[OpRow], max: usize) -> HashSet<i64> {
    let mut kept = HashSet::new();
    for op in ops.iter().rev() {
        if !kept.contains(&op.label_id) {
            if kept.len() == max {
                break;
            }
            kept.insert(op.label_id);
        }
    }
    kept
}

/// How much the backward read may take on its first attempt, or `None` when the
/// request is unbounded.
fn walk_budget(q: &LogQuery) -> Option<usize> {
    match (q.limit, q.labels) {
        (None, None) => None,
        (Some(l), None) => Some(l.max(1)),
        (None, Some(r)) => Some(r.saturating_mul(OPS_PER_LABEL).max(1)),
        (Some(l), Some(r)) => Some(l.max(r.saturating_mul(OPS_PER_LABEL)).max(1)),
    }
}

/// Operations assumed per label when a request is bounded by labels. A label
/// that holds more (a reconcile writes thousands) simply costs another round
/// of the walk.
const OPS_PER_LABEL: usize = 8;

/// How much wider each round of the walk is than the one before.
const GROWTH: usize = 8;

/// One operation, in the shape every `/log` response uses (doc "Log endpoints").
pub fn op_json(log: &dyn Log, op: &OpRow, include_snapshots: bool) -> Result<serde_json::Value> {
    let mut value = json!({
        "id": op.id,
        "label_id": op.label_id,
        "timestamp": op.timestamp,
        "origin": op.origin,
        "op_type": op.op_type,
        "entity_uuid": op.entity_uuid.as_simple().to_string(),
        "field_name": op.field_name,
        "reverts_op_id": op.reverts_op_id,
    });
    if include_snapshots {
        value["snapshots_before"] = snapshots_json(log, op.id, false)?;
        value["snapshots_after"] = snapshots_json(log, op.id, true)?;
    }
    Ok(value)
}

/// Snapshot rows in their raw column form (doc "Log endpoints" examples).
pub fn snapshots_json(log: &dyn Log, op_id: i64, after: bool) -> Result<serde_json::Value> {
    let blob_hex = |b: Vec<u8>| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    let mut out = Vec::new();
    for row in log.snapshots(op_id, after)? {
        // Raw column form (doc "Log endpoints" examples), null columns omitted.
        let encoded = rows::encode_value(&row.value);
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
