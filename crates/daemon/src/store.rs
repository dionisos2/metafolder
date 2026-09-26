//! The storage boundary (docs/spec-storage.org "Increment 2, concretely"):
//! what the daemon asks of a repository's database, as the traits a storage
//! backend implements. `Rows` is the data model's primary state — metarecords
//! and their field rows, whose ids are part of the model (`Field.id`, restored
//! by navigation); `Log` is the event log. The questions a query answers are
//! not here: they belong to the index.
//!
//! SQLite is the one backend today, implemented directly on
//! `rusqlite::Connection` (a transaction dereferences to one). The traits grow
//! as the modules that still carry a connection move onto them.

use std::collections::HashMap;

use anyhow::{Context, Result};
use metafolder_core::metarecord::Value;
use rusqlite::{params, Connection, Transaction};
use uuid::Uuid;

use crate::db::{self, FieldRow, TreeRow};
use crate::log::{self, OpRow, OpType, Retention};

/// Metarecords and their field rows.
pub trait Rows {
    /// A metarecord's version, `None` when it does not exist.
    fn version(&self, uuid: Uuid) -> Result<Option<u64>>;
    /// A metarecord's field rows, in row-id order.
    fn rows(&self, uuid: Uuid) -> Result<Vec<FieldRow>>;
    /// A metarecord's rows of one field name, in row-id order.
    fn rows_named(&self, uuid: Uuid, name: &str) -> Result<Vec<FieldRow>>;
    /// Several metarecords' rows at once.
    fn rows_for(&self, uuids: &[Uuid]) -> Result<HashMap<Uuid, Vec<FieldRow>>>;
    /// A row by its id.
    fn row(&self, id: i64) -> Result<Option<FieldRow>>;
    /// The metarecord a row belongs to.
    fn owner_of_row(&self, id: i64) -> Result<Option<Uuid>>;
    /// Every metarecord.
    fn metarecords(&self) -> Result<Vec<Uuid>>;
    /// Every row with its metarecord, in row-id order — the load's one pass.
    fn for_each_row(&self, f: &mut dyn FnMut(Uuid, FieldRow) -> Result<()>) -> Result<()>;
    /// The largest row id (`0` when there is none): a scan's progress bound.
    fn max_row_id(&self) -> Result<i64>;
    /// The value types a field name holds (`Nothing` aside), sorted — one,
    /// by the data model's invariant, save during a `retype`.
    fn value_types(&self, name: &str) -> Result<Vec<String>>;
    /// The metarecords holding at least one row of a field name.
    fn holders(&self, name: &str) -> Result<Vec<Uuid>>;
    /// The direct children of `parent` in `field`'s forest, `(uuid, name)`.
    fn children(&self, field: &str, parent: Uuid) -> Result<Vec<(Uuid, String)>>;
    /// Every `tree_ref` position, grouped by field name and metarecord, a
    /// metarecord's positions in row-id order (the order a load places them
    /// in; the groups themselves come in no promised order).
    fn forest(&self) -> Result<Vec<TreeRow>>;
}

/// The event log.
pub trait Log {
    /// The operation HEAD names, `None` on an empty log.
    fn head(&self) -> Result<Option<i64>>;
    /// One operation.
    fn op(&self, id: i64) -> Result<Option<OpRow>>;
    /// An operation's snapshot rows: the state after it (`after`) or before.
    fn snapshots(&self, op_id: i64, after: bool) -> Result<Vec<FieldRow>>;
    /// The operations from `from` back to — not including — `until`, newest
    /// first; `None` when `until` is not an ancestor within `max` steps.
    fn ops_until(&self, from: i64, until: i64, max: usize) -> Result<Option<Vec<OpRow>>>;
}

/// A repository's database: both halves.
pub trait Store: Rows + Log {}

impl<T: Rows + Log + ?Sized> Store for T {}

/// One operation a write transaction appends to the log.
pub struct NewOp {
    pub op_type: OpType,
    pub entity: Uuid,
    pub field_name: Option<String>,
    pub version_before: Option<u64>,
    pub version_after: Option<u64>,
    pub before: Vec<FieldRow>,
    pub after: Vec<FieldRow>,
    /// The operation this one undoes, when a revert writes it.
    pub reverts_op_id: Option<i64>,
}

/// A write transaction: everything it writes commits together or not at all,
/// and it reads its own writes. One `log::Writer` revision is one of these.
pub trait WriteTxn: Store {
    /// A new metarecord, with no row yet.
    fn create_metarecord(&self, uuid: Uuid, version: u64) -> Result<()>;
    /// Removes a metarecord and every row it holds.
    fn remove_metarecord(&self, uuid: Uuid) -> Result<()>;
    fn set_version(&self, uuid: Uuid, version: u64) -> Result<()>;
    /// Inserts a row and returns its id: a fresh one, never reused, or `id`
    /// when given (navigation puts rows back under their own). Refuses a
    /// second row at one position of a forest, and a second `mfr_path`.
    fn insert_row(&self, uuid: Uuid, name: &str, value: &Value, id: Option<i64>) -> Result<i64>;
    fn delete_row(&self, id: i64) -> Result<()>;
    /// Deletes a metarecord's rows — all of them, or those of one name.
    fn delete_rows(&self, uuid: Uuid, name: Option<&str>) -> Result<()>;

    /// A new revision; its id.
    fn begin_revision(&self, label: Option<&str>, timestamp_ms: i64) -> Result<i64>;
    fn set_revision_origin(&self, rev: i64, origin: &str) -> Result<()>;
    /// Drops a revision no operation was recorded in.
    fn drop_revision(&self, rev: i64) -> Result<()>;
    /// Appends `ops` to revision `rev`, chained from `parent`, numbered from
    /// `first_seq`; returns the id the last one got. Ids are never reused.
    fn append_ops(
        &self,
        rev: i64,
        parent: Option<i64>,
        first_seq: i64,
        ops: &[NewOp],
    ) -> Result<i64>;
    fn set_head(&self, op: Option<i64>) -> Result<()>;
    /// Drops the history `retention` no longer keeps behind `head`.
    fn trim(&self, retention: Retention, head: i64) -> Result<usize>;

    fn commit(self: Box<Self>) -> Result<()>;

    /// The SQLite connection underneath, for the callers that still read
    /// through it. Transitional: they move onto the traits, and this goes.
    fn as_sqlite(&self) -> Option<&Connection> {
        None
    }
}

/// Opens write transactions.
pub trait Begin {
    fn begin_write(&mut self) -> Result<Box<dyn WriteTxn + '_>>;
}

impl Begin for Connection {
    fn begin_write(&mut self) -> Result<Box<dyn WriteTxn + '_>> {
        Ok(Box::new(SqliteTxn(self.transaction()?)))
    }
}

/// A SQLite write transaction.
pub struct SqliteTxn<'c>(Transaction<'c>);

/// Stays under SQLITE_MAX_VARIABLE_NUMBER (32766 for the bundled SQLite).
const MAX_PARAMS: usize = 16_000;

fn bulk_insert(
    tx: &Transaction<'_>,
    insert_sql: &str,
    row_width: usize,
    rows: &[Vec<rusqlite::types::Value>],
) -> Result<()> {
    let rows_per_chunk = (MAX_PARAMS / row_width).max(1);
    let row_placeholder = format!("({})", vec!["?"; row_width].join(", "));
    for chunk in rows.chunks(rows_per_chunk) {
        let placeholders = vec![row_placeholder.as_str(); chunk.len()].join(", ");
        let sql = format!("{insert_sql} VALUES {placeholders}");
        tx.execute(&sql, rusqlite::params_from_iter(chunk.iter().flatten()))?;
    }
    Ok(())
}

impl Rows for SqliteTxn<'_> {
    fn version(&self, uuid: Uuid) -> Result<Option<u64>> {
        Rows::version(&*self.0, uuid)
    }
    fn rows(&self, uuid: Uuid) -> Result<Vec<FieldRow>> {
        Rows::rows(&*self.0, uuid)
    }
    fn rows_named(&self, uuid: Uuid, name: &str) -> Result<Vec<FieldRow>> {
        Rows::rows_named(&*self.0, uuid, name)
    }
    fn rows_for(&self, uuids: &[Uuid]) -> Result<HashMap<Uuid, Vec<FieldRow>>> {
        Rows::rows_for(&*self.0, uuids)
    }
    fn row(&self, id: i64) -> Result<Option<FieldRow>> {
        Rows::row(&*self.0, id)
    }
    fn owner_of_row(&self, id: i64) -> Result<Option<Uuid>> {
        Rows::owner_of_row(&*self.0, id)
    }
    fn metarecords(&self) -> Result<Vec<Uuid>> {
        Rows::metarecords(&*self.0)
    }
    fn for_each_row(&self, f: &mut dyn FnMut(Uuid, FieldRow) -> Result<()>) -> Result<()> {
        Rows::for_each_row(&*self.0, f)
    }
    fn max_row_id(&self) -> Result<i64> {
        Rows::max_row_id(&*self.0)
    }
    fn value_types(&self, name: &str) -> Result<Vec<String>> {
        Rows::value_types(&*self.0, name)
    }
    fn holders(&self, name: &str) -> Result<Vec<Uuid>> {
        Rows::holders(&*self.0, name)
    }
    fn children(&self, field: &str, parent: Uuid) -> Result<Vec<(Uuid, String)>> {
        Rows::children(&*self.0, field, parent)
    }
    fn forest(&self) -> Result<Vec<TreeRow>> {
        Rows::forest(&*self.0)
    }
}

impl Log for SqliteTxn<'_> {
    fn head(&self) -> Result<Option<i64>> {
        Log::head(&*self.0)
    }
    fn op(&self, id: i64) -> Result<Option<OpRow>> {
        Log::op(&*self.0, id)
    }
    fn snapshots(&self, op_id: i64, after: bool) -> Result<Vec<FieldRow>> {
        Log::snapshots(&*self.0, op_id, after)
    }
    fn ops_until(&self, from: i64, until: i64, max: usize) -> Result<Option<Vec<OpRow>>> {
        Log::ops_until(&*self.0, from, until, max)
    }
}

impl WriteTxn for SqliteTxn<'_> {
    fn create_metarecord(&self, uuid: Uuid, version: u64) -> Result<()> {
        self.0
            .prepare_cached("INSERT INTO metarecord (uuid, version) VALUES (?1, ?2)")?
            .execute(params![db::uuid_to_bytes(uuid), version as i64])?;
        Ok(())
    }
    fn remove_metarecord(&self, uuid: Uuid) -> Result<()> {
        // The rows go with it (`ON DELETE CASCADE`).
        self.0
            .execute("DELETE FROM metarecord WHERE uuid = ?1", params![db::uuid_to_bytes(uuid)])?;
        Ok(())
    }
    fn set_version(&self, uuid: Uuid, version: u64) -> Result<()> {
        self.0
            .prepare_cached("UPDATE metarecord SET version = ?1 WHERE uuid = ?2")?
            .execute(params![version as i64, db::uuid_to_bytes(uuid)])?;
        Ok(())
    }
    fn insert_row(&self, uuid: Uuid, name: &str, value: &Value, id: Option<i64>) -> Result<i64> {
        db::insert_field_row(&self.0, uuid, name, value, id)
    }
    fn delete_row(&self, id: i64) -> Result<()> {
        self.0.execute("DELETE FROM field WHERE id = ?1", params![id])?;
        Ok(())
    }
    fn delete_rows(&self, uuid: Uuid, name: Option<&str>) -> Result<()> {
        match name {
            Some(name) => self
                .0
                .prepare_cached("DELETE FROM field WHERE metarecord_uuid = ?1 AND field_name = ?2")?
                .execute(params![db::uuid_to_bytes(uuid), name])?,
            None => self
                .0
                .prepare_cached("DELETE FROM field WHERE metarecord_uuid = ?1")?
                .execute(params![db::uuid_to_bytes(uuid)])?,
        };
        Ok(())
    }
    fn begin_revision(&self, label: Option<&str>, timestamp_ms: i64) -> Result<i64> {
        self.0.execute(
            "INSERT INTO revision (timestamp, label) VALUES (?1, ?2)",
            params![timestamp_ms, label],
        )?;
        Ok(self.0.last_insert_rowid())
    }
    fn set_revision_origin(&self, rev: i64, origin: &str) -> Result<()> {
        self.0.execute("UPDATE revision SET origin = ?1 WHERE id = ?2", params![origin, rev])?;
        Ok(())
    }
    fn drop_revision(&self, rev: i64) -> Result<()> {
        self.0.execute("DELETE FROM revision WHERE id = ?1", params![rev])?;
        Ok(())
    }
    /// Operation ids are assigned up front from `sqlite_sequence` so the parent
    /// chain is known before inserting; explicit-id inserts into an
    /// AUTOINCREMENT table keep the sequence in step, preserving the
    /// never-reused-id guarantee. All rows go in as a few multi-row inserts
    /// (spec-event-log "Normal write flow").
    fn append_ops(
        &self,
        rev: i64,
        parent: Option<i64>,
        first_seq: i64,
        ops: &[NewOp],
    ) -> Result<i64> {
        use rusqlite::types::Value as Sql;
        use rusqlite::OptionalExtension as _;

        let last_id: Option<i64> = self
            .0
            .query_row("SELECT seq FROM sqlite_sequence WHERE name = 'operation'", [], |r| r.get(0))
            .optional()?;
        let base = last_id.unwrap_or(0) + 1;
        let mut op_rows: Vec<Vec<Sql>> = Vec::with_capacity(ops.len());
        let mut snapshot_rows: Vec<Vec<Sql>> = Vec::new();
        for (i, op) in ops.iter().enumerate() {
            let op_id = base + i as i64;
            let parent = if i == 0 { parent } else { Some(op_id - 1) };
            op_rows.push(vec![
                Sql::Integer(op_id),
                parent.map_or(Sql::Null, Sql::Integer),
                Sql::Integer(rev),
                Sql::Integer(first_seq + i as i64),
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
            &self.0,
            "INSERT INTO operation
                 (id, parent_id, rev_id, seq, op_type, entity_uuid,
                  entity_version_before, entity_version_after, field_name,
                  reverts_op_id)",
            10,
            &op_rows,
        )?;
        bulk_insert(
            &self.0,
            "INSERT INTO op_snapshot
                 (op_id, is_new, field_id, field_name, value_type, value_text,
                  value_int, value_real, value_uuid, value_ref_repo, value_name,
                  value_name_bytes)",
            12,
            &snapshot_rows,
        )?;
        Ok(base + ops.len() as i64 - 1)
    }
    fn set_head(&self, op: Option<i64>) -> Result<()> {
        self.0.execute("UPDATE log_head SET op_id = ?1 WHERE singleton = 1", params![op])?;
        Ok(())
    }
    fn trim(&self, retention: Retention, head: i64) -> Result<usize> {
        log::trim(&self.0, retention, head)
    }
    fn commit(self: Box<Self>) -> Result<()> {
        self.0.commit().context("Failed to commit write transaction")
    }
    fn as_sqlite(&self) -> Option<&Connection> {
        Some(&self.0)
    }
}

impl Rows for Connection {
    fn version(&self, uuid: Uuid) -> Result<Option<u64>> {
        db::get_version(self, uuid)
    }
    fn rows(&self, uuid: Uuid) -> Result<Vec<FieldRow>> {
        db::get_field_rows(self, uuid)
    }
    fn rows_named(&self, uuid: Uuid, name: &str) -> Result<Vec<FieldRow>> {
        db::get_field_rows_named(self, uuid, name)
    }
    fn rows_for(&self, uuids: &[Uuid]) -> Result<HashMap<Uuid, Vec<FieldRow>>> {
        db::field_rows_for(self, uuids)
    }
    fn row(&self, id: i64) -> Result<Option<FieldRow>> {
        db::get_field_row_by_id(self, id)
    }
    fn owner_of_row(&self, id: i64) -> Result<Option<Uuid>> {
        db::metarecord_of_field(self, id)
    }
    fn metarecords(&self) -> Result<Vec<Uuid>> {
        db::list_entries(self)
    }
    fn for_each_row(&self, f: &mut dyn FnMut(Uuid, FieldRow) -> Result<()>) -> Result<()> {
        db::for_each_field_row(self, f)
    }
    fn max_row_id(&self) -> Result<i64> {
        db::max_field_id(self)
    }
    fn value_types(&self, name: &str) -> Result<Vec<String>> {
        db::distinct_value_types(self, name)
    }
    fn holders(&self, name: &str) -> Result<Vec<Uuid>> {
        db::metarecords_with_field(self, name)
    }
    fn children(&self, field: &str, parent: Uuid) -> Result<Vec<(Uuid, String)>> {
        db::tree_children(self, field, parent)
    }
    fn forest(&self) -> Result<Vec<TreeRow>> {
        db::load_tree_forest(self)
    }
}

impl Log for Connection {
    fn head(&self) -> Result<Option<i64>> {
        db::current_head(self)
    }
    fn op(&self, id: i64) -> Result<Option<OpRow>> {
        log::get_op(self, id)
    }
    fn snapshots(&self, op_id: i64, after: bool) -> Result<Vec<FieldRow>> {
        log::snapshots(self, op_id, after as i64)
    }
    fn ops_until(&self, from: i64, until: i64, max: usize) -> Result<Option<Vec<OpRow>>> {
        log::ancestry_ops_until(self, from, until, max)
    }
}
