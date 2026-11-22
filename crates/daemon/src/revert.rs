//! Reverting operations (doc "Revert").
//!
//! A revert undoes a set of past operations *without moving HEAD*: it applies
//! the inverse of each member, newest to oldest, to the state as it is now,
//! and writes those inverses through [`log::Writer`] as a new write
//! (doc "Reverting onto the current state"). What an operation wrote is never
//! a condition for undoing it — a later write on the same cell does not block
//! the revert, it only earns a warning in the plan ([`changed_since`]).

use anyhow::Result;
use metafolder_core::metarecord::Field;
use uuid::Uuid;

use crate::log::{OpRow, OpType, Writer};
use crate::rows::FieldRow;
use crate::store::Log;

/// The cell an operation writes: one field of an entity, or the whole entity
/// when the operation carries no field name — what a later write must touch
/// for [`changed_since`] to name it.
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

/// The operations of a log context and of every log context nested in it, oldest first:
/// reverting a run reverts what was written inside it (doc "Log contexts").
pub fn log_context_ops(log: &dyn Log, log_context: i64) -> Result<Vec<OpRow>> {
    let mut ops = Vec::new();
    for log_context in crate::log::log_context_subtree(log, log_context)? {
        ops.extend(log.log_context_ops(log_context)?);
    }
    ops.sort_by_key(|op| op.id);
    Ok(ops)
}

/// Which operations of a log context an undo or a redo takes (doc "Undo and redo
/// by log context").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Only {
    /// Its changes still standing — what an undo takes back: not a revert,
    /// not the watcher's, not undone already.
    Changes,
    /// Its undos still standing — what a redo takes back: reverts of a
    /// change, not redone already.
    Undos,
}

impl Only {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "changes" => Some(Only::Changes),
            "undos" => Some(Only::Undos),
            _ => None,
        }
    }
}

/// `ops` narrowed to those `only` takes.
pub fn narrow(log: &dyn Log, ops: Vec<OpRow>, only: Only) -> Result<Vec<OpRow>> {
    let mut memo = std::collections::HashMap::new();
    let mut out = Vec::new();
    for op in ops {
        let keep = match only {
            Only::Changes => {
                op.reverts_op_id.is_none()
                    && op.origin.as_deref() != Some(crate::log::ORIGIN_WATCHER)
            }
            // A revert whose target is gone from the log counts as an undo:
            // the conservative reading, as the undo selection's.
            Only::Undos => match op.reverts_op_id {
                Some(target) => log.op(target)?.is_none_or(|t| t.reverts_op_id.is_none()),
                None => false,
            },
        };
        if keep && standing(log, op.id, &mut memo)? {
            out.push(op);
        }
    }
    Ok(out)
}

/// What going back to the state after operation `cut` takes (doc "Reverting
/// everything since a time"): of the operations written after it, the
/// user's — not the watcher's — still standing that are either a change, or
/// a revert of an operation from before the cut (an undo or a redo of
/// earlier work: reverting it brings that work back as it stood). A revert
/// of an operation written after the cut is left alone: the two cancel out,
/// or the revert is itself undone, and either way nothing of it was there at
/// the cut.
pub fn since(log: &dyn Log, cut: i64) -> Result<Vec<OpRow>> {
    let mut memo = std::collections::HashMap::new();
    let mut out = Vec::new();
    for op in log.ops_after(cut)? {
        let keep = op.origin.as_deref() != Some(crate::log::ORIGIN_WATCHER)
            && op.reverts_op_id.is_none_or(|target| target <= cut);
        if keep && standing(log, op.id, &mut memo)? {
            out.push(op);
        }
    }
    Ok(out)
}

/// Whether `op` still stands: no revert of it stands. Resolved along the whole
/// chain — a revert that was itself reverted undoes nothing any more (doc
/// "Why undo and redo do not oscillate").
pub fn standing(
    log: &dyn Log,
    op: i64,
    memo: &mut std::collections::HashMap<i64, bool>,
) -> Result<bool> {
    if let Some(&known) = memo.get(&op) {
        return Ok(known);
    }
    let mut stands = true;
    for reverter in log.reverters(op)? {
        if standing(log, reverter, memo)? {
            stands = false;
            break;
        }
    }
    memo.insert(op, stands);
    Ok(stands)
}

/// What the plan says of a cell that changed since `op` wrote it.
#[derive(Debug, Clone)]
pub enum Changed {
    /// The cell holds what `op` left there.
    No,
    /// It holds something else, written last by this logged operation.
    By(OpRow),
    /// It holds something else, and what wrote it is no longer in the log.
    Unlogged,
}

/// Whether the cell `op` wrote still holds what it left there (for the writes
/// that leave one: the log keeps no state an operation left, so it is a later
/// operation on the cell that says it changed) — a warning
/// for the plan, never a refusal: the revert goes back to what `op` replaced
/// whatever was written since (doc "Reverting onto the current state").
pub fn changed_since<S: crate::store::Store + ?Sized>(store: &S, op: &OpRow) -> Result<Changed> {
    let entity = op.entity_uuid;
    // The log keeps no state left by an operation: an append says what it
    // added and a removal what it took, which the rows are compared with; for
    // the others, the cell is as the operation left it unless a later
    // operation touched it.
    let checked = match op.op_type.as_str() {
        "delete_metarecord" => Some(!store.exists(entity)?),
        "append_field" => {
            let Some(name) = op.field_name.as_deref() else { return Ok(Changed::No) };
            let now = store.rows_named(entity, name)?;
            Some(store.added(op.id)?.iter().all(|a| now.iter().any(|r| r.value == a.value)))
        }
        "delete_field" => {
            let Some(name) = op.field_name.as_deref() else { return Ok(Changed::No) };
            let before = store.before_rows(op.id)?;
            let now = store.rows_named(entity, name)?;
            Some(!before.iter().any(|b| now.iter().any(|r| r.value == b.value)))
        }
        _ => None,
    };
    if checked == Some(true) {
        return Ok(Changed::No);
    }
    let cell = Cell::of(op);
    let later = store.entity_ops_after(entity, op.id)?;
    // The operations that revert `op`, the revert of that, and so on: each
    // writes the cell back as the one before it left it, so one at an even
    // depth (a redo) leaves it as `op` did, whatever was written in between.
    let mut chain: std::collections::HashMap<i64, usize> = [(op.id, 0)].into();
    for o in &later {
        if let Some(depth) = o.reverts_op_id.and_then(|r| chain.get(&r)).copied() {
            chain.insert(o.id, depth + 1);
        }
    }
    let last = later.into_iter().rev().find(|o| cell.intersects(&Cell::of(o)));
    if last.as_ref().is_some_and(|o| chain.get(&o.id).is_some_and(|d| d % 2 == 0)) {
        return Ok(Changed::No);
    }
    Ok(match (last, checked) {
        (Some(o), _) => Changed::By(o),
        // The value is not there and nothing logged says why.
        (None, Some(_)) => Changed::Unlogged,
        // Nothing was written to the cell since.
        (None, None) => Changed::No,
    })
}

/// The rows of `name` that `op` left on its entity, which the log does not
/// keep: what the next operation on the cell took off, or the rows now when
/// none came after. Right for the operations that replace the cell — a move, a
/// `set_field` — and for a cell a later append only adds to, it says what the
/// append found (nothing).
pub fn left_by<S: crate::store::Store + ?Sized>(
    store: &S,
    op: &OpRow,
    name: &str,
) -> Result<Vec<FieldRow>> {
    let cell = Cell { entity: op.entity_uuid, field: Some(name.to_string()) };
    for later in store.entity_ops_after(op.entity_uuid, op.id)? {
        if cell.intersects(&Cell::of(&later)) {
            let before = store.before_rows(later.id)?;
            return Ok(before.into_iter().filter(|r| r.name == name).collect());
        }
    }
    store.rows_named(op.entity_uuid, name)
}

/// The filesystem action crossing an operation requires, if any — what the
/// client has to do on disk for the metadata to stay true (doc "Filesystem coordination",
/// doc "Trash, undo and redo").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsAction {
    /// `mfr_path` moves: the file has to move with it.
    Move,
    /// Content the operation destroyed has to come back from the trash-bin.
    RestoreContent,
    /// A trashed metarecord goes again: its bytes have to go back to the
    /// trash-bin with it — a trashing redone.
    TrashContent,
}

/// How far a `reverts_op_id` chain is followed before giving up. A revert of a
/// revert of a … trashing is a redo of an undo of a redo: a handful deep in
/// practice, and the bound only stops a corrupted, cyclic chain.
const MAX_REVERT_CHAIN: usize = 64;

/// Whether `op` creates or deletes a metarecord whose bytes a *trashing* took:
/// the `delete_metarecord` of a write stamped `trash`, or a create/delete a
/// revert wrote whose `reverts_op_id` chain leads back to one. Only the first
/// says so in its own row — the undo of a trashing, when it is a revert, writes
/// an ordinary `create_metarecord`, and the redo of that a `delete_metarecord`
/// of no particular origin. A link pruned out of the log ends the chain: the
/// operation is then an ordinary one, which is all that is left to know of it.
pub fn trash_coupled<L: Log + ?Sized>(log: &L, op: &OpRow) -> Result<bool> {
    let mut current = op.clone();
    for _ in 0..MAX_REVERT_CHAIN {
        if !matches!(current.op_type.as_str(), "delete_metarecord" | "create_metarecord") {
            return Ok(false);
        }
        if current.op_type == "delete_metarecord" && current.origin.as_deref() == Some("trash") {
            return Ok(true);
        }
        let Some(next) = current.reverts_op_id.map(|id| log.op(id)).transpose()?.flatten() else {
            return Ok(false);
        };
        current = next;
    }
    Ok(false)
}

/// The file action *reverting* `op` requires, `coupled` being
/// [`trash_coupled`]'s answer. Pure, so the table is tested on its own.
fn action_for(op: &OpRow, coupled: bool) -> Option<FsAction> {
    match op.op_type.as_str() {
        "file_moved" => Some(FsAction::Move),
        "file_deleted" | "file_modified" => Some(FsAction::RestoreContent),
        // The bytes follow the metarecord: they come back when it does, and go
        // back to the trash when it goes. Undoing a creation is the metarecord
        // going; undoing a deletion, it coming back.
        "delete_metarecord" => coupled.then_some(FsAction::RestoreContent),
        "create_metarecord" => coupled.then_some(FsAction::TrashContent),
        _ => None,
    }
}

/// The file action *reverting* `op` requires: a revert applies its inverse.
pub fn fs_action<L: Log + ?Sized>(log: &L, op: &OpRow) -> Result<Option<FsAction>> {
    let coupled = matches!(op.op_type.as_str(), "delete_metarecord" | "create_metarecord")
        && trash_coupled(log, op)?;
    Ok(action_for(op, coupled))
}

/// The op type a revert of `op` writes: the type of the change it actually
/// makes, filesystem included (doc "What a revert writes"). It is what
/// tells a later revert of *this* operation that it needs a file action, so a
/// revert that moved a file must record `file_moved` and not `set_field`.
pub fn written_as(op: &OpRow) -> OpType {
    match op.op_type.as_str() {
        "create_metarecord" => OpType::DeleteRecord,
        "delete_metarecord" => OpType::CreateRecord,
        "set_metarecord" => OpType::SetRecord,
        "append_field" => OpType::DeleteField,
        "delete_field" => OpType::AppendField,
        // `file_moved` carries both directions — it is already what `reconcile`
        // writes when it relinks an orphan whose `mfr_path` was `Nothing`.
        "file_deleted" | "file_moved" => OpType::FileMoved,
        "file_modified" => OpType::FileModified,
        _ => OpType::SetField,
    }
}

/// Applies the inverse of `ops` (oldest first on input) to the current state,
/// as new writes, newest to oldest. Returns the number of operations actually
/// undone.
///
/// Row ids are *not* restored — a row put back gets a fresh id, since the old
/// one may have been handed out since — so nothing here addresses a row by the
/// id the log recorded: an append is undone *by value* (two rows of one name
/// and value are interchangeable, doc "No duplicate rows"). A metarecord does
/// keep its uuid, unique by construction.
pub fn apply(writer: &mut Writer, ops: &[OpRow]) -> Result<usize> {
    // The reverse walk passes through states that need not be type-consistent
    // even when the one it lands on is; the check moves to commit.
    writer.defer_type_checks();
    let mut done = 0usize;
    // The log contexts the operations undone are in, said on this write's log context.
    let mut touched: std::collections::BTreeSet<i64> = Default::default();
    let head = crate::store::Log::head(writer.store())?.unwrap_or(0);
    let mut at = ops.len();
    while at > 0 {
        // A run of deletions of one pending mass deletion is given back at
        // once (doc "Mass operations in the log"), when the state is the one
        // the deletion left: every operation written since is reverted here,
        // and ahead of it.
        let run = run_of(&ops[..at], |o| o.op_type == "delete_metarecord", false);
        if run.len() >= MASS_UNDO_MIN {
            let last = run[run.len() - 1].id;
            let newer = ops.iter().filter(|o| o.id > last).count() as i64;
            if head - last == newer && writer.undelete_pending(run)? {
                done += run.len();
                touched.extend(run.iter().map(|o| o.log_context_id));
                at -= run.len();
                continue;
            }
        }
        // The same for the operations of one mass add or remove of a field,
        // taken back by one entry of the log.
        let run = run_of(
            &ops[..at],
            |o| o.op_type == "append_field" || o.op_type == "delete_field",
            true,
        );
        if run.len() >= MASS_UNDO_MIN && writer.revert_mass_run(run)? {
            done += run.len();
            touched.extend(run.iter().map(|o| o.log_context_id));
            at -= run.len();
            continue;
        }
        at -= 1;
        let op = &ops[at];
        // Everything written for this op names it, so a later reader can tell a
        // correction from a change (doc "Log storage").
        writer.reverting(Some(op.id));
        // Read through the writer's own transaction, so the state the revert
        // is computed from is the state it is written into. An inverse reads
        // the before only if it needs it: reading it costs a key even when
        // empty.
        let before = |writer: &Writer| writer.store().before_rows(op.id);
        match op.op_type.as_str() {
            "create_metarecord" => {
                // Already gone: there is nothing left to take back.
                if !writer.store().exists(op.entity_uuid)? {
                    continue;
                }
                writer.delete_metarecord(op.entity_uuid)?
            }
            "delete_metarecord" => {
                if writer.store().exists(op.entity_uuid)? {
                    return Err(crate::error::DomainError::Conflict(format!(
                        "metarecord {} exists again: undoing its deletion (operation {}) \
                         cannot recreate it",
                        op.entity_uuid.simple(),
                        op.id
                    ))
                    .into());
                }
                let before = before(writer)?;
                // Under the dense id it had; created anew only when the store
                // lost it (a reindex numbers the ids again).
                if !writer.restore_metarecord(op.entity_uuid, before.clone())? {
                    writer.create_metarecord_with_uuid(op.entity_uuid, rows_to_fields(&before))?;
                }
            }
            "set_metarecord" => {
                let before = before(writer)?;
                writer.set_record(op.entity_uuid, rows_to_fields(&before))?;
            }
            "append_field" => {
                for row in &writer.store().added(op.id)? {
                    writer.delete_fields_valued(op.entity_uuid, &row.name, &row.value)?;
                }
            }
            "delete_field" => {
                // One `delete_field` may carry every row of a name (an unset, a
                // removal by value): each comes back.
                for row in &before(writer)? {
                    writer.append_field(op.entity_uuid, &row.name, row.value.clone())?;
                }
            }
            "unknown" => {
                anyhow::bail!("operation {} is an unlogged write and cannot be reverted", op.id)
            }
            // `set_field` and the three file types are cell-scoped: their
            // before is the whole cell.
            _ => {
                let name = op
                    .field_name
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("operation {} carries no field name", op.id))?;
                let op_type = written_as(op);
                let before = before(writer)?;
                if before.is_empty() {
                    // The field did not exist: remove the cell entirely rather
                    // than write the explicit-absence sentinel.
                    writer.clear_field_as(op_type, op.entity_uuid, &name)?;
                } else {
                    writer.set_field_multi_as(
                        op_type,
                        op.entity_uuid,
                        &name,
                        before.iter().map(|r| r.value.clone()).collect(),
                    )?;
                }
            }
        }
        done += 1;
        touched.insert(op.log_context_id);
    }
    writer.reverting(None);
    for log_context in touched {
        writer.note_log_context_reverts(log_context, false)?;
    }
    Ok(done)
}

/// How many deletions in a row are given back at once rather than one by one.
const MASS_UNDO_MIN: usize = 16;

/// The operations that end `ops` and `wanted` accepts: the longest suffix of
/// consecutive operation ids — of one type, and of one field when `one_field`
/// — (one mass write's, when the store agrees).
fn run_of(ops: &[OpRow], wanted: impl Fn(&OpRow) -> bool, one_field: bool) -> &[OpRow] {
    let mut start = ops.len();
    while start > 0 {
        let op = &ops[start - 1];
        let same = start == ops.len()
            || (ops[start].id == op.id + 1
                && ops[start].op_type == op.op_type
                && (!one_field || ops[start].field_name == op.field_name));
        if !wanted(op) || !same {
            break;
        }
        start -= 1;
    }
    &ops[start..]
}

fn rows_to_fields(rows: &[FieldRow]) -> Vec<Field> {
    rows.iter().map(|r| Field::new(r.name.clone(), r.value.clone())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(op_type: &str, origin: Option<&str>) -> OpRow {
        OpRow {
            id: 1,
            log_context_id: 1,
            timestamp: 0,
            op_type: op_type.into(),
            entity_uuid: uuid::Uuid::nil(),
            field_name: None,
            reverts_op_id: None,
            origin: origin.map(str::to_string),
        }
    }

    // A trashing deletes the metarecord outright, so undoing it has to bring
    // the *bytes* back too — they are sitting in the trash-bin, and only the
    // client can move them — and redoing it has to send them back. Nothing in
    // the op type says so: an ordinary `delete_metarecord` touches no file at
    // all (doc "Trash, undo and redo").
    #[test]
    fn a_trashing_s_deletion_moves_the_bytes_with_the_metarecord() {
        let deletion = op("delete_metarecord", Some("trash"));
        assert_eq!(action_for(&deletion, true), Some(FsAction::RestoreContent));
        // The undo a revert wrote is a creation: undoing *it* is the redo.
        let recreation = op("create_metarecord", None);
        assert_eq!(action_for(&recreation, true), Some(FsAction::TrashContent));
    }

    #[test]
    fn an_ordinary_metarecord_deletion_touches_no_file() {
        assert!(action_for(&op("delete_metarecord", None), false).is_none());
        assert!(action_for(&op("create_metarecord", None), false).is_none());
    }
}
