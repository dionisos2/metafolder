//! Reading the event log back out: the listing `GET /repos/:repo/log` serves,
//! and the JSON shape of one operation (spec-event-log).
//!
//! It lives beside the log rather than inside the route for one reason: the
//! cost of a *bounded* read is an invariant worth testing on its own
//! (spec-perf "Cost assertions"). A window of fifty operations must cost the
//! same on a log of five thousand and one of five hundred thousand, and that is
//! a property of this function, not of Axum.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use serde_json::json;
use uuid::Uuid;

use crate::db;
use crate::log::{self, OpRow};

/// Which line through the log to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The ancestry of HEAD, oldest first.
    Linear,
    /// The ancestry plus HEAD's forward continuation (the redo future stays
    /// visible), oldest first.
    Active,
    /// Every operation, including divergent branches, in creation order.
    Tree,
}

impl Mode {
    /// The query-parameter spelling, or `None` for an unknown one (a 400).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "linear" => Some(Mode::Linear),
            "active" => Some(Mode::Active),
            "tree" => Some(Mode::Tree),
            _ => None,
        }
    }
}

/// One log listing request.
#[derive(Debug, Clone)]
pub struct LogQuery {
    pub mode: Mode,
    /// Cap on the number of *operations* returned (the most recent ones).
    pub limit: Option<usize>,
    /// Cap on the number of *revisions* returned.
    pub revisions: Option<usize>,
    pub entity: Option<Uuid>,
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub include_snapshots: bool,
}

impl Default for LogQuery {
    fn default() -> Self {
        Self {
            mode: Mode::Linear,
            limit: None,
            revisions: None,
            entity: None,
            since: None,
            until: None,
            include_snapshots: false,
        }
    }
}

/// The body of `GET /repos/:repo/log`.
pub fn listing(conn: &rusqlite::Connection, q: &LogQuery) -> Result<serde_json::Value> {
    let head = log::get_head(conn)?;
    let ops = select_ops(conn, q, head)?;

    // The revisions of the operations in hand — never the whole table. A
    // window of fifty operations must not pay for the log behind it
    // (spec-perf "Cost assertions").
    let rev_meta = revision_meta(conn, ops.iter().map(|op| op.rev_id))?;

    let mut op_values = Vec::with_capacity(ops.len());
    let mut seen_revs = HashSet::new();
    let mut revisions = Vec::new();
    for op in &ops {
        op_values.push(op_json(conn, op, q.include_snapshots)?);
        if seen_revs.insert(op.rev_id) {
            if let Some((ts, label, origin)) = rev_meta.get(&op.rev_id) {
                revisions.push(json!({
                    "id": op.rev_id, "timestamp": ts, "label": label, "origin": origin,
                }));
            }
        }
    }

    // Repository-wide totals, so a client showing a bounded window (the GUI log
    // panel fetches only the most recent `limit` operations) can still report
    // how much log there is. Two counts off the primary keys — not the size of
    // the returned window, and unaffected by `limit`/`mode`.
    let total_operations: i64 =
        conn.query_row("SELECT COUNT(*) FROM operation", [], |r| r.get(0))?;
    let total_revisions: i64 = conn.query_row("SELECT COUNT(*) FROM revision", [], |r| r.get(0))?;

    Ok(json!({
        "head": head,
        "operations": op_values,
        "revisions": revisions,
        "total_operations": total_operations,
        "total_revisions": total_revisions,
    }))
}

/// The operations the request selects: the mode's line through the log,
/// filtered, and bounded to what the caller asked for — oldest first.
///
/// A bound is a bound on what is *returned*, not a promise to stop looking: a
/// request filtered on one metarecord walks further back until it has what it
/// asked for, or reaches the root. The walk grows geometrically, so finding
/// nothing costs one full walk and finding it quickly costs almost nothing —
/// the common case, a window of the most recent operations, is one bounded
/// walk (spec-event-log "limit").
fn select_ops(conn: &rusqlite::Connection, q: &LogQuery, head: Option<i64>) -> Result<Vec<OpRow>> {
    let Some(head) = head else {
        return Ok(match q.mode {
            // An empty log has no HEAD; `tree` still answers from the table,
            // which is how a pruned-to-nothing log reads back as empty rather
            // than as an error.
            Mode::Tree => {
                let mut ops = log::all_ops(conn)?;
                filter_ops(conn, q, &mut ops)?;
                bound_ops(q, &mut ops);
                ops
            }
            _ => vec![],
        });
    };

    let Some(initial) = walk_budget(q) else {
        // Unbounded: read the whole line, filter it, and answer.
        let mut ops = whole_line(conn, q.mode, head)?;
        filter_ops(conn, q, &mut ops)?;
        return Ok(ops);
    };

    let mut budget = initial;
    loop {
        // `active` with a forward continuation below HEAD has no bounded form:
        // rebuilding the branch reads every operation either way.
        let bounded_walk =
            q.mode != Mode::Tree && !(q.mode == Mode::Active && log::has_children(conn, head)?);
        let (mut ops, exhausted) = if bounded_walk {
            let mut chain = log::ancestry_ops_limited(conn, head, budget)?;
            let exhausted = chain.len() < budget;
            chain.reverse(); // root → HEAD, oldest first
            (chain, exhausted)
        } else {
            (whole_line(conn, q.mode, head)?, true)
        };
        filter_ops(conn, q, &mut ops)?;
        if exhausted || satisfied(q, &ops) {
            bound_ops(q, &mut ops);
            return Ok(ops);
        }
        budget = budget.saturating_mul(GROWTH);
    }
}

/// The mode's whole line through the log, oldest first.
fn whole_line(conn: &rusqlite::Connection, mode: Mode, head: i64) -> Result<Vec<OpRow>> {
    Ok(match mode {
        Mode::Tree => log::all_ops(conn)?,
        Mode::Linear => {
            let mut chain = log::ancestry_ops(conn, head)?;
            chain.reverse();
            chain
        }
        Mode::Active => log::active_line_ops(conn, head)?,
    })
}

/// Drops the operations the request does not select: another metarecord's, or
/// one whose revision falls outside the time window.
fn filter_ops(conn: &rusqlite::Connection, q: &LogQuery, ops: &mut Vec<OpRow>) -> Result<()> {
    if let Some(filter) = q.entity {
        ops.retain(|op| op.entity_uuid == filter);
    }
    if q.since.is_some() || q.until.is_some() {
        let meta = revision_meta(conn, ops.iter().map(|op| op.rev_id))?;
        ops.retain(|op| {
            let ts = meta.get(&op.rev_id).map(|(ts, _, _)| *ts).unwrap_or(0);
            q.since.is_none_or(|s| ts >= s) && q.until.is_none_or(|u| ts <= u)
        });
    }
    Ok(())
}

/// Whether the walk so far already holds everything the request will return.
///
/// A revision bound needs one revision more than it returns: the oldest one in
/// the window is the one the walk may have cut in half, and a client must never
/// be shown half a revision.
fn satisfied(q: &LogQuery, ops: &[OpRow]) -> bool {
    let by_limit = q.limit.is_some_and(|l| ops.len() >= l);
    let by_revisions = q.revisions.is_some_and(|r| distinct_revisions(ops) > r);
    match (q.limit, q.revisions) {
        (Some(_), Some(_)) => by_limit && by_revisions,
        _ => by_limit || by_revisions,
    }
}

fn distinct_revisions(ops: &[OpRow]) -> usize {
    ops.iter().map(|op| op.rev_id).collect::<HashSet<_>>().len()
}

/// Trims the selection to what was asked for: the most recent `limit`
/// operations, then the most recent `revisions` revisions — whole ones, so an
/// operation is never shown without its siblings.
fn bound_ops(q: &LogQuery, ops: &mut Vec<OpRow>) {
    if let Some(limit) = q.limit {
        if ops.len() > limit {
            ops.drain(..ops.len() - limit);
        }
    }
    if let Some(max) = q.revisions {
        let kept = newest_revisions(ops, max);
        ops.retain(|op| kept.contains(&op.rev_id));
    }
}

/// The ids of the `max` most recent revisions present in `ops` (oldest first).
fn newest_revisions(ops: &[OpRow], max: usize) -> HashSet<i64> {
    let mut kept = HashSet::new();
    for op in ops.iter().rev() {
        if !kept.contains(&op.rev_id) {
            if kept.len() == max {
                break;
            }
            kept.insert(op.rev_id);
        }
    }
    kept
}

/// How much the ancestry walk may read on its first attempt, or `None` when the
/// request is unbounded.
fn walk_budget(q: &LogQuery) -> Option<usize> {
    match (q.limit, q.revisions) {
        (None, None) => None,
        (Some(l), None) => Some(l.max(1)),
        (None, Some(r)) => Some(r.saturating_mul(OPS_PER_REVISION).max(1)),
        (Some(l), Some(r)) => Some(l.max(r.saturating_mul(OPS_PER_REVISION)).max(1)),
    }
}

/// Operations assumed per revision when a request is bounded by revisions. A
/// revision that holds more (a reconcile writes thousands) simply costs another
/// round of the walk.
const OPS_PER_REVISION: usize = 8;

/// How much wider each round of the walk is than the one before.
const GROWTH: usize = 8;

/// What a listing needs of a revision: when it was written, its label, and who
/// wrote it (`origin`).
type RevisionMeta = (i64, Option<String>, Option<String>);

/// `(timestamp, label, origin)` for the given revision ids — one query over a
/// bounded id set, never a scan of the table.
fn revision_meta(
    conn: &rusqlite::Connection,
    ids: impl Iterator<Item = i64>,
) -> Result<HashMap<i64, RevisionMeta>> {
    /// Ids per `IN (…)` query. SQLite allows 32 766 parameters; a few hundred
    /// keeps the prepared-statement cache useful on a large window.
    const CHUNK: usize = 256;

    let unique: Vec<i64> = {
        let mut seen = HashSet::new();
        ids.filter(|id| seen.insert(*id)).collect()
    };
    let mut out = HashMap::with_capacity(unique.len());
    for chunk in unique.chunks(CHUNK) {
        let placeholders = std::iter::repeat_n("?", chunk.len()).collect::<Vec<_>>().join(",");
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT id, timestamp, label, origin FROM revision WHERE id IN ({placeholders})"
        ))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |r| {
            Ok((r.get::<_, i64>(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
        for row in rows {
            let (id, ts, label, origin) = row?;
            out.insert(id, (ts, label, origin));
        }
    }
    Ok(out)
}

/// One operation, in the shape every `/log` response uses (spec-event-log).
pub fn op_json(
    conn: &rusqlite::Connection,
    op: &OpRow,
    include_snapshots: bool,
) -> Result<serde_json::Value> {
    let mut value = json!({
        "id": op.id,
        "parent_id": op.parent_id,
        "rev_id": op.rev_id,
        "seq": op.seq,
        "op_type": op.op_type,
        "entity_uuid": op.entity_uuid.as_simple().to_string(),
        "field_name": op.field_name,
        "reverts_op_id": op.reverts_op_id,
    });
    if include_snapshots {
        value["snapshots_before"] = snapshots_json(conn, op.id, 0)?;
        value["snapshots_after"] = snapshots_json(conn, op.id, 1)?;
    }
    Ok(value)
}

/// Snapshot rows in their raw column form (spec-event-log examples).
pub fn snapshots_json(
    conn: &rusqlite::Connection,
    op_id: i64,
    is_new: i64,
) -> Result<serde_json::Value> {
    let blob_hex = |b: Vec<u8>| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    let mut out = Vec::new();
    for row in log::snapshots(conn, op_id, is_new)? {
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
