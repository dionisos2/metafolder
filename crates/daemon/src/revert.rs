//! Reverting operations (spec-event-log "Revert").
//!
//! A revert undoes a set of past operations *without moving HEAD*: it applies
//! the inverse of each member, newest to oldest, and writes those inverses
//! through [`log::Writer`] as a new revision. Rollback undoes everything back
//! to a point in time; a revert undoes one thing and leaves the rest standing.
//!
//! Whether that is meaningful is decided by the *dependency check*: a later
//! operation may have overwritten what the reverted one wrote, in which case
//! there is no "before" left to restore. The check is structural — it reads the
//! `operation` table and nothing else.

use std::collections::{BTreeSet, HashMap, HashSet};

use anyhow::Result;
use metafolder_core::metarecord::Field;
use rusqlite::params;
use uuid::Uuid;

use crate::db::{self, FieldRow};
use crate::log::{self, OpRow, OpType, Writer};

/// The cell an operation writes: one field of an entity, or the whole entity
/// when the operation carries no field name (spec-event-log "Operation
/// dependencies").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    pub entity: Uuid,
    /// `None` = the whole entity, which intersects every cell of it.
    pub field: Option<String>,
}

impl Cell {
    pub fn of(op: &OpRow) -> Self {
        Self { entity: op.entity_uuid, field: op.field_name.clone() }
    }

    pub fn intersects(&self, other: &Self) -> bool {
        self.entity == other.entity
            && match (&self.field, &other.field) {
                (Some(a), Some(b)) => a == b,
                // Either side covering the whole entity meets everything on it.
                _ => true,
            }
    }
}

/// An operation standing in the way of a revert.
#[derive(Debug, Clone)]
pub struct Blocker {
    pub op: OpRow,
    pub timestamp: i64,
}

/// What a revert of a given target would do.
pub struct Analysis {
    /// The operations asked for, oldest first.
    pub requested: Vec<OpRow>,
    /// The dependency closure minus `requested`, oldest first: what
    /// `with_dependents` would add.
    pub dependents: Vec<OpRow>,
    /// The operations directly blocking `requested`. Empty exactly when
    /// `dependents` is.
    pub blocked: Vec<Blocker>,
}

impl Analysis {
    pub fn revertable(&self) -> bool {
        self.blocked.is_empty()
    }

    /// The operations a revert would actually undo, oldest first.
    pub fn effective(&self, with_dependents: bool) -> Vec<OpRow> {
        let mut ops = self.requested.clone();
        if with_dependents {
            ops.extend(self.dependents.iter().cloned());
            ops.sort_by_key(|o| o.id);
        }
        ops
    }
}

/// Resolves the operations of a revision, oldest first.
pub fn revision_ops(conn: &rusqlite::Connection, rev_id: i64) -> Result<Vec<OpRow>> {
    let ids: Vec<i64> = conn
        .prepare_cached("SELECT id FROM operation WHERE rev_id = ?1 ORDER BY seq, id")?
        .query_map(params![rev_id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    ids.into_iter()
        .map(|id| {
            log::get_op(conn, id)?
                .ok_or_else(|| anyhow::anyhow!("operation {id} vanished from revision {rev_id}"))
        })
        .collect()
}

fn revision_timestamp(conn: &rusqlite::Connection, rev_id: i64) -> Result<i64> {
    Ok(conn
        .query_row("SELECT timestamp FROM revision WHERE id = ?1", params![rev_id], |r| r.get(0))?)
}

/// Operations of `entity` newer than `after_id`, whatever branch they sit on.
/// Served by `idx_operation_entity (entity_uuid, id)`.
fn entity_ops_after(
    conn: &rusqlite::Connection,
    entity: Uuid,
    after_id: i64,
) -> Result<Vec<OpRow>> {
    let ids: Vec<i64> = conn
        .prepare_cached("SELECT id FROM operation WHERE entity_uuid = ?1 AND id > ?2 ORDER BY id")?
        .query_map(params![db::uuid_to_bytes(entity), after_id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    ids.into_iter()
        .map(|id| log::get_op(conn, id)?.ok_or_else(|| anyhow::anyhow!("operation {id} vanished")))
        .collect()
}

/// Analyses a revert of `requested`: its blockers, and the closure that taking
/// them along would produce.
///
/// The closure is a fixpoint, not one pass: an entity-scoped blocker widens the
/// cells the set writes, so operations that were independent a round earlier
/// become blockers in turn. It terminates because a blocker is always younger
/// than the set's oldest member, so the iteration never leaves the window
/// between that operation and HEAD.
pub fn analyse(
    conn: &rusqlite::Connection,
    head: Option<i64>,
    requested: Vec<OpRow>,
) -> Result<Analysis> {
    let Some(head) = head else {
        return Ok(Analysis { requested, dependents: vec![], blocked: vec![] });
    };
    let ancestry: HashSet<i64> = log::ancestry(conn, head)?.into_iter().collect();
    let oldest = requested.iter().map(|o| o.id).min().unwrap_or(head);

    let mut set: BTreeSet<i64> = requested.iter().map(|o| o.id).collect();
    let mut cells: Vec<Cell> = requested.iter().map(Cell::of).collect();
    let mut dependents: Vec<OpRow> = vec![];
    let mut blocked: Vec<Blocker> = vec![];

    loop {
        let entities: BTreeSet<Uuid> = cells.iter().map(|c| c.entity).collect();
        let mut found: Vec<OpRow> = vec![];
        for entity in entities {
            for op in entity_ops_after(conn, entity, oldest)? {
                if set.contains(&op.id) || !ancestry.contains(&op.id) {
                    continue;
                }
                let cell = Cell::of(&op);
                if cells.iter().any(|c| c.intersects(&cell)) {
                    found.push(op);
                }
            }
        }
        if found.is_empty() {
            break;
        }
        if blocked.is_empty() {
            // The first round is what blocks the operations actually asked for.
            for op in &found {
                blocked.push(Blocker {
                    timestamp: revision_timestamp(conn, op.rev_id)?,
                    op: op.clone(),
                });
            }
        }
        for op in found {
            set.insert(op.id);
            cells.push(Cell::of(&op));
            dependents.push(op);
        }
    }
    dependents.sort_by_key(|o| o.id);
    Ok(Analysis { requested, dependents, blocked })
}

/// The filesystem action undoing an operation requires, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsAction {
    /// `mfr_path` moves back: the file has to move with it.
    Move,
    /// Content the operation destroyed has to come back from the trash-bin.
    RestoreContent,
}

pub fn fs_action(op: &OpRow) -> Option<FsAction> {
    match op.op_type.as_str() {
        "file_moved" => Some(FsAction::Move),
        "file_deleted" | "file_modified" => Some(FsAction::RestoreContent),
        // A trashing deletes the metarecord outright, and its bytes are in the
        // trash-bin: putting the record back means putting the file back with
        // it. The op type cannot say so — an ordinary `delete_metarecord`
        // touches no file — so the revision's origin does.
        "delete_metarecord" if op.origin.as_deref() == Some("trash") => {
            Some(FsAction::RestoreContent)
        }
        _ => None,
    }
}

/// The op type a revert of `op` writes: the type of the change it actually
/// makes, filesystem included (spec-event-log "Revert content"). It is what
/// tells a later navigation that crossing this operation needs a file action,
/// so a revert that moved a file must record `file_moved` and not `set_field`.
pub fn written_as(op: &OpRow) -> OpType {
    match op.op_type.as_str() {
        // `file_moved` carries both directions — it is already what `reconcile`
        // writes when it relinks an orphan whose `mfr_path` was `Nothing`.
        "file_deleted" | "file_moved" => OpType::FileMoved,
        "file_modified" => OpType::FileModified,
        _ => OpType::SetField,
    }
}

/// Applies the inverse of `ops` (oldest first on input) as new writes, newest
/// to oldest. Returns the number of operations actually undone.
///
/// Row ids are *not* restored: a revert is a new write, so its inserts take
/// fresh ids. That is what makes the `remap` necessary — a rollback composes
/// inverses blindly precisely because it restores `field.id` exactly, and a
/// revert cannot. Every row a step re-creates is recorded here under the id it
/// used to have, so a later (older) row-scoped inverse finds it again.
pub fn apply(writer: &mut Writer, ops: &[OpRow]) -> Result<usize> {
    // The reverse walk passes through states that need not be type-consistent
    // even when the one it lands on is; the check moves to commit.
    writer.defer_type_checks();
    let mut remap: HashMap<i64, i64> = HashMap::new();
    let mut done = 0usize;
    for op in ops.iter().rev() {
        // Everything written for this op names it, so a later reader can tell a
        // correction from a change (spec-event-log "reverts_op_id").
        writer.reverting(Some(op.id));
        // Read through the writer's own transaction, so the state the revert
        // is computed from is the state it is written into.
        let before = log::snapshots(writer.connection(), op.id, 0)?;
        let after = log::snapshots(writer.connection(), op.id, 1)?;
        match op.op_type.as_str() {
            "create_metarecord" => writer.delete_metarecord(op.entity_uuid)?,
            "delete_metarecord" => {
                let record =
                    writer.create_metarecord_with_uuid(op.entity_uuid, rows_to_fields(&before))?;
                remember_fields(&mut remap, &before, &record.fields);
            }
            "set_metarecord" => {
                let record = writer.set_record(op.entity_uuid, rows_to_fields(&before))?;
                remember_fields(&mut remap, &before, &record.fields);
            }
            "append_field" => {
                // The row this operation added — under whatever id it carries now.
                let Some(row) = after.first() else { continue };
                let id = remap.get(&row.id).copied().unwrap_or(row.id);
                writer.delete_field(op.entity_uuid, id)?;
            }
            "delete_field" => {
                let Some(row) = before.first() else { continue };
                let new_id =
                    writer.append_field(op.entity_uuid, &row.name, row.value.clone())?.id();
                remap.insert(row.id, new_id);
            }
            "unknown" => {
                anyhow::bail!("operation {} is an unlogged write and cannot be reverted", op.id)
            }
            // `set_field` and the three file types are cell-scoped: their
            // before-snapshot is the whole cell.
            _ => {
                let name = op
                    .field_name
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("operation {} carries no field name", op.id))?;
                let op_type = written_as(op);
                if before.is_empty() {
                    // The field did not exist: remove the cell entirely rather
                    // than write the explicit-absence sentinel.
                    writer.clear_field_as(op_type, op.entity_uuid, &name)?;
                } else {
                    let ids = writer.set_field_multi_as(
                        op_type,
                        op.entity_uuid,
                        &name,
                        before.iter().map(|r| r.value.clone()).collect(),
                    )?;
                    remember(&mut remap, &before, ids.into_iter());
                }
            }
        }
        done += 1;
    }
    writer.reverting(None);
    Ok(done)
}

fn rows_to_fields(rows: &[FieldRow]) -> Vec<Field> {
    rows.iter().map(|r| Field::new(r.name.clone(), r.value.clone())).collect()
}

/// Records `old row id → the id the revert's own write gave it`, pairing
/// positionally: both sequences are in the order the rows were written.
fn remember(remap: &mut HashMap<i64, i64>, old: &[FieldRow], new: impl Iterator<Item = i64>) {
    for (row, id) in old.iter().zip(new) {
        remap.insert(row.id, id);
    }
}

/// The whole-record form of [`remember`]: pairs each snapshot row with the
/// written field carrying the same `(name, value)` rather than by position,
/// because a whole-record write collapses repeated pairs (spec-data-model "No
/// duplicate rows") — two rows a pre-rule revision recorded as duplicates then
/// map onto the single row that stands for both.
fn remember_fields(remap: &mut HashMap<i64, i64>, old: &[FieldRow], new: &[Field]) {
    for row in old {
        let written = new.iter().find(|f| f.name == row.name && f.value == row.value);
        if let Some(id) = written.and_then(|f| f.id) {
            remap.insert(row.id, id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(op_type: &str, origin: Option<&str>) -> OpRow {
        OpRow {
            id: 1,
            parent_id: None,
            rev_id: 1,
            seq: 0,
            op_type: op_type.into(),
            entity_uuid: uuid::Uuid::nil(),
            entity_version_before: None,
            entity_version_after: None,
            field_name: None,
            reverts_op_id: None,
            origin: origin.map(str::to_string),
        }
    }

    // A trashing deletes the metarecord outright, so reverting it has to bring
    // the *bytes* back too — they are sitting in the trash-bin, and only the
    // client can move them. Nothing in the op type says so: an ordinary
    // `delete_metarecord` touches no file at all. The revision's origin is what
    // separates the two (spec-trash "Undo, rollback and redo").
    #[test]
    fn a_trashing_s_metarecord_deletion_asks_for_the_content_back() {
        assert!(matches!(
            fs_action(&op("delete_metarecord", Some("trash"))),
            Some(FsAction::RestoreContent)
        ));
    }

    #[test]
    fn an_ordinary_metarecord_deletion_touches_no_file() {
        assert!(fs_action(&op("delete_metarecord", None)).is_none());
        assert!(fs_action(&op("delete_metarecord", Some("watcher"))).is_none());
    }
}
