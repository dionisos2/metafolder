//! The logged write flow (spec-event-log "Normal write flow"). Every write to
//! the data tables goes through a [`Writer`], which records a revision, one
//! operation per atomic change with before/after snapshots, and keeps the
//! `log_head` pointer consistent with the data tables — all in one SQLite
//! transaction.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use rusqlite::{params, Transaction};
use uuid::Uuid;

pub use metafolder_core::date::now_ms;
use metafolder_core::metarecord::{Field, FieldType, MetaRecord, Value};

use crate::db::{self, FieldRow};
use crate::error::DomainError;
use crate::version;

/// The `revision.origin` of a revision the daemon writes on the filesystem's
/// behalf — the watcher's flush and the restoration replay (spec-event-log
/// "Revision origin"). A client's own write leaves the column NULL.
pub const ORIGIN_WATCHER: &str = "watcher";

/// `revision.origin` for the metarecord deletion a trashing writes
/// (spec-trash "Deleting the metarecords").
///
/// Deliberately *not* `watcher`: a trashing is a write the user asked for and
/// must stay undoable, and only `watcher` disqualifies a revision from being
/// undone (spec-event-log "Revision origin"). It is a distinct value only so
/// that a client walking a rollback knows the bytes of a deleted metarecord are
/// in the trash-bin rather than gone.
pub const ORIGIN_TRASH: &str = "trash";

/// Maximum depth of a TreeRef chain (spec-main invariant).
pub const MAX_TREE_DEPTH: usize = 1000;

/// Operation types recorded in the log (spec-event-log "Operation types").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpType {
    CreateRecord,
    DeleteRecord,
    SetRecord,
    SetField,
    AppendField,
    DeleteField,
    FileDeleted,
    FileMoved,
    FileModified,
    Unknown,
}

impl OpType {
    pub fn as_str(self) -> &'static str {
        match self {
            OpType::CreateRecord => "create_metarecord",
            OpType::DeleteRecord => "delete_metarecord",
            OpType::SetRecord => "set_metarecord",
            OpType::SetField => "set_field",
            OpType::AppendField => "append_field",
            OpType::DeleteField => "delete_field",
            OpType::FileDeleted => "file_deleted",
            OpType::FileMoved => "file_moved",
            OpType::FileModified => "file_modified",
            OpType::Unknown => "unknown",
        }
    }

    /// Whether the operation is a *manual* write — something a client asked
    /// for — as opposed to one the watcher records on the filesystem's behalf.
    ///
    /// The filesystem is authoritative for `mfr_path`: a directory that is gone
    /// is gone, and the watcher nulls its whole subtree (spec-file-tracking
    /// "Cascading removal"). A manual write has no such story, so it is held to
    /// the forest's referential integrity instead (see
    /// `Writer::check_forest_integrity`).
    pub fn is_manual(self) -> bool {
        !matches!(self, OpType::FileDeleted | OpType::FileMoved | OpType::FileModified)
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "create_metarecord" => OpType::CreateRecord,
            "delete_metarecord" => OpType::DeleteRecord,
            "set_metarecord" => OpType::SetRecord,
            "set_field" => OpType::SetField,
            "append_field" => OpType::AppendField,
            "delete_field" => OpType::DeleteField,
            "file_deleted" => OpType::FileDeleted,
            "file_moved" => OpType::FileMoved,
            "file_modified" => OpType::FileModified,
            "unknown" => OpType::Unknown,
            _ => return None,
        })
    }
}

// ── History reading ───────────────────────────────────────────────────────────

/// One row of the `operation` table.
#[derive(Debug, Clone)]
pub struct OpRow {
    pub id: i64,
    pub parent_id: Option<i64>,
    pub rev_id: i64,
    pub seq: i64,
    pub op_type: String,
    pub entity_uuid: Uuid,
    pub entity_version_before: Option<u64>,
    /// The version the entity held *after* this op. Redo restores it exactly;
    /// `None` on pre-migration rows, where forward application falls back to
    /// `entity_version_before + 1`.
    pub entity_version_after: Option<u64>,
    pub field_name: Option<String>,
    /// The operation this one undid, when a revert wrote it (spec-event-log
    /// "Revert"). `None` for an ordinary write — and for every row of a
    /// database written before the column existed. Allowed to dangle: pruning
    /// may remove the operation it names.
    pub reverts_op_id: Option<i64>,
    /// The `origin` of the revision this operation belongs to (spec-event-log
    /// "Revision origin"), carried along so a reader never has to go back to
    /// the revision for it. `None` for a revision nothing stamped.
    ///
    /// It is what separates two operations the op type alone cannot: an
    /// ordinary `delete_metarecord` touches no file, while the one a *trashing*
    /// wrote has its bytes waiting in the trash-bin.
    pub origin: Option<String>,
}

/// The version `op`'s entity held *before the whole revision* `op` belongs to —
/// the `entity_version_before` of the revision's *first* operation on it.
///
/// Selected by `seq`, and not as the smallest of the revision's values: a
/// version is a content hash and carries no order (spec-data-model "Version"),
/// so "before the revision" is a position in the revision, not a minimum.
///
/// One event can write several fields of one record (orphaning writes both
/// `mfr_path` and `mfr_path_old`), and each operation then restores to its own
/// intermediate version. Anything that observed the record from *outside* the
/// revision — a trash entry, which records the version at the moment the file
/// was trashed — knows only this pre-revision version, so the rollback
/// correlation must be made against it and not against a per-op version
/// (spec-trash "rollback auto-restore").
pub fn entity_version_before_revision(
    conn: &rusqlite::Connection,
    op: &OpRow,
) -> Result<Option<u64>> {
    use rusqlite::OptionalExtension as _;
    let first: Option<Option<i64>> = conn
        .query_row(
            "SELECT entity_version_before FROM operation \
             WHERE rev_id = ?1 AND entity_uuid = ?2 ORDER BY seq LIMIT 1",
            params![op.rev_id, db::uuid_to_bytes(op.entity_uuid)],
            |r| r.get(0),
        )
        .optional()?;
    Ok(first.flatten().map(|v| v as u64))
}

pub fn get_head(conn: &rusqlite::Connection) -> Result<Option<i64>> {
    Ok(conn.query_row("SELECT op_id FROM log_head WHERE singleton = 1", [], |r| r.get(0))?)
}

/// The `operation` columns `row_to_op` reads, qualified with `alias` — the
/// table's name or its alias in the query.
///
/// One source of truth on purpose: six queries read this row shape, three of
/// them through a CTE join that has to qualify every name. A column added to
/// one list and not the others is not a compile error, it is an "Invalid
/// column index" the first time that path runs.
fn op_columns(alias: &str) -> String {
    format!(
        "{alias}.id, {alias}.parent_id, {alias}.rev_id, {alias}.seq, {alias}.op_type, \
         {alias}.entity_uuid, {alias}.entity_version_before, {alias}.entity_version_after, \
         {alias}.field_name, {alias}.reverts_op_id, \
         (SELECT origin FROM revision WHERE revision.id = {alias}.rev_id)"
    )
}

fn row_to_op(row: &rusqlite::Row<'_>) -> rusqlite::Result<(OpRow, Vec<u8>)> {
    let entity: Vec<u8> = row.get(5)?;
    Ok((
        OpRow {
            id: row.get(0)?,
            parent_id: row.get(1)?,
            rev_id: row.get(2)?,
            seq: row.get(3)?,
            op_type: row.get(4)?,
            entity_uuid: Uuid::nil(), // patched by the caller from the blob
            entity_version_before: row.get::<_, Option<i64>>(6)?.map(|v| v as u64),
            entity_version_after: row.get::<_, Option<i64>>(7)?.map(|v| v as u64),
            field_name: row.get(8)?,
            reverts_op_id: row.get(9)?,
            origin: row.get(10)?,
        },
        entity,
    ))
}

pub fn get_op(conn: &rusqlite::Connection, id: i64) -> Result<Option<OpRow>> {
    use rusqlite::OptionalExtension as _;
    let row = conn
        .prepare_cached(&format!(
            "SELECT {} FROM operation WHERE id = ?1",
            op_columns("operation")
        ))?
        .query_row(params![id], row_to_op)
        .optional()?;
    row.map(|(mut op, entity)| {
        op.entity_uuid = db::bytes_to_uuid(entity)?;
        Ok(op)
    })
    .transpose()
}

/// All operations, in insertion order.
pub fn all_ops(conn: &rusqlite::Connection) -> Result<Vec<OpRow>> {
    let mut stmt =
        conn.prepare(&format!("SELECT {} FROM operation ORDER BY id", op_columns("operation")))?;
    let ops = stmt
        .query_map([], row_to_op)?
        .map(|r| {
            let (mut op, entity) = r?;
            op.entity_uuid = db::bytes_to_uuid(entity)?;
            Ok(op)
        })
        .collect::<Result<Vec<OpRow>>>()?;
    Ok(ops)
}

/// Operations created after `op_id` — by creation order, across *all* branches
/// (a new edit after a rollback is parented elsewhere but still has a larger
/// id). The change delta a client polls to learn which metarecords were touched
/// since it last synced (each op names its `entity_uuid`).
/// How many operations are newer than `op_id`. Cheap count used by the change
/// feed to decide whether a delta is small enough to stream op-by-op or must be
/// collapsed to a coarse "everything changed" signal (a large reconcile would
/// otherwise flood the client with tens of thousands of operations).
pub fn ops_since_count(conn: &rusqlite::Connection, op_id: i64) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM operation WHERE id > ?1", [op_id], |r| r.get(0))?)
}

pub fn ops_since(conn: &rusqlite::Connection, op_id: i64) -> Result<Vec<OpRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {} FROM operation WHERE id > ?1 ORDER BY id",
        op_columns("operation")
    ))?;
    let ops = stmt
        .query_map([op_id], row_to_op)?
        .map(|r| {
            let (mut op, entity) = r?;
            op.entity_uuid = db::bytes_to_uuid(entity)?;
            Ok(op)
        })
        .collect::<Result<Vec<OpRow>>>()?;
    Ok(ops)
}

/// The recursive CTE walking the parent chain from `?1` up to the root.
/// `?2` caps the walk at (operation count + 1) rows so a corrupted log with a
/// cycle terminates instead of looping; the duplicate id is detected in Rust.
const ANCESTRY_CTE: &str = "
    WITH RECURSIVE chain(id, depth) AS (
        SELECT ?1, 0
        UNION ALL
        SELECT o.parent_id, c.depth + 1
        FROM chain c JOIN operation o ON o.id = c.id
        WHERE o.parent_id IS NOT NULL
        LIMIT ?2
    )";

fn cycle_cap(conn: &rusqlite::Connection) -> Result<i64> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM operation", [], |r| r.get(0))?;
    Ok(count + 1)
}

/// Ancestor chain from `from` (inclusive) up to the root, in that order.
/// One recursive CTE instead of one query per operation.
pub fn ancestry(conn: &rusqlite::Connection, from: i64) -> Result<Vec<i64>> {
    Ok(ancestry_ops(conn, from)?.into_iter().map(|op| op.id).collect())
}

/// Full operation rows of the ancestor chain from `from` (inclusive) up to
/// the root, in that order.
pub fn ancestry_ops(conn: &rusqlite::Connection, from: i64) -> Result<Vec<OpRow>> {
    let cols = op_columns("o");
    let mut stmt = conn.prepare_cached(&format!(
        "{ANCESTRY_CTE}
         SELECT {cols}
         FROM chain c JOIN operation o ON o.id = c.id
         ORDER BY c.depth"
    ))?;
    let ops = stmt
        .query_map(params![from, cycle_cap(conn)?], row_to_op)?
        .map(|r| {
            let (mut op, entity) = r?;
            op.entity_uuid = db::bytes_to_uuid(entity)?;
            Ok(op)
        })
        .collect::<Result<Vec<OpRow>>>()?;
    if ops.is_empty() {
        return Err(DomainError::NotFound(format!("operation {from} not found")).into());
    }
    let mut seen = HashSet::new();
    for op in &ops {
        if !seen.insert(op.id) {
            anyhow::bail!("operation history contains a cycle at op {}", op.id);
        }
    }
    Ok(ops)
}

/// The ancestor chain from `from` (inclusive) back to — but *excluding* —
/// `until`, HEAD-first; `None` if `until` was not reached within `max`
/// operations (it is not on the chain, or the delta is larger than the budget).
///
/// Unlike [`ancestry_ops`] the recursion *stops* at the anchor instead of
/// materialising the whole chain up to the root, and unlike
/// [`ancestry_ops_limited`] it never reads `max` rows just because the budget
/// allows it. This is what the bitmap index's forward delta needs: after a write
/// the delta is one or two operations, and it is read on the read path before
/// every query that follows a write — walking to the root there made every such
/// query cost O(total log length). Like the bounded walk it does not validate
/// the chain against cycles (the budget bounds it).
pub fn ancestry_ops_until(
    conn: &rusqlite::Connection,
    from: i64,
    until: i64,
    max: usize,
) -> Result<Option<Vec<OpRow>>> {
    Ok(match delta_until(conn, from, until, max)? {
        Delta::Found(ops) => Some(ops),
        Delta::Budget | Delta::Unrelated => None,
    })
}

/// What a bounded ancestor walk found. [`ancestry_ops_until`] flattens the two
/// failures into `None`; they are kept apart here for the caller that *widens*
/// its budget ([`linear_path`]), which has to tell "not far enough yet" from
/// "not on this chain at all" — the first is a reason to look again, the second
/// is the answer.
enum Delta {
    /// The chain from `from` (inclusive) down to — but excluding — the anchor.
    Found(Vec<OpRow>),
    /// The budget ran out before the anchor was met; it may still be further
    /// down the chain.
    Budget,
    /// The walk reached the root of the history without meeting the anchor, so
    /// the anchor is not an ancestor of `from` at all.
    Unrelated,
}

fn delta_until(conn: &rusqlite::Connection, from: i64, until: i64, max: usize) -> Result<Delta> {
    if from == until {
        return Ok(Delta::Found(Vec::new()));
    }
    // `c.id <> ?2` stops the expansion once the anchor is reached, so the anchor
    // itself is the last row produced and its parent is never visited. `max + 1`
    // rows leaves room for that trailing anchor row on a maximal delta — and is
    // what tells a truncated walk from one that ran out of history.
    let cols = op_columns("o");
    let mut stmt = conn.prepare_cached(&format!(
        "WITH RECURSIVE chain(id, depth) AS (
             SELECT ?1, 0
             UNION ALL
             SELECT o.parent_id, c.depth + 1
             FROM chain c JOIN operation o ON o.id = c.id
             WHERE o.parent_id IS NOT NULL AND c.id <> ?2
             LIMIT ?3
         )
         SELECT {cols}
         FROM chain c JOIN operation o ON o.id = c.id
         ORDER BY c.depth"
    ))?;
    let mut ops = stmt
        .query_map(params![from, until, max as i64 + 1], row_to_op)?
        .map(|r| {
            let (mut op, entity) = r?;
            op.entity_uuid = db::bytes_to_uuid(entity)?;
            Ok(op)
        })
        .collect::<Result<Vec<OpRow>>>()?;
    match ops.last() {
        // The anchor closed the walk: drop it, the delta is what sits on top.
        Some(last) if last.id == until => {
            ops.pop();
            Ok(Delta::Found(ops))
        }
        // Every row the budget allowed, and still no anchor.
        _ if ops.len() > max => Ok(Delta::Budget),
        // The walk stopped on a row with no parent (or `from` does not exist):
        // the anchor is nowhere on this chain.
        _ => Ok(Delta::Unrelated),
    }
}

/// The most-recent `max` operations of the ancestor chain from `from`
/// (inclusive), HEAD-first: `from` and its `max - 1` nearest ancestors. Unlike
/// [`ancestry_ops`] the walk is bounded by a small `LIMIT`, so it stays O(max)
/// on a huge log (each step is a primary-key lookup up the parent chain); it
/// therefore does not validate the chain against cycles. Backs the bounded log
/// listing that keeps `GET /log?…&limit=N` fast on repositories with millions
/// of operations.
pub fn ancestry_ops_limited(
    conn: &rusqlite::Connection,
    from: i64,
    max: usize,
) -> Result<Vec<OpRow>> {
    let cols = op_columns("o");
    let mut stmt = conn.prepare_cached(&format!(
        "{ANCESTRY_CTE}
         SELECT {cols}
         FROM chain c JOIN operation o ON o.id = c.id
         ORDER BY c.depth"
    ))?;
    let ops = stmt
        .query_map(params![from, max as i64], row_to_op)?
        .map(|r| {
            let (mut op, entity) = r?;
            op.entity_uuid = db::bytes_to_uuid(entity)?;
            Ok(op)
        })
        .collect::<Result<Vec<OpRow>>>()?;
    Ok(ops)
}

/// Whether any operation has `op_id` as its `parent_id` — i.e. whether a forward
/// continuation (a rolled-back redo future, or a divergent branch) exists below
/// `op_id`. Indexed lookup (`idx_operation_parent`), so O(log N) even on a huge
/// log; used to take the cheap bounded-ancestry path in the log listing when
/// HEAD is a plain tip.
pub fn has_children(conn: &rusqlite::Connection, op_id: i64) -> Result<bool> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM operation WHERE parent_id = ?1)",
        [op_id],
        |r| r.get(0),
    )?;
    Ok(exists)
}

/// The "active line" through HEAD: the ancestry path root→HEAD followed by the
/// forward continuation that, at each fork below HEAD, follows the child whose
/// subtree contains the most recently created operation (largest id). This
/// reconstructs the branch the user was last on, so a rolled-back "future"
/// stays visible (for redo) while operations on divergent branches are hidden.
/// Returned root→leaf (oldest first), like the `linear` mode.
pub fn active_line_ops(conn: &rusqlite::Connection, head: i64) -> Result<Vec<OpRow>> {
    // Ancestry is HEAD→root; reverse to root→HEAD.
    let mut line = ancestry_ops(conn, head)?;
    line.reverse();

    // Build the child map from every operation to walk forward from HEAD.
    let all = all_ops(conn)?;
    let mut children: HashMap<i64, Vec<i64>> = HashMap::new();
    for op in &all {
        if let Some(parent) = op.parent_id {
            children.entry(parent).or_default().push(op.id);
        }
    }
    // Subtree-max id per node. A child is always created after its parent
    // (parent_id < id), so processing ids in descending order visits children
    // before parents — one bottom-up pass, no recursion.
    let mut subtree_max: HashMap<i64, i64> = HashMap::new();
    let mut ids: Vec<i64> = all.iter().map(|o| o.id).collect();
    ids.sort_unstable_by(|a, b| b.cmp(a));
    for id in ids {
        let mut m = id;
        if let Some(kids) = children.get(&id) {
            for &c in kids {
                m = m.max(*subtree_max.get(&c).unwrap_or(&c));
            }
        }
        subtree_max.insert(id, m);
    }

    // Walk forward from HEAD, always descending toward the largest reachable id.
    let by_id: HashMap<i64, &OpRow> = all.iter().map(|o| (o.id, o)).collect();
    let mut cur = head;
    while let Some(kids) = children.get(&cur) {
        let Some(&next) = kids.iter().max_by_key(|&&c| subtree_max.get(&c).copied().unwrap_or(c))
        else {
            break;
        };
        line.push((*by_id.get(&next).expect("child op present")).clone());
        cur = next;
    }
    Ok(line)
}

/// Snapshot rows of one operation (`is_new` 0 = before, 1 = after).
pub fn snapshots(conn: &rusqlite::Connection, op_id: i64, is_new: i64) -> Result<Vec<FieldRow>> {
    let mut stmt = conn.prepare_cached(
        "SELECT field_id, field_name, value_type, value_text, value_int, value_real,
                value_uuid, value_ref_repo, value_name, value_name_bytes
         FROM op_snapshot WHERE op_id = ?1 AND is_new = ?2 ORDER BY field_id",
    )?;
    let rows = stmt
        .query_map(params![op_id, is_new], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, db::RawValue::from_row(row)?))
        })?
        .map(|r| {
            let (id, name, raw) = r?;
            let value = db::decode_value(raw)?;
            Ok(FieldRow { id, name, value })
        })
        .collect::<Result<Vec<FieldRow>>>()?;
    Ok(rows)
}

// ── Navigation (spec-event-log "Navigation") ──────────────────────────────────

/// A rollback target, as given in the API request.
#[derive(Debug)]
pub enum Target {
    Id(i64),
    Timestamp(i64),
    Label(String),
    PrevRevision,
}

/// Resolves a target to an operation id; `Ok(None)` is the empty state.
pub fn resolve_target(conn: &rusqlite::Connection, target: &Target) -> Result<Option<i64>> {
    let head = get_head(conn)?;
    match target {
        Target::Id(id) => {
            get_op(conn, *id)?
                .ok_or_else(|| DomainError::NotFound(format!("operation {id} not found")))?;
            Ok(Some(*id))
        }
        Target::Timestamp(t) => {
            use rusqlite::OptionalExtension as _;
            let Some(head) = head else {
                anyhow::bail!("no operation found at or before timestamp {t} (empty history)");
            };
            // Walking from HEAD down, the first operation whose revision is
            // at or before the timestamp.
            let found: Option<i64> = conn
                .prepare_cached(&format!(
                    "{ANCESTRY_CTE}
                     SELECT c.id FROM chain c
                     JOIN operation o ON o.id = c.id
                     JOIN revision r ON r.id = o.rev_id
                     WHERE r.timestamp <= ?3
                     ORDER BY c.depth LIMIT 1"
                ))?
                .query_row(params![head, cycle_cap(conn)?, t], |r| r.get(0))
                .optional()?;
            found
                .map(Some)
                .with_context(|| format!("no operation found at or before timestamp {t}"))
        }
        Target::Label(label) => {
            use rusqlite::OptionalExtension as _;
            let Some(head) = head else {
                return Err(DomainError::NotFound(format!(
                    "label '{label}' not found (empty history)"
                ))
                .into());
            };
            // Walking from HEAD down, the first op of a matching revision is
            // the last operation of the most recent matching revision.
            let found: Option<i64> = conn
                .prepare_cached(&format!(
                    "{ANCESTRY_CTE}
                     SELECT c.id FROM chain c
                     JOIN operation o ON o.id = c.id
                     JOIN revision r ON r.id = o.rev_id
                     WHERE r.label = ?3
                     ORDER BY c.depth LIMIT 1"
                ))?
                .query_row(params![head, cycle_cap(conn)?, label], |r| r.get(0))
                .optional()?;
            found.map(Some).ok_or_else(|| {
                DomainError::NotFound(format!(
                    "label '{label}' not found on the HEAD ancestry path"
                ))
                .into()
            })
        }
        Target::PrevRevision => {
            let Some(head) = head else {
                anyhow::bail!("nothing to undo: the history is empty");
            };
            // The first operation of HEAD's revision (operations of one
            // revision form a chain); its parent is the state before the
            // whole revision (None = empty state).
            let parent: Option<i64> = conn.query_row(
                "SELECT parent_id FROM operation
                 WHERE rev_id = (SELECT rev_id FROM operation WHERE id = ?1)
                 ORDER BY seq LIMIT 1",
                params![head],
                |r| r.get(0),
            )?;
            Ok(parent)
        }
    }
}

#[derive(Debug, serde::Serialize)]
pub struct NavResult {
    pub previous_head: Option<i64>,
    pub new_head: Option<i64>,
    pub operations_unapplied: usize,
    pub operations_applied: usize,
}

/// Outcome of [`Writer::retype_field`]: how many rows were converted and the
/// metarecords whose values could not be converted and fell back to the
/// target's sentinel (deduped, sorted).
pub struct RetypeSummary {
    pub converted: usize,
    pub fallback_uuids: Vec<Uuid>,
}

/// Moves HEAD to `target` in one atomic transaction: inverse operations on
/// the path up to the LCA, forward operations down to the target.
pub fn navigate(conn: &mut rusqlite::Connection, target: Option<i64>) -> Result<NavResult> {
    let previous_head = get_head(conn)?;
    if previous_head == target {
        return Ok(NavResult {
            previous_head,
            new_head: target,
            operations_unapplied: 0,
            operations_applied: 0,
        });
    }
    let tx = conn.transaction()?;

    let (unapply, apply): (Vec<i64>, Vec<i64>) = match (previous_head, target) {
        (None, None) => (vec![], vec![]),
        (Some(head), None) => {
            // Empty state: every data row of this repository is removed (one repo
            // per database file, so the whole `metarecord` table goes).
            let unapplied = ancestry(&tx, head)?.len();
            tx.execute("DELETE FROM metarecord", [])?;
            // One repo per database file, so emptying it clears the whole FTS index.
            tx.execute("UPDATE log_head SET op_id = NULL WHERE singleton = 1", [])?;
            tx.commit()?;
            return Ok(NavResult {
                previous_head,
                new_head: None,
                operations_unapplied: unapplied,
                operations_applied: 0,
            });
        }
        (None, Some(t)) => {
            let mut chain = ancestry(&tx, t)?;
            chain.reverse(); // root → target
            (vec![], chain)
        }
        (Some(h), Some(t)) => {
            let h_anc = ancestry(&tx, h)?;
            let h_set: HashSet<i64> = h_anc.iter().copied().collect();
            let t_anc = ancestry(&tx, t)?;
            let lca = t_anc.iter().find(|id| h_set.contains(id)).copied();
            let unapply: Vec<i64> = h_anc.into_iter().take_while(|id| Some(*id) != lca).collect();
            let mut apply: Vec<i64> = t_anc.into_iter().take_while(|id| Some(*id) != lca).collect();
            apply.reverse(); // oldest first
            (unapply, apply)
        }
    };

    for op_id in &unapply {
        let op = get_op(&tx, *op_id)?.context("operation vanished during navigation")?;
        apply_inverse(&tx, &op)?;
    }
    for op_id in &apply {
        let op = get_op(&tx, *op_id)?.context("operation vanished during navigation")?;
        apply_forward(&tx, &op)?;
    }
    tx.execute("UPDATE log_head SET op_id = ?1 WHERE singleton = 1", params![target])?;
    tx.commit()?;

    Ok(NavResult {
        previous_head,
        new_head: target,
        operations_unapplied: unapply.len(),
        operations_applied: apply.len(),
    })
}

/// Direction of one step in a coordinated navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavDir {
    /// Undo the operation (rollback toward an ancestor / the LCA).
    Inverse,
    /// Re-apply the operation (redo toward a descendant target).
    Forward,
}

/// The unapply and apply id lists from `head` toward `target`, one operation
/// at a time (unlike [`navigate`], the empty-state case is not bulk-deleted).
fn step_paths(
    conn: &rusqlite::Connection,
    head: Option<i64>,
    target: Option<i64>,
) -> Result<(Vec<i64>, Vec<i64>)> {
    Ok(match (head, target) {
        (None, None) => (vec![], vec![]),
        (Some(h), None) => (ancestry(conn, h)?, vec![]),
        (None, Some(t)) => {
            let mut chain = ancestry(conn, t)?;
            chain.reverse();
            (vec![], chain)
        }
        (Some(h), Some(t)) => {
            let h_anc = ancestry(conn, h)?;
            let h_set: HashSet<i64> = h_anc.iter().copied().collect();
            let t_anc = ancestry(conn, t)?;
            let lca = t_anc.iter().find(|id| h_set.contains(id)).copied();
            let unapply: Vec<i64> = h_anc.into_iter().take_while(|id| Some(*id) != lca).collect();
            let mut apply: Vec<i64> = t_anc.into_iter().take_while(|id| Some(*id) != lca).collect();
            apply.reverse();
            (unapply, apply)
        }
    })
}

/// The first budget [`linear_path`] walks with, and the factor it widens by.
/// Small enough that the ordinary case — undoing the answer just given — reads
/// a handful of rows, and geometric so that a large delta still costs O(delta)
/// in total rather than one walk per widening.
const LINEAR_BUDGET: usize = 256;
const LINEAR_WIDEN: usize = 8;

/// The path between two operations that lie on one chain — the whole of a
/// rollback, and the whole of a redo — or `None` when they sit on diverging
/// branches and only the LCA answers.
///
/// This is the bounded half of [`nav_path`], and the reason it exists is the
/// *coordinated* navigation: it asks for the path again before every single
/// step, so a walk to the root of the log there is paid once per operation
/// undone. Going back over a tag applied to a folder of a thousand files then
/// costs a thousand walks over the whole history — the "back" key of a
/// classification walk (spec-gui "Reserved keys") taking minutes to answer.
///
/// The budget widens instead of being guessed: [`Delta::Budget`] means "look
/// further", [`Delta::Unrelated`] means "not this way", and only two
/// `Unrelated`s — the genuinely divergent case — fall back to the LCA.
fn linear_path(
    conn: &rusqlite::Connection,
    head: i64,
    target: i64,
) -> Result<Option<Vec<(OpRow, NavDir)>>> {
    let (mut backward, mut forward) = (true, true);
    let mut budget = LINEAR_BUDGET;
    while backward || forward {
        if backward {
            match delta_until(conn, head, target, budget)? {
                // The target is an ancestor: unapply everything above it,
                // newest first.
                Delta::Found(ops) => {
                    return Ok(Some(ops.into_iter().map(|op| (op, NavDir::Inverse)).collect()))
                }
                Delta::Unrelated => backward = false,
                Delta::Budget => {}
            }
        }
        if forward {
            match delta_until(conn, target, head, budget)? {
                // The target is a descendant: re-apply the chain down to it,
                // oldest first.
                Delta::Found(mut ops) => {
                    ops.reverse();
                    return Ok(Some(ops.into_iter().map(|op| (op, NavDir::Forward)).collect()));
                }
                Delta::Unrelated => forward = false,
                Delta::Budget => {}
            }
        }
        budget = budget.saturating_mul(LINEAR_WIDEN);
    }
    Ok(None)
}

/// The full ordered list of operations to process to move HEAD from `head` to
/// `target`: each unapply op (most recent first) as [`NavDir::Inverse`], then
/// each apply op (oldest first) as [`NavDir::Forward`]. Empty when already at
/// the target (spec-event-log "Coordinated navigation").
pub fn nav_path(
    conn: &rusqlite::Connection,
    head: Option<i64>,
    target: Option<i64>,
) -> Result<Vec<(OpRow, NavDir)>> {
    if head == target {
        return Ok(vec![]);
    }
    // Almost every navigation is along one chain — a rollback to an ancestor of
    // HEAD, a redo to a descendant of it — and that path can be read without
    // ever walking to the root of the log.
    if let (Some(h), Some(t)) = (head, target) {
        if let Some(path) = linear_path(conn, h, t)? {
            return Ok(path);
        }
    }
    let (unapply, apply) = step_paths(conn, head, target)?;
    let mut out = Vec::with_capacity(unapply.len() + apply.len());
    for id in unapply {
        out.push((
            get_op(conn, id)?.context("operation vanished during navigation")?,
            NavDir::Inverse,
        ));
    }
    for id in apply {
        out.push((
            get_op(conn, id)?.context("operation vanished during navigation")?,
            NavDir::Forward,
        ));
    }
    Ok(out)
}

/// Applies the *first* operation on the path from the current HEAD toward
/// `target` (one atomic step) and advances HEAD. Returns the new HEAD and what
/// the step did to the forest, for the caller's tree cache
/// ([`crate::tree_cache::TreeCache::apply_ops`]). When `skip` is set and the
/// operation is a file op, a restoration entry is enqueued in
/// `pending_operation` (replayed as a new branch once the lock is released —
/// spec-event-log "skip").
pub fn coordinated_step(
    conn: &mut rusqlite::Connection,
    target: Option<i64>,
    skip: bool,
) -> Result<(Option<i64>, Vec<TreeOp>)> {
    let head = get_head(conn)?;
    let path = nav_path(conn, head, target)?;
    let Some((op, dir)) = path.into_iter().next() else {
        return Ok((head, vec![])); // Already at the target.
    };
    let tx = conn.transaction()?;
    let tree = nav_tree_ops(&tx, &op, dir)?;
    if skip {
        enqueue_restoration(&tx, &op, dir)?;
    }
    let new_head = match dir {
        NavDir::Inverse => {
            apply_inverse(&tx, &op)?;
            op.parent_id
        }
        NavDir::Forward => {
            apply_forward(&tx, &op)?;
            Some(op.id)
        }
    };
    tx.execute("UPDATE log_head SET op_id = ?1 WHERE singleton = 1", params![new_head])?;
    tx.commit()?;
    Ok((new_head, tree))
}

/// What one navigation step does to the forest: the same description a
/// [`Writer`] records for its own writes ([`tree_ops_of`]), derived from the
/// operation's snapshots instead — which carry both the rows it put in place
/// and the rows it replaced, so nothing has to be read back afterwards.
fn nav_tree_ops(conn: &rusqlite::Connection, op: &OpRow, dir: NavDir) -> Result<Vec<TreeOp>> {
    let Some(op_type) = OpType::parse(&op.op_type) else {
        return Ok(Vec::new());
    };
    let before = snapshots(conn, op.id, 0)?;
    let after = snapshots(conn, op.id, 1)?;
    Ok(match dir {
        NavDir::Forward => tree_ops_of(op_type, &before, &after, op.entity_uuid),
        NavDir::Inverse => inverse_tree_ops(op_type, &before, &after, op.entity_uuid),
    })
}

/// Enqueues the restoration operation for a skipped file op: a synthetic
/// metadata write replayed after the lock is released, correcting the metadata
/// to match the actual filesystem (spec-event-log "skip"). No filesystem check.
///
/// For `file_moved` the restoration *rewinds* `mfr_path` to the location the
/// file is recorded at **before this step** — i.e. the snapshot that is *not*
/// the one the step just applied. On an inverse step the step applied the
/// pre-move location (`is_new=0`), so we rewind to `is_new=1`; on a forward
/// (redo) step it applied the post-move location (`is_new=1`), so we rewind to
/// `is_new=0`. Taking the wrong side leaves the metadata where the (skipped,
/// hence not performed) move would have put it. See `docs/review-followups.md`
/// (#6).
fn enqueue_restoration(tx: &Transaction<'_>, op: &OpRow, dir: NavDir) -> Result<()> {
    let entity = op.entity_uuid.as_simple().to_string();
    match op.op_type.as_str() {
        // Rewind to the file's recorded location before this step (the side the
        // step did *not* apply): is_new=1 for an inverse, is_new=0 for a redo.
        "file_moved" => {
            let recorded_is_new = match dir {
                NavDir::Inverse => 1,
                NavDir::Forward => 0,
            };
            let rows = snapshots(tx, op.id, recorded_is_new)?;
            let Some(row) = rows.iter().find(|r| r.name == "mfr_path") else {
                return Ok(());
            };
            if let Value::TreeRef { parent, name } = &row.value {
                let parent_hex = parent.map(|p| p.as_simple().to_string()).unwrap_or_default();
                tx.execute(
                    "INSERT INTO pending_operation (op_type, path, from_path, to_path)
                     VALUES ('restore_set_path', ?1, ?2, ?3)",
                    params![entity, parent_hex, name.display().as_ref()],
                )?;
            }
        }
        // The file is gone: re-record the deletion.
        "file_deleted" => {
            tx.execute(
                "INSERT INTO pending_operation (op_type, path) VALUES ('restore_clear_path', ?1)",
                params![entity],
            )?;
        }
        // The content changed: invalidate the hashes (size/mtime left stale).
        "file_modified" => {
            tx.execute(
                "INSERT INTO pending_operation (op_type, path) VALUES ('restore_clear_hashes', ?1)",
                params![entity],
            )?;
        }
        _ => {}
    }
    Ok(())
}

/// Writes a metarecord's version. The value is always derived from the
/// metarecord's own rows — by [`version::apply`] on a write, by
/// [`version::of_rows`] once navigation has restored them — never invented and
/// never read back from the log (spec-event-log "Field ID and version
/// stability").
fn set_version(tx: &Transaction<'_>, uuid: Uuid, version: u64) -> Result<()> {
    tx.prepare_cached("UPDATE metarecord SET version = ?1 WHERE uuid = ?2")?
        .execute(params![version as i64, db::uuid_to_bytes(uuid)])?;
    Ok(())
}

/// Recomputes a metarecord's version from the rows it currently holds. This is
/// what navigation uses: restoring the rows restores the version with them, so
/// there is no second source of truth that could disagree with the content.
fn resync_version(tx: &Transaction<'_>, uuid: Uuid) -> Result<()> {
    let rows = db::get_field_rows(tx, uuid)?;
    // A no-op when the step removed the metarecord: the UPDATE matches no row.
    set_version(tx, uuid, version::of_rows(uuid, &rows))
}

/// Undoes one operation (spec-event-log "Inverse operations"). Field rows
/// are restored with their original primary keys.
fn apply_inverse(tx: &Transaction<'_>, op: &OpRow) -> Result<()> {
    let entity = op.entity_uuid;
    match op.op_type.as_str() {
        "create_metarecord" => {
            tx.execute(
                "DELETE FROM metarecord WHERE uuid = ?1",
                params![db::uuid_to_bytes(entity)],
            )?;
        }
        "delete_metarecord" => {
            tx.execute(
                "INSERT INTO metarecord (uuid, version) VALUES (?1, 0)",
                params![db::uuid_to_bytes(entity)],
            )?;
            for row in snapshots(tx, op.id, 0)? {
                db::insert_field_row(tx, entity, &row.name, &row.value, Some(row.id))?;
            }
        }
        // Whole-record set: replace the entire field set (all names).
        "set_metarecord" => {
            tx.execute(
                "DELETE FROM field WHERE metarecord_uuid = ?1",
                params![db::uuid_to_bytes(entity)],
            )?;
            for row in snapshots(tx, op.id, 0)? {
                db::insert_field_row(tx, entity, &row.name, &row.value, Some(row.id))?;
            }
        }
        // All set-field-shaped operations (one field name, full replacement).
        "set_field" | "file_deleted" | "file_moved" | "file_modified" => {
            let field = op.field_name.as_deref().context("set-shaped op without field_name")?;
            tx.prepare_cached("DELETE FROM field WHERE metarecord_uuid = ?1 AND field_name = ?2")?
                .execute(params![db::uuid_to_bytes(entity), field])?;
            for row in snapshots(tx, op.id, 0)? {
                db::insert_field_row(tx, entity, &row.name, &row.value, Some(row.id))?;
            }
        }
        "append_field" => {
            for row in snapshots(tx, op.id, 1)? {
                tx.execute("DELETE FROM field WHERE id = ?1", params![row.id])?;
            }
        }
        "delete_field" => {
            for row in snapshots(tx, op.id, 0)? {
                db::insert_field_row(tx, entity, &row.name, &row.value, Some(row.id))?;
            }
        }
        "unknown" => anyhow::bail!("cannot navigate across an 'unknown' operation (op {})", op.id),
        other => anyhow::bail!("unsupported op_type '{other}' in the log"),
    }
    // The version is not restored from the log: it is a function of the rows
    // this step has just put back (spec-event-log "Field ID and version
    // stability"). `entity_version_before`/`after` are provenance, and nothing
    // reads them to decide what to write.
    resync_version(tx, entity)
}

/// Replays one operation forward (redo).
fn apply_forward(tx: &Transaction<'_>, op: &OpRow) -> Result<()> {
    let entity = op.entity_uuid;
    match op.op_type.as_str() {
        "create_metarecord" => {
            tx.execute(
                "INSERT INTO metarecord (uuid, version) VALUES (?1, 0)",
                params![db::uuid_to_bytes(entity)],
            )?;
            for row in snapshots(tx, op.id, 1)? {
                db::insert_field_row(tx, entity, &row.name, &row.value, Some(row.id))?;
            }
        }
        "delete_metarecord" => {
            tx.execute(
                "DELETE FROM metarecord WHERE uuid = ?1",
                params![db::uuid_to_bytes(entity)],
            )?;
        }
        "set_metarecord" => {
            tx.execute(
                "DELETE FROM field WHERE metarecord_uuid = ?1",
                params![db::uuid_to_bytes(entity)],
            )?;
            for row in snapshots(tx, op.id, 1)? {
                db::insert_field_row(tx, entity, &row.name, &row.value, Some(row.id))?;
            }
        }
        "set_field" | "file_deleted" | "file_moved" | "file_modified" => {
            let field = op.field_name.as_deref().context("set-shaped op without field_name")?;
            tx.prepare_cached("DELETE FROM field WHERE metarecord_uuid = ?1 AND field_name = ?2")?
                .execute(params![db::uuid_to_bytes(entity), field])?;
            for row in snapshots(tx, op.id, 1)? {
                db::insert_field_row(tx, entity, &row.name, &row.value, Some(row.id))?;
            }
        }
        "append_field" => {
            for row in snapshots(tx, op.id, 1)? {
                db::insert_field_row(tx, entity, &row.name, &row.value, Some(row.id))?;
            }
        }
        "delete_field" => {
            for row in snapshots(tx, op.id, 0)? {
                tx.execute("DELETE FROM field WHERE id = ?1", params![row.id])?;
            }
        }
        "unknown" => anyhow::bail!("cannot navigate across an 'unknown' operation (op {})", op.id),
        other => anyhow::bail!("unsupported op_type '{other}' in the log"),
    }
    // The version is not restored from the log: it is a function of the rows
    // this step has just put back (spec-event-log "Field ID and version
    // stability"). `entity_version_before`/`after` are provenance, and nothing
    // reads them to decide what to write.
    resync_version(tx, entity)
}

// ── Pruning (spec-event-log "Log pruning") ────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub enum PruneMode {
    Before,
    Linearize,
}

/// Permanently removes operations. The target must be an ancestor of HEAD
/// (or HEAD itself). Returns (pruned operations, pruned revisions).
pub fn prune(
    conn: &mut rusqlite::Connection,
    mode: PruneMode,
    target: i64,
) -> Result<(usize, usize)> {
    let head = get_head(conn)?.context("cannot prune an empty history")?;
    let head_path = ancestry(conn, head)?;
    if !head_path.contains(&target) {
        anyhow::bail!("prune target {target} must be an ancestor of HEAD (or HEAD itself)");
    }

    let ops = all_ops(conn)?;
    let mut children: HashMap<Option<i64>, Vec<i64>> = HashMap::new();
    for op in &ops {
        children.entry(op.parent_id).or_default().push(op.id);
    }
    let subtree = |roots: Vec<i64>| -> HashSet<i64> {
        let mut set = HashSet::new();
        let mut stack = roots;
        while let Some(id) = stack.pop() {
            if set.insert(id) {
                stack.extend(children.get(&Some(id)).into_iter().flatten().copied());
            }
        }
        set
    };

    let to_delete: HashSet<i64> = match mode {
        PruneMode::Before => {
            // Keep the target and everything below it; drop the rest.
            let keep = subtree(vec![target]);
            ops.iter().map(|o| o.id).filter(|id| !keep.contains(id)).collect()
        }
        PruneMode::Linearize => {
            // Drop branches diverging from the HEAD path strictly before the
            // target (the segment root→target becomes a straight line).
            let path_set: HashSet<i64> = head_path.iter().copied().collect();
            let target_pos = head_path.iter().position(|id| *id == target).unwrap();
            // head_path is head→root: nodes strictly before the target are
            // the ones after target_pos in that ordering.
            let mut branch_roots = Vec::new();
            for node in &head_path[target_pos + 1..] {
                for child in children.get(&Some(*node)).into_iter().flatten() {
                    if !path_set.contains(child) {
                        branch_roots.push(*child);
                    }
                }
            }
            subtree(branch_roots)
        }
    };

    let revisions_before: i64 =
        conn.query_row("SELECT COUNT(*) FROM revision", [], |r| r.get(0))?;
    let tx = conn.transaction()?;
    if matches!(mode, PruneMode::Before) {
        tx.execute("UPDATE operation SET parent_id = NULL WHERE id = ?1", params![target])?;
    }
    // Children reference their parent (FK): delete newest-first, which is
    // child-before-parent since ids are monotonically increasing.
    let mut ordered: Vec<i64> = to_delete.iter().copied().collect();
    ordered.sort_unstable_by(|a, b| b.cmp(a));
    {
        let mut stmt = tx.prepare_cached("DELETE FROM operation WHERE id = ?1")?;
        for id in ordered {
            stmt.execute(params![id])?;
        }
    }
    tx.execute("DELETE FROM revision WHERE id NOT IN (SELECT DISTINCT rev_id FROM operation)", [])?;
    tx.commit()?;
    let revisions_after: i64 = conn.query_row("SELECT COUNT(*) FROM revision", [], |r| r.get(0))?;

    // Return the freed pages to the filesystem (best-effort): the deleted
    // snapshots would otherwise keep the file at its high-water size
    // (spec-event-log "Log pruning"). The deletion above is already committed,
    // so a VACUUM failure (no room for its temp copy, a read-only filesystem…)
    // must not turn a successful prune into an error — it only defers the
    // space reclaim to a later prune.
    compact_best_effort(conn);

    Ok((to_delete.len(), (revisions_before - revisions_after) as usize))
}

/// Compacts the database to release freed pages, best-effort. Returns whether
/// it succeeded; a failure is logged, not propagated, because the caller's
/// write is already committed (see [`prune`]).
fn compact_best_effort(conn: &rusqlite::Connection) -> bool {
    match conn.execute_batch("VACUUM; PRAGMA wal_checkpoint(TRUNCATE);") {
        Ok(()) => true,
        Err(e) => {
            crate::diagnostics::warn(
                "prune",
                format!("could not compact the database after prune: {e}"),
            );
            false
        }
    }
}

/// Multi-row INSERT in chunks. `insert_sql` is the statement up to (and
/// excluding) the VALUES clause; every row must have `row_width` parameters.
fn bulk_insert(
    tx: &Transaction<'_>,
    insert_sql: &str,
    row_width: usize,
    rows: &[Vec<rusqlite::types::Value>],
) -> Result<()> {
    // Stay well under SQLITE_MAX_VARIABLE_NUMBER (32766 for bundled SQLite).
    const MAX_PARAMS: usize = 16_000;
    let rows_per_chunk = (MAX_PARAMS / row_width).max(1);
    let row_placeholder = format!("({})", vec!["?"; row_width].join(", "));
    for chunk in rows.chunks(rows_per_chunk) {
        let placeholders = vec![row_placeholder.as_str(); chunk.len()].join(", ");
        let sql = format!("{insert_sql} VALUES {placeholders}");
        tx.execute(&sql, rusqlite::params_from_iter(chunk.iter().flatten()))?;
    }
    Ok(())
}

/// One buffered operation, written to `operation`/`op_snapshot` in bulk
/// (spec-event-log "Normal write flow": for batch operations all operation
/// rows are inserted together after computing the parent chain).
struct PendingOp {
    op_type: OpType,
    entity: Uuid,
    field_name: Option<String>,
    version_before: Option<u64>,
    version_after: Option<u64>,
    before: Vec<FieldRow>,
    after: Vec<FieldRow>,
    /// The operation this one undoes, when the writer is reverting.
    reverts_op_id: Option<i64>,
}

/// Buffered operations are flushed to the database once this many accumulate,
/// keeping the Writer's memory bounded on huge revisions (e.g. reconcile).
pub const FLUSH_THRESHOLD: usize = 4096;

/// One position of a TreeRef field: the metarecord it hangs under (`None` for a
/// root of the forest), the name component it contributes, and the `field` row
/// that holds it.
///
/// The row id is not decoration: a metarecord's positions are ordered by it —
/// a load reads them that way, and the first one is where the forest hangs this
/// metarecord's children — so an operation that puts a position *back* under
/// its original id (a navigation, an edit by row id) has to be settled at that
/// place in the order, not at the end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreePos {
    pub row: i64,
    pub parent: Option<Uuid>,
    pub name: metafolder_core::metarecord::TreeName,
}

/// The row id of a position whose producer does not know one: the watcher's
/// incremental upkeep, which works from filesystem events and not from rows.
/// Sorts last, so it never displaces a position that has a real id — which is
/// all that is asked of it, `mfr_path` holding one position per metarecord.
pub const UNKNOWN_ROW: i64 = i64::MAX;

/// What one operation does to one `(field name, metarecord)` cell of the
/// forest — all the tree cache needs to follow a write, taken from the rows the
/// operation moved rather than read back from the database afterwards.
///
/// There is one variant per *shape* of operation, not per op type: a field set,
/// a record created, replaced or deleted all say "the cell now holds exactly
/// this", while an append and a field deletion name only the positions they
/// move and leave the rest of the cell alone.
#[derive(Debug, Clone, PartialEq)]
pub enum TreeOp {
    /// The cell holds exactly these positions now — `[]` when the write took
    /// its last one away, or gave the field another type.
    Set { field: String, uuid: Uuid, positions: Vec<TreePos> },
    /// These join whatever the cell already holds.
    Add { field: String, uuid: Uuid, positions: Vec<TreePos> },
    /// These leave it; the cell keeps the rest.
    Remove { field: String, uuid: Uuid, positions: Vec<TreePos> },
}

impl TreeOp {
    pub fn field(&self) -> &str {
        match self {
            TreeOp::Set { field, .. }
            | TreeOp::Add { field, .. }
            | TreeOp::Remove { field, .. } => field,
        }
    }

    pub fn uuid(&self) -> Uuid {
        match self {
            TreeOp::Set { uuid, .. } | TreeOp::Add { uuid, .. } | TreeOp::Remove { uuid, .. } => {
                *uuid
            }
        }
    }
}

/// The TreeRef positions among `rows`, paired with the field name each belongs
/// to, in row order.
fn tree_positions(rows: &[FieldRow]) -> Vec<(&str, TreePos)> {
    rows.iter()
        .filter_map(|row| match &row.value {
            Value::TreeRef { parent, name } => Some((
                row.name.as_str(),
                TreePos { row: row.id, parent: *parent, name: name.clone() },
            )),
            _ => None,
        })
        .collect()
}

/// What one operation does to the forest, from the rows it moved.
///
/// This is the single description of an operation's effect on the tree, shared
/// by the two producers that have one: a [`Writer`], which calls it as it
/// records each operation, and the coordinated navigation, which calls it on an
/// operation read back from the log ([`inverse_tree_ops`]).
pub fn tree_ops_of(
    op_type: OpType,
    before: &[FieldRow],
    after: &[FieldRow],
    entity: Uuid,
) -> Vec<TreeOp> {
    let group = |rows: &[FieldRow]| -> Vec<(String, Vec<TreePos>)> {
        let mut out: Vec<(String, Vec<TreePos>)> = Vec::new();
        for (field, pos) in tree_positions(rows) {
            match out.iter_mut().find(|(f, _)| f == field) {
                Some((_, positions)) => positions.push(pos),
                None => out.push((field.to_string(), vec![pos])),
            }
        }
        out
    };
    match op_type {
        // These two name the rows they move and nothing else.
        OpType::AppendField => group(after)
            .into_iter()
            .map(|(field, positions)| TreeOp::Add { field, uuid: entity, positions })
            .collect(),
        OpType::DeleteField => group(before)
            .into_iter()
            .map(|(field, positions)| TreeOp::Remove { field, uuid: entity, positions })
            .collect(),
        // Every other shape replaces whole cells: `after` is what each of them
        // holds now. A field that only appears in `before` had its last
        // position taken away, and is replaced by nothing.
        _ => {
            let mut settled = group(after);
            for (field, _) in group(before) {
                if !settled.iter().any(|(f, _)| *f == field) {
                    settled.push((field, Vec::new()));
                }
            }
            settled
                .into_iter()
                .map(|(field, positions)| TreeOp::Set { field, uuid: entity, positions })
                .collect()
        }
    }
}

/// What *undoing* one operation does to the forest: the same description, with
/// the two sides of the snapshot exchanged. An append undone is a removal and a
/// deletion undone is an append; every other shape is symmetric already, since
/// it reads only the side it lands on.
pub fn inverse_tree_ops(
    op_type: OpType,
    before: &[FieldRow],
    after: &[FieldRow],
    entity: Uuid,
) -> Vec<TreeOp> {
    let flipped = match op_type {
        OpType::AppendField => OpType::DeleteField,
        OpType::DeleteField => OpType::AppendField,
        other => other,
    };
    tree_ops_of(flipped, after, before, entity)
}

/// What a committed revision obliges its caller to bring back in step — the
/// in-memory state a transaction cannot update itself (see
/// `RepoState::settle`). Read off the writer *before* `commit` consumes it.
#[derive(Debug, Default, Clone)]
pub struct WriteEffects {
    /// What the revision did to the forest, one entry per operation that moved
    /// a position, in write order. Never truncated: the cache settles a batch
    /// of any size, and a revision that changed the whole forest is exactly the
    /// one whose cache upkeep must not be guessed at.
    ///
    /// Not deduplicated either, unlike the cells this replaced: an `Add` and a
    /// `Remove` say what they move, so two operations on one cell are two
    /// changes and the order they are applied in is the order they were
    /// written in.
    tree: Vec<TreeOp>,
    /// Whether the revision wrote a field that decides which directories are
    /// watched.
    watch: bool,
}

impl WriteEffects {
    /// True if any `tree_ref` field row was created or removed. Manual API
    /// writes do not go through the watcher's incremental tree-cache upkeep, so
    /// a caller holding a complete cache must settle it after such a write.
    pub fn touches_tree(&self) -> bool {
        !self.tree.is_empty()
    }

    /// What the revision did to the forest, in write order.
    pub fn tree_ops(&self) -> &[TreeOp] {
        &self.tree
    }

    /// True if the revision wrote `mf_watch` / `mf_ignore` (eligibility) or
    /// `mfr_watch_exceeded` (the watch budget). A caller that keeps a live
    /// inotify watch set must refresh it: the set of watched directories may
    /// have changed.
    pub fn touches_watch(&self) -> bool {
        self.watch
    }
}

// ── Automatic retention (spec-event-log "Automatic retention") ────────────────

/// How much history the log keeps behind HEAD. Applied by [`Writer::commit`],
/// inside the write's own transaction: a trim is a range delete over the
/// oldest operations, which costs less than the append that triggered it and
/// adds no `fsync` of its own.
///
/// Deliberately *not* a [`prune`]: no `VACUUM`. In a steady state the freed
/// pages are what the next appends write into, so the database settles on a
/// plateau instead of being rewritten whole at every trim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    /// Revisions to keep. `0` = unlimited: nothing is ever dropped.
    pub revisions: u64,
    /// Never trim past a labelled revision: a named checkpoint is a floor, and
    /// the log grows past the limit rather than losing it.
    pub keep_labels: bool,
}

impl Retention {
    /// Keep everything. Deliberately not a `Default` impl: the default a
    /// repository actually writes with is the configured one
    /// (`DEFAULT_LOG_RETENTION_REVISIONS`), and a silent `Retention::default()`
    /// meaning "unlimited" would read as agreeing with it.
    pub const UNLIMITED: Self = Self { revisions: 0, keep_labels: false };

    /// How far past the limit the log is allowed to drift before a trim runs.
    /// Hysteresis, not sloppiness: trimming one revision per write would dirty
    /// the oldest (cold) pages of the log at every single commit, where one
    /// trim per `slack` revisions costs the same work spread over `slack`
    /// transactions that were going to write anyway.
    pub fn slack(revisions: u64) -> u64 {
        (revisions / 10).max(1)
    }

    fn enabled(&self) -> bool {
        self.revisions > 0
    }
}

/// Drops the revisions that fall outside `retention`, oldest first. Returns the
/// number of operations removed.
///
/// The log is a tree, so "the oldest" is not an id range: a branch rooted below
/// the cut loses its ancestry and must go with it. The doomed set is therefore
/// the operations older than the cutoff *plus their descendants*, and the
/// cutoff is severed from its parent first so that the surviving history is not
/// itself reachable from the set.
fn trim(tx: &Transaction<'_>, retention: Retention, head: i64) -> Result<usize> {
    if !retention.enabled() {
        return Ok(0);
    }
    let kept: u64 =
        tx.query_row("SELECT COUNT(*) FROM revision", [], |r| r.get::<_, i64>(0))? as u64;
    if kept <= retention.revisions + Retention::slack(retention.revisions) {
        return Ok(0);
    }

    // The oldest revision to keep: the `revisions`-th newest.
    let mut keep_from: i64 = tx.query_row(
        "SELECT id FROM revision ORDER BY id DESC LIMIT 1 OFFSET ?1",
        params![retention.revisions as i64 - 1],
        |r| r.get(0),
    )?;
    if retention.keep_labels {
        let oldest_label: Option<i64> =
            tx.query_row("SELECT MIN(id) FROM revision WHERE label IS NOT NULL", [], |r| r.get(0))?;
        if let Some(label) = oldest_label {
            keep_from = keep_from.min(label);
        }
    }
    // First operation of that revision: the cutoff, and the log's new root.
    let cutoff: Option<i64> =
        tx.query_row("SELECT MIN(id) FROM operation WHERE rev_id = ?1", params![keep_from], |r| {
            r.get(0)
        })?;
    let Some(cutoff) = cutoff else { return Ok(0) };
    let oldest: Option<i64> = tx.query_row("SELECT MIN(id) FROM operation", [], |r| r.get(0))?;
    if oldest == Some(cutoff) {
        return Ok(0);
    }

    let savepoint = "log_trim";
    tx.execute_batch(&format!("SAVEPOINT {savepoint}"))?;
    let outcome = trim_at(tx, cutoff, head);
    match outcome {
        Ok(Some(pruned)) => {
            tx.execute_batch(&format!("RELEASE {savepoint}"))?;
            Ok(pruned)
        }
        Ok(None) => {
            // The cutoff is not on HEAD's line of history — the newest
            // revisions sit on a branch abandoned by a rollback. Cutting there
            // would delete the history HEAD stands on, so decline: retention
            // resumes on its own once the current line is again the newest.
            tx.execute_batch(&format!("ROLLBACK TO {savepoint}; RELEASE {savepoint}"))?;
            Ok(0)
        }
        Err(e) => {
            let _ = tx.execute_batch(&format!("ROLLBACK TO {savepoint}; RELEASE {savepoint}"));
            Err(e)
        }
    }
}

/// Makes `cutoff` the new root and deletes everything that no longer hangs off
/// it. `Ok(None)` when `cutoff` is not an ancestor of `head` (nothing done).
fn trim_at(tx: &Transaction<'_>, cutoff: i64, head: i64) -> Result<Option<usize>> {
    // Sever the cutoff first: the walk below descends from the operations older
    // than it, and would otherwise reach the whole surviving history through it.
    tx.execute("UPDATE operation SET parent_id = NULL WHERE id = ?1", params![cutoff])?;
    if !ancestry(tx, head)?.contains(&cutoff) {
        return Ok(None);
    }

    tx.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS log_trim_doomed (id INTEGER PRIMARY KEY);
         DELETE FROM log_trim_doomed;",
    )?;
    tx.execute(
        "INSERT INTO log_trim_doomed (id)
         WITH RECURSIVE doomed(id) AS (
             SELECT id FROM operation WHERE id < ?1
             UNION
             SELECT o.id FROM operation o JOIN doomed d ON o.parent_id = d.id)
         SELECT id FROM doomed",
        params![cutoff],
    )?;
    // Revisions to reconsider afterwards: collected before the operations that
    // name them are gone.
    tx.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS log_trim_revs (id INTEGER PRIMARY KEY);
         DELETE FROM log_trim_revs;",
    )?;
    tx.execute(
        "INSERT INTO log_trim_revs (id) SELECT DISTINCT rev_id FROM operation \
         WHERE id IN (SELECT id FROM log_trim_doomed)",
        [],
    )?;
    // One statement, so SQLite checks the self-referencing foreign key once at
    // its end rather than per row — a parent and its child may go together.
    // `op_snapshot` follows by cascade.
    let pruned =
        tx.execute("DELETE FROM operation WHERE id IN (SELECT id FROM log_trim_doomed)", [])?;
    tx.execute(
        "DELETE FROM revision WHERE id IN (SELECT id FROM log_trim_revs) \
         AND NOT EXISTS (SELECT 1 FROM operation WHERE rev_id = revision.id)",
        [],
    )?;
    tx.execute_batch("DELETE FROM log_trim_doomed; DELETE FROM log_trim_revs;")?;
    Ok(Some(pruned))
}

/// A single logged write transaction. All changes made through one Writer
/// form one revision; commit is atomic. Dropping a Writer without committing
/// rolls everything back. After any method returns an error, the Writer must
/// be dropped (the whole revision is abandoned).
///
/// Data-table changes are applied immediately (later changes and lookups in
/// the same revision observe them); the log rows are buffered and inserted
/// in bulk, in batches of [`FLUSH_THRESHOLD`] operations.
pub struct Writer<'c> {
    tx: Transaction<'c>,
    rev_id: i64,
    /// Parent of the next operation to flush: HEAD as of `begin`, then the
    /// last flushed operation.
    chain_head: Option<i64>,
    /// Number of operations already flushed to the database.
    flushed: i64,
    pending: Vec<PendingOp>,
    /// Per-revision cache of each field name's established (non-`Nothing`) value
    /// type, populated lazily on the first checked write of a name. Collapses the
    /// per-write type probe to one DB seek per field name (a bulk reconcile/watcher
    /// revision writes the same ~8 reserved names across thousands of records).
    field_types: HashMap<String, String>,
    /// Field names whose type check this revision deferred to commit, and the
    /// reason it may: see [`Self::defer_type_checks`]. Empty for every ordinary
    /// write, which is checked as it happens.
    deferred_types: Option<HashSet<String>>,
    /// How much history to keep behind HEAD; applied on commit.
    retention: Retention,
    /// What this revision will oblige its caller to refresh, accumulated as the
    /// operations are recorded rather than read back off `pending` — which the
    /// flush above empties, so a long revision used to forget its early writes.
    effects: WriteEffects,
    /// The cells a *manual* operation took a `tree_ref` row from, checked once
    /// at commit (see [`Self::check_forest_integrity`]). Deduplicated through
    /// its own index — a watcher flush writes one `mfr_path` op per file, and
    /// scanning the list each time made a batch quadratic in itself; the list
    /// keeps write order for a stable error.
    tree_lost: Vec<(String, Uuid)>,
    tree_lost_seen: HashMap<String, HashSet<Uuid>>,
    /// The operation the writes recorded from now on are undoing, stamped onto
    /// each of them as `reverts_op_id` (see [`Self::reverting`]). `None` for
    /// every ordinary write.
    reverting: Option<i64>,
}

impl<'c> Writer<'c> {
    /// Opens a transaction and creates the revision row, keeping the whole
    /// history. Every write of a *loaded* repository goes through
    /// [`crate::state::RepoState::writer`] instead, which applies the
    /// repository's configured retention.
    pub fn begin(conn: &'c mut rusqlite::Connection, label: Option<String>) -> Result<Self> {
        Self::begin_with_retention(conn, label, Retention::UNLIMITED)
    }

    /// Opens a transaction and creates the revision row, dropping the history
    /// that falls outside `retention` when the revision is committed.
    pub fn begin_with_retention(
        conn: &'c mut rusqlite::Connection,
        label: Option<String>,
        retention: Retention,
    ) -> Result<Self> {
        let tx = conn.transaction()?;
        let head: Option<i64> =
            tx.query_row("SELECT op_id FROM log_head WHERE singleton = 1", [], |r| r.get(0))?;
        tx.execute(
            "INSERT INTO revision (timestamp, label) VALUES (?1, ?2)",
            params![now_ms(), label],
        )?;
        let rev_id = tx.last_insert_rowid();
        Ok(Self {
            tx,
            rev_id,
            chain_head: head,
            flushed: 0,
            pending: Vec::new(),
            field_types: HashMap::new(),
            deferred_types: None,
            retention,
            effects: WriteEffects::default(),
            tree_lost: Vec::new(),
            tree_lost_seen: HashMap::new(),
            reverting: None,
        })
    }

    pub fn rev_id(&self) -> i64 {
        self.rev_id
    }

    /// Marks this revision as written on the filesystem's behalf rather than at
    /// a client's request (spec-event-log "Revision origin"). The watcher's
    /// flush and the restoration replay set it; every other write leaves it
    /// unset, which is what "a client asked for this" means.
    ///
    /// It is the revision, not the operation types, that carries this: a file
    /// arriving is recorded as a `create_metarecord`, indistinguishable by type
    /// from a metarecord the user created.
    pub fn set_origin(&mut self, origin: &str) -> Result<()> {
        self.tx.execute(
            "UPDATE revision SET origin = ?1 WHERE id = ?2",
            params![origin, self.rev_id],
        )?;
        Ok(())
    }

    /// Read access to the underlying transaction, for lookups (tree cache,
    /// eligibility) that must observe the writes already applied.
    pub fn connection(&self) -> &rusqlite::Connection {
        &self.tx
    }

    /// Number of operations recorded so far in this revision.
    pub fn op_count(&self) -> i64 {
        self.flushed + self.pending.len() as i64
    }

    /// What this revision obliges its caller to bring back in step (tree cache,
    /// watch set). Read it before [`Self::commit`], which consumes the writer.
    pub fn effects(&self) -> WriteEffects {
        self.effects.clone()
    }

    /// Records what one operation implies: what its caller will have to bring
    /// back in step ([`WriteEffects`]), and which cells lost a `tree_ref` row
    /// and must therefore be checked at commit.
    fn observe_effects(
        &mut self,
        op_type: OpType,
        before: &[FieldRow],
        after: &[FieldRow],
        entity: Uuid,
    ) {
        for row in before {
            if op_type.is_manual()
                && matches!(row.value, Value::TreeRef { .. })
                && !self
                    .tree_lost_seen
                    .get(row.name.as_str())
                    .is_some_and(|seen| seen.contains(&entity))
            {
                self.tree_lost.push((row.name.clone(), entity));
                self.tree_lost_seen.entry(row.name.clone()).or_default().insert(entity);
            }
        }
        const DECIDES_WATCHES: &[&str] =
            &["mf_watch", "mf_ignore", crate::eligibility::WATCH_EXCEEDED];
        for row in before.iter().chain(after) {
            if DECIDES_WATCHES.contains(&row.name.as_str()) {
                self.effects.watch = true;
            }
        }
        self.effects.tree.extend(tree_ops_of(op_type, before, after, entity));
    }

    /// The other half of [`Self::validate_tree_ref`]: a `tree_ref` reference is
    /// refused when it *names* a parent carrying no position of its own, so a
    /// position may not be *removed* while metarecords are placed under it.
    ///
    /// Checked once, on the state the revision is about to commit, rather than
    /// per operation — deleting a whole subtree is legitimate even though the
    /// parent goes first, and only the committed state has to be a forest.
    ///
    /// Without it the children stayed, naming a node that no longer existed:
    /// *detached*, reachable by uuid but under no parent and in no roots map, so
    /// `GET /tree/roots` and path reconstruction disagreed about them ever after.
    /// Defers this revision's type checks to [`Self::commit`], where they are
    /// made against the state the revision lands on rather than against each
    /// intermediate one.
    ///
    /// Only a revert needs this, and only because of how it works: it undoes a
    /// set of operations one inverse at a time, newest to oldest, and the states
    /// in between need not be type-consistent even when the final one is.
    /// Undoing a `retype_field` is the case — after the last converted row's
    /// `append_field` is undone, the cell still holds the others in the *new*
    /// type. A rollback never meets this because navigation does not go through
    /// a `Writer` at all (see `apply_inverse`).
    ///
    /// Deferring is not skipping: [`Self::check_deferred_types`] refuses a
    /// revision that leaves two value types under one field name, which is what
    /// the per-write check was protecting.
    pub fn defer_type_checks(&mut self) {
        self.deferred_types.get_or_insert_with(Default::default);
        // The per-revision cache holds types probed under the eager rule; drop
        // it so nothing downstream reads a type this revision is about to move.
        self.field_types.clear();
    }

    /// Declares that the operations recorded from now on undo `op_id`, which
    /// each of them records as its `reverts_op_id` (spec-event-log "Revert").
    /// A revert sets it around each operation it walks; `None` restores the
    /// ordinary, unattributed write.
    pub fn reverting(&mut self, op_id: Option<i64>) {
        self.reverting = op_id;
    }

    /// The deferred check: every field name this revision wrote must carry a
    /// single value type across the repository (spec-data-model "One value type
    /// per field name").
    fn check_deferred_types(&self) -> Result<()> {
        let Some(names) = &self.deferred_types else { return Ok(()) };
        for name in names {
            let types = db::distinct_value_types(&self.tx, name)?;
            if types.len() > 1 {
                return Err(DomainError::BadRequest(format!(
                    "field '{name}' would be left with more than one value type ({}); \
                     the value types recorded under one field name must agree",
                    types.join(", ")
                ))
                .into());
            }
        }
        Ok(())
    }

    fn check_forest_integrity(&self) -> Result<()> {
        for (field_name, uuid) in &self.tree_lost {
            // Cheapest question first, and the one that is almost always "none":
            // a cell nobody is placed under has nothing to keep.
            let children = db::tree_children(&self.tx, field_name, *uuid)?;
            if children.is_empty() {
                continue;
            }
            if db::tree_position(&self.tx, field_name, *uuid)?.is_some() {
                continue;
            }
            let mut named: Vec<String> =
                children.iter().take(3).map(|(_, name)| format!("{name:?}")).collect();
            if children.len() > named.len() {
                named.push(format!("and {} more", children.len() - named.len()));
            }
            return Err(DomainError::BadRequest(format!(
                "cannot remove the last '{field_name}' position of {uuid}: {} metarecord(s) \
                 are placed under it ({}); move or delete them first",
                children.len(),
                named.join(", ")
            ))
            .into());
        }
        Ok(())
    }

    /// Removes every row of `(uuid, name)`, leaving the field unknown.
    /// Set-field shaped (before = all rows, after = none) so the standard
    /// `set_field` inverse applies. Used to invalidate `mfr_*` hashes.
    pub fn clear_field_as(&mut self, op_type: OpType, uuid: Uuid, name: &str) -> Result<()> {
        let before = db::get_field_rows_named(&self.tx, uuid, name)?;
        if before.is_empty() {
            return Ok(());
        }
        let version_before = self.current_version(uuid)?;
        self.tx
            .prepare_cached("DELETE FROM field WHERE metarecord_uuid = ?1 AND field_name = ?2")?
            .execute(params![db::uuid_to_bytes(uuid), name])?;
        self.log_op(op_type, uuid, Some(name), Some(version_before), before, vec![])?;
        self.field_types.remove(name); // rows removed: the type may have unlocked
        Ok(())
    }

    /// Creates a new metarecord owned by this repository.
    pub fn create_metarecord(&mut self, fields: Vec<Field>) -> Result<MetaRecord> {
        self.create_metarecord_with_uuid(Uuid::new_v4(), fields)
    }

    /// Like [`create_metarecord`] but at a caller-supplied UUID (sync
    /// bare-record creation, spec-sync). The UUID must not already exist — the
    /// `metarecord` PRIMARY KEY rejects a duplicate.
    pub fn create_metarecord_with_uuid(
        &mut self,
        uuid: Uuid,
        fields: Vec<Field>,
    ) -> Result<MetaRecord> {
        // Repeated (name, value) pairs are written once (spec-data-model
        // "No duplicate rows"); the record returned mirrors what is stored.
        let fields = collapse_duplicate_fields(fields);
        for f in &fields {
            self.validate_tree_ref(uuid, &f.name, &f.value)?;
        }
        // Seeded with the metarecord's own term; `log_op` then adds the terms
        // of the rows created below, so a fresh record's version describes its
        // initial fields like any other (spec-data-model "Version").
        self.tx
            .prepare_cached("INSERT INTO metarecord (uuid, version) VALUES (?1, ?2)")?
            .execute(params![db::uuid_to_bytes(uuid), version::base(uuid) as i64])?;

        let mut after = Vec::with_capacity(fields.len());
        let mut out_fields = Vec::with_capacity(fields.len());
        for f in fields {
            // Checked inside the loop so two rows of the same name with different
            // types within one create are rejected (the second sees the first).
            self.validate_value_type(&f.name, &f.value)?;
            let id = db::insert_field_row(&self.tx, uuid, &f.name, &f.value, None)?;
            after.push(FieldRow { id, name: f.name.clone(), value: f.value.clone() });
            out_fields.push(Field { id: Some(id), ..f });
        }

        self.log_op(OpType::CreateRecord, uuid, None, None, vec![], after)?;
        let version = self.current_version(uuid)?;
        Ok(MetaRecord { uuid, version, fields: out_fields })
    }

    /// Deletes a metarecord and all its rows.
    pub fn delete_metarecord(&mut self, uuid: Uuid) -> Result<()> {
        let version = db::get_version(&self.tx, uuid)?
            .ok_or_else(|| DomainError::NotFound(format!("Metarecord not found: {uuid}")))?;
        let before = db::get_field_rows(&self.tx, uuid)?;
        self.tx
            .execute("DELETE FROM metarecord WHERE uuid = ?1", params![db::uuid_to_bytes(uuid)])?;
        self.log_op(OpType::DeleteRecord, uuid, None, Some(version), before, vec![])?;
        Ok(())
    }

    /// Replaces the *entire* field set of an existing metarecord, keeping its
    /// UUID, in one `SetRecord` operation (before = all old fields, after = the
    /// new set) — the whole-record analogue of create/delete. Literal overwrite:
    /// every old row is dropped, including reserved ones not in `fields`.
    pub fn set_record(&mut self, uuid: Uuid, fields: Vec<Field>) -> Result<MetaRecord> {
        let fields = collapse_duplicate_fields(fields); // spec-data-model "No duplicate rows"
        let version_before = self.current_version(uuid)?; // errors NotFound if absent
        let before = db::get_field_rows(&self.tx, uuid)?;
        self.tx
            .prepare_cached("DELETE FROM field WHERE metarecord_uuid = ?1")?
            .execute(params![db::uuid_to_bytes(uuid)])?;
        // The whole record's types are reset; drop cached locks so the new set
        // re-probes from a clean slate (validated against the post-delete state).
        for row in &before {
            self.field_types.remove(&row.name);
        }
        let mut after = Vec::with_capacity(fields.len());
        let mut out_fields = Vec::with_capacity(fields.len());
        for f in fields {
            self.validate_tree_ref(uuid, &f.name, &f.value)?;
            self.validate_value_type(&f.name, &f.value)?;
            let id = db::insert_field_row(&self.tx, uuid, &f.name, &f.value, None)?;
            after.push(FieldRow { id, name: f.name.clone(), value: f.value.clone() });
            out_fields.push(Field { id: Some(id), ..f });
        }
        self.log_op(OpType::SetRecord, uuid, None, Some(version_before), before, after)?;
        Ok(MetaRecord { uuid, version: version_before + 1, fields: out_fields })
    }

    /// Replaces all rows for `(uuid, name)` with a single value.
    pub fn set_field(&mut self, uuid: Uuid, name: &str, value: Value) -> Result<()> {
        self.set_field_as(OpType::SetField, uuid, name, value)
    }

    /// `set_field` recorded under a watcher-specific op type
    /// (`file_deleted`, `file_moved`, `file_modified`).
    pub fn set_field_as(
        &mut self,
        op_type: OpType,
        uuid: Uuid,
        name: &str,
        value: Value,
    ) -> Result<()> {
        self.validate_tree_ref(uuid, name, &value)?;
        self.validate_value_type(name, &value)?;
        let version_before = self.current_version(uuid)?;
        let before = db::get_field_rows_named(&self.tx, uuid, name)?;
        self.tx
            .prepare_cached("DELETE FROM field WHERE metarecord_uuid = ?1 AND field_name = ?2")?
            .execute(params![db::uuid_to_bytes(uuid), name])?;
        let cleared_to_nothing = matches!(value, Value::Nothing);
        let id = db::insert_field_row(&self.tx, uuid, name, &value, None)?;
        let after = vec![FieldRow { id, name: name.to_string(), value }];
        self.log_op(op_type, uuid, Some(name), Some(version_before), before, after)?;
        if cleared_to_nothing {
            // The only remaining row is Nothing: the type may have unlocked.
            self.field_types.remove(name);
        }
        Ok(())
    }

    /// Replaces all rows for `(uuid, name)` with the given set of values
    /// (multi-map), in a single `SetField` operation. The existing `set_field`
    /// inverse/forward arms already restore *all* snapshot rows, so this needs
    /// no navigation change.
    pub fn set_field_multi(&mut self, uuid: Uuid, name: &str, values: Vec<Value>) -> Result<()> {
        self.set_field_multi_as(OpType::SetField, uuid, name, values).map(|_| ())
    }

    /// [`Self::set_field_multi`] under a chosen op type, returning one row id
    /// per *given* value, in order. A revert needs both: the watcher types when
    /// it undoes a file event (spec-event-log "Revert content"), and the ids to
    /// remap the row-scoped inverses that follow it — so a repeated value, which
    /// is collapsed to a single row (spec-data-model "No duplicate rows"),
    /// reports the id of the row that swallowed it rather than shifting the
    /// caller's pairing.
    pub fn set_field_multi_as(
        &mut self,
        op_type: OpType,
        uuid: Uuid,
        name: &str,
        values: Vec<Value>,
    ) -> Result<Vec<i64>> {
        for value in &values {
            self.validate_tree_ref(uuid, name, value)?;
            self.validate_value_type(name, value)?;
        }
        // Each value is written once; a repeat points back at its first
        // occurrence so the returned ids still pair with `values`.
        let (kept, slot) = collapse_duplicates(&values);
        let version_before = self.current_version(uuid)?;
        let before = db::get_field_rows_named(&self.tx, uuid, name)?;
        self.tx
            .prepare_cached("DELETE FROM field WHERE metarecord_uuid = ?1 AND field_name = ?2")?
            .execute(params![db::uuid_to_bytes(uuid), name])?;
        let cleared_to_nothing = values.iter().all(|v| matches!(v, Value::Nothing));
        let mut after = Vec::with_capacity(kept.len());
        for value in kept {
            let id = db::insert_field_row(&self.tx, uuid, name, &value, None)?;
            after.push(FieldRow { id, name: name.to_string(), value });
        }
        let ids = slot.iter().map(|&i| after[i].id).collect();
        self.log_op(op_type, uuid, Some(name), Some(version_before), before, after)?;
        if cleared_to_nothing {
            // No non-Nothing row remains: the type may have unlocked.
            self.field_types.remove(name);
        }
        Ok(ids)
    }

    /// Appends one row without touching existing rows of that name.
    /// A value the metarecord already holds under that name is *not* appended
    /// again (spec-data-model "No duplicate rows"): nothing is written, nothing
    /// is logged, the version does not move, and the row that already holds it
    /// is reported as [`Appended::AlreadyPresent`].
    pub fn append_field(&mut self, uuid: Uuid, name: &str, value: Value) -> Result<Appended> {
        self.validate_tree_ref(uuid, name, &value)?;
        self.validate_value_type(name, &value)?;
        if let Some(existing) = db::duplicate_row_id(&self.tx, uuid, name, &value)? {
            return Ok(Appended::AlreadyPresent(existing));
        }
        let version_before = self.current_version(uuid)?;
        let id = db::insert_field_row(&self.tx, uuid, name, &value, None)?;
        let after = vec![FieldRow { id, name: name.to_string(), value }];
        self.log_op(OpType::AppendField, uuid, Some(name), Some(version_before), vec![], after)?;
        Ok(Appended::Created(id))
    }

    /// Replaces the single row identified by `field_id`, keeping its row id.
    /// Logged as a `delete_field` + `append_field` pair so that the inverse
    /// operations remain row-scoped (a `set_field` snapshot covers *all* rows
    /// of the name, which would clobber untouched sibling rows on rollback).
    pub fn replace_field(&mut self, uuid: Uuid, field_id: i64, value: Value) -> Result<()> {
        let old = self.get_owned_row(uuid, field_id)?;
        let name = old.name.clone();
        self.validate_tree_ref(uuid, &name, &value)?;
        self.validate_value_type(&name, &value)?;
        self.reject_duplicate(uuid, field_id, &name, &value)?;
        self.replace_owned_row(uuid, old, &name, value)
    }

    /// A by-id edit names one specific row, so turning it into the twin of a
    /// sibling is refused rather than silently dropping the row the caller just
    /// addressed (spec-data-model "No duplicate rows"). The row being edited is
    /// not its own twin.
    fn reject_duplicate(&self, uuid: Uuid, field_id: i64, name: &str, value: &Value) -> Result<()> {
        match self.twin_row(uuid, field_id, name, value)? {
            Some(id) => Err(DomainError::BadRequest(format!(
                "duplicate value for field '{name}': row {id} of {uuid} already holds it"
            ))
            .into()),
            None => Ok(()),
        }
    }

    /// The id of *another* row of `(uuid, name)` already holding `value`, if
    /// any — what makes the row `field_id` a duplicate.
    fn twin_row(
        &self,
        uuid: Uuid,
        field_id: i64,
        name: &str,
        value: &Value,
    ) -> Result<Option<i64>> {
        Ok(db::get_field_rows_named(&self.tx, uuid, name)?
            .into_iter()
            .find(|r| r.id != field_id && &r.value == value)
            .map(|r| r.id))
    }

    /// Changes a field row's *name and/or value* in place, keeping its id — the
    /// by-id edit behind `mf field set`. The value type is validated against the
    /// *target* field name (a rename moves the row into that name's type group).
    pub fn rename_field(
        &mut self,
        uuid: Uuid,
        field_id: i64,
        new_name: &str,
        value: Value,
    ) -> Result<()> {
        let old = self.get_owned_row(uuid, field_id)?;
        self.validate_tree_ref(uuid, new_name, &value)?;
        self.validate_value_type(new_name, &value)?;
        self.reject_duplicate(uuid, field_id, new_name, &value)?;
        self.replace_owned_row(uuid, old, new_name, value)
    }

    /// The logged delete+append core of [`Self::replace_field`], without the
    /// invariant checks. Shared with [`Self::retype_field`], which is itself the
    /// authority that changes a field's established type (so it must not be
    /// rejected by the per-write type check while converting row by row).
    /// `new_name` may differ from `old.name` (a by-id rename).
    fn replace_owned_row(
        &mut self,
        uuid: Uuid,
        old: FieldRow,
        new_name: &str,
        value: Value,
    ) -> Result<()> {
        let field_id = old.id;
        let v1 = self.current_version(uuid)?;
        self.tx.execute("DELETE FROM field WHERE id = ?1", params![field_id])?;
        self.log_op(
            OpType::DeleteField,
            uuid,
            Some(&old.name.clone()),
            Some(v1),
            vec![old.clone()],
            vec![],
        )?;

        let v2 = self.current_version(uuid)?;
        db::insert_field_row(&self.tx, uuid, new_name, &value, Some(field_id))?;
        let after = vec![FieldRow { id: field_id, name: new_name.to_string(), value }];
        self.log_op(OpType::AppendField, uuid, Some(new_name), Some(v2), vec![], after)?;
        if new_name != old.name {
            // A rename can unlock the old name's type and lock the new one.
            self.field_types.remove(&old.name);
            self.field_types.remove(new_name);
        }
        Ok(())
    }

    /// Converts every non-`Nothing` row of field `name` to the type `to`,
    /// repository-wide, in this one revision (spec-data-model "Changing a field's
    /// type"). The target may be any type, including the reference variants.
    /// Row-scoped (each row keeps its id, so rollback restores it exactly) and
    /// bypasses the per-write type check (it *is* the type change). `Nothing`
    /// rows are left untouched (explicit absence is preserved). A `String →
    /// TreeRef` result that would violate the forest (missing parent, cycle,
    /// depth) is demoted to `Nothing` rather than aborting the whole retype, so
    /// one bad path never blocks the rest. Returns how many rows changed and the
    /// metarecords whose values fell back to the sentinel.
    pub fn retype_field(&mut self, name: &str, to: FieldType) -> Result<RetypeSummary> {
        // The field's established type changes here; drop any cached entry so a
        // later checked write in this revision re-probes.
        self.field_types.remove(name);
        let mut converted = 0usize;
        let mut fallback: std::collections::BTreeSet<Uuid> = Default::default();
        for uuid in db::metarecords_with_field(&self.tx, name)? {
            for row in db::get_field_rows_named(&self.tx, uuid, name)? {
                if matches!(row.value, Value::Nothing) {
                    continue;
                }
                let (mut new_value, mut fell_back) = row.value.convert_to(to);
                // A converted TreeRef must satisfy the forest invariants like any
                // other write; a violating value is demoted to the Nothing
                // sentinel (and reported) so the retype as a whole still succeeds.
                if matches!(new_value, Value::TreeRef { .. }) {
                    if let Err(e) = self.validate_tree_ref(uuid, &row.name, &new_value) {
                        if e.downcast_ref::<DomainError>().is_some() {
                            new_value = Value::Nothing;
                            fell_back = true;
                        } else {
                            return Err(e); // a genuine DB error, not a forest violation
                        }
                    }
                }
                if new_value == row.value {
                    continue; // already the target type
                }
                if fell_back {
                    fallback.insert(uuid);
                }
                let name = row.name.clone();
                // Two values the conversion made equal are one row afterwards,
                // not a duplicate (spec-data-model "No duplicate rows"). The
                // probe reads the transaction's own state, so rows converted
                // earlier in this pass count as siblings.
                if self.twin_row(uuid, row.id, &name, &new_value)?.is_some() {
                    self.delete_field(uuid, row.id)?;
                } else {
                    self.replace_owned_row(uuid, row, &name, new_value)?;
                }
                converted += 1;
            }
        }
        Ok(RetypeSummary { converted, fallback_uuids: fallback.into_iter().collect() })
    }

    /// Removes the single row identified by `field_id`.
    pub fn delete_field(&mut self, uuid: Uuid, field_id: i64) -> Result<()> {
        let old = self.get_owned_row(uuid, field_id)?;
        let version_before = self.current_version(uuid)?;
        self.tx.execute("DELETE FROM field WHERE id = ?1", params![field_id])?;
        self.log_op(
            OpType::DeleteField,
            uuid,
            Some(&old.name.clone()),
            Some(version_before),
            vec![old.clone()],
            vec![],
        )?;
        self.field_types.remove(&old.name); // a row removed: the type may have unlocked
        Ok(())
    }

    /// Deletes the given rows of `(uuid, name)` and logs them as *one*
    /// `DeleteField` operation (before = the rows, after = empty), so a bulk
    /// removal is one reversible log line per metarecord rather than one per row.
    /// Returns the number removed; logs nothing when `rows` is empty.
    fn delete_field_rows(&mut self, uuid: Uuid, name: &str, rows: Vec<FieldRow>) -> Result<usize> {
        if rows.is_empty() {
            return Ok(0);
        }
        let version_before = self.current_version(uuid)?;
        for row in &rows {
            self.tx.execute("DELETE FROM field WHERE id = ?1", params![row.id])?;
        }
        let removed = rows.len();
        self.log_op(OpType::DeleteField, uuid, Some(name), Some(version_before), rows, vec![])?;
        self.field_types.remove(name); // rows removed: the type may have unlocked
        Ok(removed)
    }

    /// Removes every row of `(uuid, name)` whose value equals `value` — the
    /// inverse of [`Self::append_field`] — in one operation. Returns the count.
    pub fn delete_fields_valued(&mut self, uuid: Uuid, name: &str, value: &Value) -> Result<usize> {
        let rows: Vec<FieldRow> = db::get_field_rows_named(&self.tx, uuid, name)?
            .into_iter()
            .filter(|r| &r.value == value)
            .collect();
        self.delete_field_rows(uuid, name, rows)
    }

    /// Removes the field *entirely* — every row of `(uuid, name)`, whatever its
    /// value — in one operation, leaving the field unknown (absent).
    pub fn delete_fields_named(&mut self, uuid: Uuid, name: &str) -> Result<usize> {
        let rows = db::get_field_rows_named(&self.tx, uuid, name)?;
        self.delete_field_rows(uuid, name, rows)
    }

    /// Flushes the remaining buffered operations, writes the final HEAD and
    /// commits the transaction.
    pub fn commit(mut self) -> Result<()> {
        self.check_forest_integrity()?;
        self.check_deferred_types()?;
        if self.flushed == 0 && self.pending.is_empty() {
            // Nothing was written: drop the empty revision, leave HEAD alone.
            self.tx.execute("DELETE FROM revision WHERE id = ?1", params![self.rev_id])?;
        } else {
            self.flush_pending()?;
            self.tx.execute(
                "UPDATE log_head SET op_id = ?1 WHERE singleton = 1",
                params![self.chain_head],
            )?;
            if let Some(head) = self.chain_head {
                // In this transaction, deliberately: the trim then rides the
                // commit the write was going to pay for anyway.
                trim(&self.tx, self.retention, head)?;
            }
        }
        self.tx.commit().context("Failed to commit write transaction")
    }

    // ── Internals ────────────────────────────────────────────────────────────

    /// The entity's version as it stands, i.e. the version this write is about
    /// to move away from. It is only *read* here: the new version is derived
    /// from the rows the write moves, in `log_op`, which is the single place a
    /// version is assigned.
    fn current_version(&self, uuid: Uuid) -> Result<u64> {
        db::get_version(&self.tx, uuid)?
            .ok_or_else(|| DomainError::NotFound(format!("Metarecord not found: {uuid}")).into())
    }

    /// Fetches a field row, checking it belongs to the given metarecord.
    fn get_owned_row(&self, uuid: Uuid, field_id: i64) -> Result<FieldRow> {
        db::get_field_rows(&self.tx, uuid)?.into_iter().find(|r| r.id == field_id).ok_or_else(
            || {
                DomainError::NotFound(format!("Field {field_id} not found on metarecord {uuid}"))
                    .into()
            },
        )
    }

    /// Buffers one operation; the log rows are inserted in bulk, in batches
    /// of [`FLUSH_THRESHOLD`].
    fn log_op(
        &mut self,
        op_type: OpType,
        entity: Uuid,
        field_name: Option<&str>,
        version_before: Option<u64>,
        before: Vec<FieldRow>,
        after: Vec<FieldRow>,
    ) -> Result<()> {
        // The version follows the rows this op moved: subtract the terms of
        // what it removed, add the terms of what it inserted (spec-data-model
        // "Version"). This is the single place a version is assigned. `None`
        // when the op removed the metarecord itself — there is no row left to
        // carry a version, and nothing for a redo to restore.
        let version_after = match db::get_version(&self.tx, entity)? {
            Some(current) => {
                let (add, sub) = version::delta(&before, &after);
                let assigned = version::apply(current, add, sub);
                set_version(&self.tx, entity, assigned)?;
                Some(assigned)
            }
            None => None,
        };
        self.observe_effects(op_type, &before, &after, entity);
        self.pending.push(PendingOp {
            op_type,
            entity,
            field_name: field_name.map(str::to_string),
            version_before,
            version_after,
            before,
            after,
            reverts_op_id: self.reverting,
        });
        if self.pending.len() >= FLUSH_THRESHOLD {
            self.flush_pending()?;
        }
        Ok(())
    }

    /// Bulk-inserts the buffered `operation` and `op_snapshot` rows,
    /// advancing the running chain head.
    ///
    /// Operation ids are assigned up front from `sqlite_sequence` so the
    /// parent chain can be computed before inserting; explicit-id inserts
    /// into an AUTOINCREMENT table keep the sequence in step, preserving the
    /// never-reused-id guarantee.
    fn flush_pending(&mut self) -> Result<()> {
        use rusqlite::types::Value as Sql;
        use rusqlite::OptionalExtension as _;

        if self.pending.is_empty() {
            return Ok(());
        }
        let last_id: Option<i64> = self
            .tx
            .query_row("SELECT seq FROM sqlite_sequence WHERE name = 'operation'", [], |r| r.get(0))
            .optional()?;
        let base = last_id.unwrap_or(0) + 1;

        let pending = std::mem::take(&mut self.pending);
        let mut op_rows: Vec<Vec<Sql>> = Vec::with_capacity(pending.len());
        let mut snapshot_rows: Vec<Vec<Sql>> = Vec::new();
        for (i, op) in pending.iter().enumerate() {
            let op_id = base + i as i64;
            let parent = if i == 0 { self.chain_head } else { Some(op_id - 1) };
            op_rows.push(vec![
                Sql::Integer(op_id),
                parent.map_or(Sql::Null, Sql::Integer),
                Sql::Integer(self.rev_id),
                Sql::Integer(self.flushed + i as i64 + 1), // seq
                Sql::Text(op.op_type.as_str().to_string()),
                Sql::Blob(db::uuid_to_bytes(op.entity)),
                op.version_before.map_or(Sql::Null, |v| Sql::Integer(v as i64)),
                op.version_after.map_or(Sql::Null, |v| Sql::Integer(v as i64)),
                op.field_name.clone().map_or(Sql::Null, Sql::Text),
                op.reverts_op_id.map_or(Sql::Null, Sql::Integer),
            ]);
            for (is_new, rows) in [(0, &op.before), (1, &op.after)] {
                for row in rows {
                    let e = db::encode_value(&row.value);
                    snapshot_rows.push(vec![
                        Sql::Integer(op_id),
                        Sql::Integer(is_new),
                        Sql::Integer(row.id),
                        Sql::Text(row.name.clone()),
                        Sql::Text(e.value_type.to_string()),
                        e.text.map_or(Sql::Null, Sql::Text),
                        e.int.map_or(Sql::Null, Sql::Integer),
                        e.real.map_or(Sql::Null, Sql::Real),
                        e.uuid.map_or(Sql::Null, Sql::Blob),
                        e.ref_repo.map_or(Sql::Null, Sql::Blob),
                        e.name.map_or(Sql::Null, Sql::Text),
                        e.name_bytes.map_or(Sql::Null, Sql::Blob),
                    ]);
                }
            }
        }

        bulk_insert(
            &self.tx,
            "INSERT INTO operation
                 (id, parent_id, rev_id, seq, op_type, entity_uuid,
                  entity_version_before, entity_version_after, field_name,
                  reverts_op_id)",
            10,
            &op_rows,
        )?;
        bulk_insert(
            &self.tx,
            "INSERT INTO op_snapshot
                 (op_id, is_new, field_id, field_name, value_type, value_text,
                  value_int, value_real, value_uuid, value_ref_repo, value_name,
                  value_name_bytes)",
            12,
            &snapshot_rows,
        )?;
        self.flushed += pending.len() as i64;
        self.chain_head = Some(base + pending.len() as i64 - 1);
        Ok(())
    }

    /// Enforces the "one value type per field name" invariant (spec-data-model):
    /// a field name carries a single non-`Nothing` value type repository-wide.
    /// `Nothing` is absence, not a type, so it is always allowed. A non-`Nothing`
    /// value whose type differs from the established one is rejected; changing a
    /// field's type is the dedicated `retype` operation (which bypasses this).
    /// At most one index seek (`idx_field_name_type`) per field name per revision:
    /// the established type is cached in [`Self::field_types`] after the first
    /// probe (or after this write establishes it).
    fn validate_value_type(&mut self, field_name: &str, value: &Value) -> Result<()> {
        if matches!(value, Value::Nothing) {
            return Ok(());
        }
        if let Some(deferred) = &mut self.deferred_types {
            // Checked once at commit instead, over the state this revision
            // actually lands on.
            deferred.insert(field_name.to_string());
            return Ok(());
        }
        let new_type = db::encode_value(value).value_type;

        if let Some(established) = self.field_types.get(field_name) {
            return if established == new_type {
                Ok(())
            } else {
                Err(Self::type_conflict(field_name, established, new_type))
            };
        }

        // Not cached yet: probe the DB once. An established differing type is a
        // conflict; otherwise this write fixes the type — cache it either way.
        if let Some(established) = db::established_value_type(&self.tx, field_name)? {
            if established != new_type {
                return Err(Self::type_conflict(field_name, &established, new_type));
            }
        }
        self.field_types.insert(field_name.to_string(), new_type.to_string());
        Ok(())
    }

    fn type_conflict(field_name: &str, established: &str, attempted: &str) -> anyhow::Error {
        DomainError::BadRequest(format!(
            "field '{field_name}' has value type '{established}'; cannot write a \
             '{attempted}' value (use retype to change the field's type)"
        ))
        .into()
    }

    /// For TreeRef values: the parent must be null (root) or an existing metarecord
    /// carrying a TreeRef of the same field name; the write must not create a
    /// cycle nor exceed [`MAX_TREE_DEPTH`] (spec-main invariants).
    fn validate_tree_ref(&self, metarecord: Uuid, field_name: &str, value: &Value) -> Result<()> {
        let Value::TreeRef { parent, .. } = value else {
            return Ok(());
        };
        let Some(parent) = parent else {
            return Ok(()); // Root node: nothing to check.
        };
        if *parent == metarecord {
            return Err(DomainError::BadRequest(format!(
                "TreeRef write would create a cycle on '{field_name}'"
            ))
            .into());
        }
        let parent_positions = db::get_tree_parents(&self.tx, field_name, *parent)?;
        if parent_positions.is_empty() {
            return Err(DomainError::BadRequest(format!(
                "invalid TreeRef parent {parent}: no such metarecord carrying a \
                 '{field_name}' TreeRef field"
            ))
            .into());
        }

        // Walk every ancestor chain (multi-map fields make this a DAG walk):
        // detect cycles through the metarecord being written and measure depth.
        let mut visited: HashSet<Uuid> = HashSet::new();
        let mut frontier = vec![*parent];
        let mut chain_len = 1; // The parent itself.
        loop {
            let mut next = Vec::new();
            for node in frontier {
                if node == metarecord {
                    return Err(DomainError::BadRequest(format!(
                        "TreeRef write would create a cycle on '{field_name}'"
                    ))
                    .into());
                }
                if !visited.insert(node) {
                    continue;
                }
                for gp in db::get_tree_parents(&self.tx, field_name, node)?.into_iter().flatten() {
                    next.push(gp);
                }
            }
            if next.is_empty() {
                break;
            }
            chain_len += 1;
            if chain_len >= MAX_TREE_DEPTH {
                return Err(DomainError::BadRequest(format!(
                    "TreeRef depth exceeds {MAX_TREE_DEPTH}"
                ))
                .into());
            }
            frontier = next;
        }
        // The new node sits one level below the deepest ancestor chain.
        if chain_len + 1 > MAX_TREE_DEPTH {
            return Err(
                DomainError::BadRequest(format!("TreeRef depth exceeds {MAX_TREE_DEPTH}")).into()
            );
        }
        Ok(())
    }
}

/// The outcome of [`Writer::append_field`]: either the row it wrote, or the row
/// that already held the value — in which case the append was a no-op
/// (spec-data-model "No duplicate rows").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appended {
    Created(i64),
    AlreadyPresent(i64),
}

impl Appended {
    /// The id of the row holding the value, written now or already there.
    pub fn id(self) -> i64 {
        match self {
            Appended::Created(id) | Appended::AlreadyPresent(id) => id,
        }
    }

    /// Whether a row was actually written.
    pub fn created(self) -> bool {
        matches!(self, Appended::Created(_))
    }
}

// ── Duplicate collapsing (spec-data-model "No duplicate rows") ───────────────

/// Splits `values` into the values actually to be written (first occurrence of
/// each) and, for every given value, the index of the kept value that stands
/// for it. `(["a", "b", "a"])` → `(["a", "b"], [0, 1, 0])`.
fn collapse_duplicates(values: &[Value]) -> (Vec<Value>, Vec<usize>) {
    let mut kept: Vec<Value> = Vec::with_capacity(values.len());
    let mut slot = Vec::with_capacity(values.len());
    for value in values {
        match kept.iter().position(|k| k == value) {
            Some(i) => slot.push(i),
            None => {
                slot.push(kept.len());
                kept.push(value.clone());
            }
        }
    }
    (kept, slot)
}

/// The fields to write for a whole-record write: the first occurrence of each
/// `(name, value)` pair, in the order given.
fn collapse_duplicate_fields(fields: Vec<Field>) -> Vec<Field> {
    let mut kept: Vec<Field> = Vec::with_capacity(fields.len());
    for f in fields {
        if !kept.iter().any(|k| k.name == f.name && k.value == f.value) {
            kept.push(f);
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_best_effort_swallows_a_vacuum_failure() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        // Idle connection: VACUUM runs.
        assert!(compact_best_effort(&conn), "VACUUM should succeed on an idle connection");

        // VACUUM cannot run inside an open transaction; the failure must be
        // swallowed (returned as false), not propagated — a committed prune is
        // never failed by its best-effort compaction.
        conn.execute_batch("BEGIN").unwrap();
        assert!(!compact_best_effort(&conn), "a VACUUM failure must be reported, not raised");
        conn.execute_batch("ROLLBACK").unwrap();
    }
}
