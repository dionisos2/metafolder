//! The logged write flow (spec-event-log "Normal write flow"). Every write to
//! the data tables goes through a [`Writer`], which records a revision, one
//! operation per atomic change with before/after snapshots, and keeps the
//! `log_head` pointer consistent with the data tables — all in one SQLite
//! transaction.

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use rusqlite::{params, Transaction};
use uuid::Uuid;

use metafolder_core::entry::{Field, Metadata, Value};

use crate::db::{self, FieldRow};

/// Maximum depth of a TreeRef chain (spec-main invariant).
pub const MAX_TREE_DEPTH: usize = 1000;

/// Operation types recorded in the log (spec-event-log "Operation types").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpType {
    CreateEntry,
    DeleteEntry,
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
            OpType::CreateEntry => "create_entry",
            OpType::DeleteEntry => "delete_entry",
            OpType::SetField => "set_field",
            OpType::AppendField => "append_field",
            OpType::DeleteField => "delete_field",
            OpType::FileDeleted => "file_deleted",
            OpType::FileMoved => "file_moved",
            OpType::FileModified => "file_modified",
            OpType::Unknown => "unknown",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "create_entry" => OpType::CreateEntry,
            "delete_entry" => OpType::DeleteEntry,
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

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
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
    pub field_name: Option<String>,
}

pub fn get_head(conn: &rusqlite::Connection) -> Result<Option<i64>> {
    Ok(conn.query_row("SELECT op_id FROM log_head WHERE singleton = 1", [], |r| r.get(0))?)
}

const OP_COLUMNS: &str =
    "id, parent_id, rev_id, seq, op_type, entity_uuid, entity_version_before, field_name";

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
            field_name: row.get(7)?,
        },
        entity,
    ))
}

pub fn get_op(conn: &rusqlite::Connection, id: i64) -> Result<Option<OpRow>> {
    use rusqlite::OptionalExtension as _;
    let row = conn
        .query_row(&format!("SELECT {OP_COLUMNS} FROM operation WHERE id = ?1"), params![id], row_to_op)
        .optional()?;
    row.map(|(mut op, entity)| {
        op.entity_uuid = db::bytes_to_uuid(entity)?;
        Ok(op)
    })
    .transpose()
}

/// All operations, in insertion order.
pub fn all_ops(conn: &rusqlite::Connection) -> Result<Vec<OpRow>> {
    let mut stmt = conn.prepare(&format!("SELECT {OP_COLUMNS} FROM operation ORDER BY id"))?;
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

/// Ancestor chain from `from` (inclusive) up to the root, in that order.
pub fn ancestry(conn: &rusqlite::Connection, from: i64) -> Result<Vec<i64>> {
    let mut chain = Vec::new();
    let mut seen = HashSet::new();
    let mut cur = Some(from);
    while let Some(id) = cur {
        if !seen.insert(id) {
            anyhow::bail!("operation history contains a cycle at op {id}");
        }
        chain.push(id);
        cur = get_op(conn, id)?
            .with_context(|| format!("operation {id} not found"))?
            .parent_id;
    }
    Ok(chain)
}

/// Snapshot rows of one operation (`is_new` 0 = before, 1 = after).
pub fn snapshots(conn: &rusqlite::Connection, op_id: i64, is_new: i64) -> Result<Vec<FieldRow>> {
    let mut stmt = conn.prepare(
        "SELECT field_id, field_name, value_type, value_text, value_int, value_real,
                value_uuid, value_ref_repo, value_name
         FROM op_snapshot WHERE op_id = ?1 AND is_new = ?2 ORDER BY field_id",
    )?;
    let rows = stmt
        .query_map(params![op_id, is_new], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<f64>>(5)?,
                row.get::<_, Option<Vec<u8>>>(6)?,
                row.get::<_, Option<Vec<u8>>>(7)?,
                row.get::<_, Option<String>>(8)?,
            ))
        })?
        .map(|r| {
            let (id, name, vtype, text, int, real, uuid, ref_repo, vname) = r?;
            let value = db::decode_value(&vtype, text, int, real, uuid, ref_repo, vname)?;
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
            get_op(conn, *id)?.with_context(|| format!("operation {id} not found"))?;
            Ok(Some(*id))
        }
        Target::Timestamp(t) => {
            let Some(head) = head else {
                anyhow::bail!("no operation found at or before timestamp {t} (empty history)");
            };
            for op_id in ancestry(conn, head)? {
                let op = get_op(conn, op_id)?.context("ancestry op vanished")?;
                let ts: i64 = conn.query_row(
                    "SELECT timestamp FROM revision WHERE id = ?1",
                    params![op.rev_id],
                    |r| r.get(0),
                )?;
                if ts <= *t {
                    return Ok(Some(op_id));
                }
            }
            anyhow::bail!("no operation found at or before timestamp {t}")
        }
        Target::Label(label) => {
            let Some(head) = head else {
                anyhow::bail!("label '{label}' not found (empty history)");
            };
            // Walking from HEAD down, the first op of a matching revision is
            // the last operation of the most recent matching revision.
            for op_id in ancestry(conn, head)? {
                let op = get_op(conn, op_id)?.context("ancestry op vanished")?;
                let rev_label: Option<String> = conn.query_row(
                    "SELECT label FROM revision WHERE id = ?1",
                    params![op.rev_id],
                    |r| r.get(0),
                )?;
                if rev_label.as_deref() == Some(label.as_str()) {
                    return Ok(Some(op_id));
                }
            }
            anyhow::bail!("label '{label}' not found on the HEAD ancestry path")
        }
        Target::PrevRevision => {
            let Some(head) = head else {
                anyhow::bail!("nothing to undo: the history is empty");
            };
            // Find the first operation of HEAD's revision; its parent is the
            // state before the whole revision (None = empty state).
            let mut op = get_op(conn, head)?.context("HEAD operation not found")?;
            while let Some(parent_id) = op.parent_id {
                let parent = get_op(conn, parent_id)?.context("parent op vanished")?;
                if parent.rev_id != op.rev_id {
                    break;
                }
                op = parent;
            }
            Ok(op.parent_id)
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

/// Moves HEAD to `target` in one atomic transaction: inverse operations on
/// the path up to the LCA, forward operations down to the target.
pub fn navigate(
    conn: &mut rusqlite::Connection,
    db_id: Uuid,
    target: Option<i64>,
) -> Result<NavResult> {
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
            // Empty state: every data row of this repository is removed.
            let unapplied = ancestry(&tx, head)?.len();
            tx.execute(
                "DELETE FROM metadata WHERE uuid IN
                     (SELECT metadata_uuid FROM metadata_db WHERE db_id = ?1)",
                params![db::uuid_to_bytes(db_id)],
            )?;
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
            let unapply: Vec<i64> =
                h_anc.into_iter().take_while(|id| Some(*id) != lca).collect();
            let mut apply: Vec<i64> =
                t_anc.into_iter().take_while(|id| Some(*id) != lca).collect();
            apply.reverse(); // oldest first
            (unapply, apply)
        }
    };

    for op_id in &unapply {
        let op = get_op(&tx, *op_id)?.context("operation vanished during navigation")?;
        apply_inverse(&tx, db_id, &op)?;
    }
    for op_id in &apply {
        let op = get_op(&tx, *op_id)?.context("operation vanished during navigation")?;
        apply_forward(&tx, db_id, &op)?;
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

fn restore_version(tx: &Transaction<'_>, uuid: Uuid, version: Option<u64>) -> Result<()> {
    if let Some(version) = version {
        tx.execute(
            "UPDATE metadata SET version = ?1 WHERE uuid = ?2",
            params![version as i64, db::uuid_to_bytes(uuid)],
        )?;
    }
    Ok(())
}

/// Undoes one operation (spec-event-log "Inverse operations"). Field rows
/// are restored with their original primary keys.
fn apply_inverse(tx: &Transaction<'_>, db_id: Uuid, op: &OpRow) -> Result<()> {
    let entity = op.entity_uuid;
    match op.op_type.as_str() {
        "create_entry" => {
            tx.execute(
                "DELETE FROM metadata WHERE uuid = ?1",
                params![db::uuid_to_bytes(entity)],
            )?;
        }
        "delete_entry" => {
            tx.execute(
                "INSERT INTO metadata (uuid, version) VALUES (?1, ?2)",
                params![
                    db::uuid_to_bytes(entity),
                    op.entity_version_before.unwrap_or(0) as i64
                ],
            )?;
            tx.execute(
                "INSERT INTO metadata_db (metadata_uuid, db_id) VALUES (?1, ?2)",
                params![db::uuid_to_bytes(entity), db::uuid_to_bytes(db_id)],
            )?;
            for row in snapshots(tx, op.id, 0)? {
                db::insert_field_row(tx, entity, &row.name, &row.value, Some(row.id))?;
            }
        }
        // All set-field-shaped operations (one field name, full replacement).
        "set_field" | "file_deleted" | "file_moved" | "file_modified" => {
            let field = op.field_name.as_deref().context("set-shaped op without field_name")?;
            tx.execute(
                "DELETE FROM field WHERE metadata_uuid = ?1 AND field_name = ?2",
                params![db::uuid_to_bytes(entity), field],
            )?;
            for row in snapshots(tx, op.id, 0)? {
                db::insert_field_row(tx, entity, &row.name, &row.value, Some(row.id))?;
            }
            restore_version(tx, entity, op.entity_version_before)?;
        }
        "append_field" => {
            for row in snapshots(tx, op.id, 1)? {
                tx.execute("DELETE FROM field WHERE id = ?1", params![row.id])?;
            }
            restore_version(tx, entity, op.entity_version_before)?;
        }
        "delete_field" => {
            for row in snapshots(tx, op.id, 0)? {
                db::insert_field_row(tx, entity, &row.name, &row.value, Some(row.id))?;
            }
            restore_version(tx, entity, op.entity_version_before)?;
        }
        "unknown" => anyhow::bail!("cannot navigate across an 'unknown' operation (op {})", op.id),
        other => anyhow::bail!("unsupported op_type '{other}' in the log"),
    }
    Ok(())
}

/// Replays one operation forward (redo).
fn apply_forward(tx: &Transaction<'_>, db_id: Uuid, op: &OpRow) -> Result<()> {
    let entity = op.entity_uuid;
    match op.op_type.as_str() {
        "create_entry" => {
            tx.execute(
                "INSERT INTO metadata (uuid, version) VALUES (?1, 0)",
                params![db::uuid_to_bytes(entity)],
            )?;
            tx.execute(
                "INSERT INTO metadata_db (metadata_uuid, db_id) VALUES (?1, ?2)",
                params![db::uuid_to_bytes(entity), db::uuid_to_bytes(db_id)],
            )?;
            for row in snapshots(tx, op.id, 1)? {
                db::insert_field_row(tx, entity, &row.name, &row.value, Some(row.id))?;
            }
        }
        "delete_entry" => {
            tx.execute(
                "DELETE FROM metadata WHERE uuid = ?1",
                params![db::uuid_to_bytes(entity)],
            )?;
        }
        "set_field" | "file_deleted" | "file_moved" | "file_modified" => {
            let field = op.field_name.as_deref().context("set-shaped op without field_name")?;
            tx.execute(
                "DELETE FROM field WHERE metadata_uuid = ?1 AND field_name = ?2",
                params![db::uuid_to_bytes(entity), field],
            )?;
            for row in snapshots(tx, op.id, 1)? {
                db::insert_field_row(tx, entity, &row.name, &row.value, Some(row.id))?;
            }
            restore_version(tx, entity, op.entity_version_before.map(|v| v + 1))?;
        }
        "append_field" => {
            for row in snapshots(tx, op.id, 1)? {
                db::insert_field_row(tx, entity, &row.name, &row.value, Some(row.id))?;
            }
            restore_version(tx, entity, op.entity_version_before.map(|v| v + 1))?;
        }
        "delete_field" => {
            for row in snapshots(tx, op.id, 0)? {
                tx.execute("DELETE FROM field WHERE id = ?1", params![row.id])?;
            }
            restore_version(tx, entity, op.entity_version_before.map(|v| v + 1))?;
        }
        "unknown" => anyhow::bail!("cannot navigate across an 'unknown' operation (op {})", op.id),
        other => anyhow::bail!("unsupported op_type '{other}' in the log"),
    }
    Ok(())
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
    for id in ordered {
        tx.execute("DELETE FROM operation WHERE id = ?1", params![id])?;
    }
    tx.execute(
        "DELETE FROM revision WHERE id NOT IN (SELECT DISTINCT rev_id FROM operation)",
        [],
    )?;
    tx.commit()?;
    let revisions_after: i64 =
        conn.query_row("SELECT COUNT(*) FROM revision", [], |r| r.get(0))?;

    Ok((to_delete.len(), (revisions_before - revisions_after) as usize))
}

/// A single logged write transaction. All changes made through one Writer
/// form one revision; commit is atomic. Dropping a Writer without committing
/// rolls everything back. After any method returns an error, the Writer must
/// be dropped (the whole revision is abandoned).
pub struct Writer<'c> {
    tx: Transaction<'c>,
    db_id: Uuid,
    rev_id: i64,
    head: Option<i64>,
    seq: i64,
}

impl<'c> Writer<'c> {
    /// Opens a transaction and creates the revision row.
    pub fn begin(
        conn: &'c mut rusqlite::Connection,
        db_id: Uuid,
        label: Option<String>,
    ) -> Result<Self> {
        let tx = conn.transaction()?;
        let head: Option<i64> =
            tx.query_row("SELECT op_id FROM log_head WHERE singleton = 1", [], |r| r.get(0))?;
        tx.execute(
            "INSERT INTO revision (timestamp, label) VALUES (?1, ?2)",
            params![now_ms(), label],
        )?;
        let rev_id = tx.last_insert_rowid();
        Ok(Self { tx, db_id, rev_id, head, seq: 0 })
    }

    pub fn rev_id(&self) -> i64 {
        self.rev_id
    }

    /// Read access to the underlying transaction, for lookups (tree cache,
    /// eligibility) that must observe the writes already applied.
    pub fn connection(&self) -> &rusqlite::Connection {
        &self.tx
    }

    /// Number of operations recorded so far in this revision.
    pub fn op_count(&self) -> i64 {
        self.seq
    }

    /// Removes every row of `(uuid, name)`, leaving the field unknown.
    /// Set-field shaped (before = all rows, after = none) so the standard
    /// `set_field` inverse applies. Used to invalidate `mfr_*` hashes.
    pub fn clear_field_as(&mut self, op_type: OpType, uuid: Uuid, name: &str) -> Result<()> {
        let before = db::get_field_rows_named(&self.tx, uuid, name)?;
        if before.is_empty() {
            return Ok(());
        }
        let version_before = self.bump_version(uuid)?;
        self.tx.execute(
            "DELETE FROM field WHERE metadata_uuid = ?1 AND field_name = ?2",
            params![db::uuid_to_bytes(uuid), name],
        )?;
        self.log_op(op_type, uuid, Some(name), Some(version_before), &before, &[])?;
        Ok(())
    }

    /// Creates a new entry owned by this repository.
    pub fn create_entry(&mut self, fields: Vec<Field>) -> Result<Metadata> {
        let uuid = Uuid::new_v4();
        for f in &fields {
            self.validate_tree_ref(uuid, &f.name, &f.value)?;
        }
        self.tx.execute(
            "INSERT INTO metadata (uuid, version) VALUES (?1, 0)",
            params![db::uuid_to_bytes(uuid)],
        )?;
        self.tx.execute(
            "INSERT INTO metadata_db (metadata_uuid, db_id) VALUES (?1, ?2)",
            params![db::uuid_to_bytes(uuid), db::uuid_to_bytes(self.db_id)],
        )?;

        let mut after = Vec::with_capacity(fields.len());
        let mut out_fields = Vec::with_capacity(fields.len());
        for f in fields {
            let id = db::insert_field_row(&self.tx, uuid, &f.name, &f.value, None)?;
            after.push(FieldRow { id, name: f.name.clone(), value: f.value.clone() });
            out_fields.push(Field { id: Some(id), ..f });
        }

        self.log_op(OpType::CreateEntry, uuid, None, None, &[], &after)?;
        Ok(Metadata { uuid, db_ids: vec![self.db_id], version: 0, fields: out_fields })
    }

    /// Deletes an entry and all its rows.
    pub fn delete_entry(&mut self, uuid: Uuid) -> Result<()> {
        let version = db::get_version(&self.tx, uuid)?
            .with_context(|| format!("Metadata entry not found: {uuid}"))?;
        let before = db::get_field_rows(&self.tx, uuid)?;
        // CASCADE removes field and metadata_db rows.
        self.tx
            .execute("DELETE FROM metadata WHERE uuid = ?1", params![db::uuid_to_bytes(uuid)])?;
        self.log_op(OpType::DeleteEntry, uuid, None, Some(version), &before, &[])?;
        Ok(())
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
        let version_before = self.bump_version(uuid)?;
        let before = db::get_field_rows_named(&self.tx, uuid, name)?;
        self.tx.execute(
            "DELETE FROM field WHERE metadata_uuid = ?1 AND field_name = ?2",
            params![db::uuid_to_bytes(uuid), name],
        )?;
        let id = db::insert_field_row(&self.tx, uuid, name, &value, None)?;
        let after = vec![FieldRow { id, name: name.to_string(), value }];
        self.log_op(op_type, uuid, Some(name), Some(version_before), &before, &after)?;
        Ok(())
    }

    /// Appends one row without touching existing rows of that name.
    /// Returns the new field row id.
    pub fn append_field(&mut self, uuid: Uuid, name: &str, value: Value) -> Result<i64> {
        self.validate_tree_ref(uuid, name, &value)?;
        let version_before = self.bump_version(uuid)?;
        let id = db::insert_field_row(&self.tx, uuid, name, &value, None)?;
        let after = vec![FieldRow { id, name: name.to_string(), value }];
        self.log_op(OpType::AppendField, uuid, Some(name), Some(version_before), &[], &after)?;
        Ok(id)
    }

    /// Replaces the single row identified by `field_id`, keeping its row id.
    /// Logged as a `delete_field` + `append_field` pair so that the inverse
    /// operations remain row-scoped (a `set_field` snapshot covers *all* rows
    /// of the name, which would clobber untouched sibling rows on rollback).
    pub fn replace_field(&mut self, uuid: Uuid, field_id: i64, value: Value) -> Result<()> {
        let old = self.get_owned_row(uuid, field_id)?;
        self.validate_tree_ref(uuid, &old.name, &value)?;

        let v1 = self.bump_version(uuid)?;
        self.tx.execute("DELETE FROM field WHERE id = ?1", params![field_id])?;
        self.log_op(
            OpType::DeleteField,
            uuid,
            Some(&old.name.clone()),
            Some(v1),
            std::slice::from_ref(&old),
            &[],
        )?;

        let v2 = self.bump_version(uuid)?;
        db::insert_field_row(&self.tx, uuid, &old.name, &value, Some(field_id))?;
        let after = vec![FieldRow { id: field_id, name: old.name.clone(), value }];
        self.log_op(OpType::AppendField, uuid, Some(&old.name), Some(v2), &[], &after)?;
        Ok(())
    }

    /// Removes the single row identified by `field_id`.
    pub fn delete_field(&mut self, uuid: Uuid, field_id: i64) -> Result<()> {
        let old = self.get_owned_row(uuid, field_id)?;
        let version_before = self.bump_version(uuid)?;
        self.tx.execute("DELETE FROM field WHERE id = ?1", params![field_id])?;
        self.log_op(
            OpType::DeleteField,
            uuid,
            Some(&old.name.clone()),
            Some(version_before),
            &[old],
            &[],
        )?;
        Ok(())
    }

    /// Writes the final HEAD and commits the transaction.
    pub fn commit(self) -> Result<()> {
        if self.seq == 0 {
            // Nothing was written: drop the empty revision, leave HEAD alone.
            self.tx.execute("DELETE FROM revision WHERE id = ?1", params![self.rev_id])?;
        } else {
            self.tx.execute(
                "UPDATE log_head SET op_id = ?1 WHERE singleton = 1",
                params![self.head],
            )?;
        }
        self.tx.commit().context("Failed to commit write transaction")
    }

    // ── Internals ────────────────────────────────────────────────────────────

    /// Increments `metadata.version` and returns the value before the bump.
    fn bump_version(&self, uuid: Uuid) -> Result<u64> {
        let before = db::get_version(&self.tx, uuid)?
            .with_context(|| format!("Metadata entry not found: {uuid}"))?;
        self.tx.execute(
            "UPDATE metadata SET version = version + 1 WHERE uuid = ?1",
            params![db::uuid_to_bytes(uuid)],
        )?;
        Ok(before)
    }

    /// Fetches a field row, checking it belongs to the given entry.
    fn get_owned_row(&self, uuid: Uuid, field_id: i64) -> Result<FieldRow> {
        db::get_field_rows(&self.tx, uuid)?
            .into_iter()
            .find(|r| r.id == field_id)
            .with_context(|| format!("Field {field_id} not found on entry {uuid}"))
    }

    /// Inserts the operation row and its snapshots, advancing the running HEAD.
    fn log_op(
        &mut self,
        op_type: OpType,
        entity: Uuid,
        field_name: Option<&str>,
        version_before: Option<u64>,
        before: &[FieldRow],
        after: &[FieldRow],
    ) -> Result<i64> {
        self.seq += 1;
        self.tx.execute(
            "INSERT INTO operation
                 (parent_id, rev_id, seq, op_type, entity_uuid, entity_version_before, field_name)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                self.head,
                self.rev_id,
                self.seq,
                op_type.as_str(),
                db::uuid_to_bytes(entity),
                version_before.map(|v| v as i64),
                field_name
            ],
        )?;
        let op_id = self.tx.last_insert_rowid();
        for row in before {
            self.insert_snapshot(op_id, 0, row)?;
        }
        for row in after {
            self.insert_snapshot(op_id, 1, row)?;
        }
        self.head = Some(op_id);
        Ok(op_id)
    }

    fn insert_snapshot(&self, op_id: i64, is_new: i64, row: &FieldRow) -> Result<()> {
        let e = db::encode_value(&row.value);
        self.tx.execute(
            "INSERT INTO op_snapshot
                 (op_id, is_new, field_id, field_name, value_type, value_text,
                  value_int, value_real, value_uuid, value_ref_repo, value_name)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                op_id,
                is_new,
                row.id,
                row.name,
                e.value_type,
                e.text,
                e.int,
                e.real,
                e.uuid,
                e.ref_repo,
                e.name
            ],
        )?;
        Ok(())
    }

    /// For TreeRef values: the parent must be null (root) or an existing entry
    /// carrying a TreeRef of the same field name; the write must not create a
    /// cycle nor exceed [`MAX_TREE_DEPTH`] (spec-main invariants).
    fn validate_tree_ref(&self, entry: Uuid, field_name: &str, value: &Value) -> Result<()> {
        let Value::TreeRef { parent, .. } = value else {
            return Ok(());
        };
        let Some(parent) = parent else {
            return Ok(()); // Root node: nothing to check.
        };
        if *parent == entry {
            bail!("TreeRef write would create a cycle on '{field_name}'");
        }
        let parent_positions = db::get_tree_parents(&self.tx, field_name, *parent)?;
        if parent_positions.is_empty() {
            bail!(
                "invalid TreeRef parent {parent}: no such entry carrying a \
                 '{field_name}' TreeRef field"
            );
        }

        // Walk every ancestor chain (multi-map fields make this a DAG walk):
        // detect cycles through the entry being written and measure depth.
        let mut visited: HashSet<Uuid> = HashSet::new();
        let mut frontier = vec![*parent];
        let mut chain_len = 1; // The parent itself.
        loop {
            let mut next = Vec::new();
            for node in frontier {
                if node == entry {
                    bail!("TreeRef write would create a cycle on '{field_name}'");
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
                bail!("TreeRef depth exceeds {MAX_TREE_DEPTH}");
            }
            frontier = next;
        }
        // The new node sits one level below the deepest ancestor chain.
        if chain_len + 1 > MAX_TREE_DEPTH {
            bail!("TreeRef depth exceeds {MAX_TREE_DEPTH}");
        }
        Ok(())
    }
}
