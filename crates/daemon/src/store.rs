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
use metafolder_core::metarecord::{TreeName, Value};
use rusqlite::{params, Connection, Transaction};
use uuid::Uuid;

use crate::db::{self, FieldRow, TreeRow};
use crate::log::{self, Delta, OpRow, OpType, Retention};

/// Implements `Rows` and `Log` for a type holding a SQLite connection, by
/// handing every call to the `Connection` implementation.
macro_rules! forward_to_connection {
    ($ty:ty, |$me:ident| $conn:expr) => {
        impl Rows for $ty {
            fn version(&self, uuid: Uuid) -> Result<Option<u64>> {
                let $me = self;
                Rows::version($conn, uuid)
            }
            fn rows(&self, uuid: Uuid) -> Result<Vec<FieldRow>> {
                let $me = self;
                Rows::rows($conn, uuid)
            }
            fn rows_named(&self, uuid: Uuid, name: &str) -> Result<Vec<FieldRow>> {
                let $me = self;
                Rows::rows_named($conn, uuid, name)
            }
            fn rows_for(&self, uuids: &[Uuid]) -> Result<HashMap<Uuid, Vec<FieldRow>>> {
                let $me = self;
                Rows::rows_for($conn, uuids)
            }
            fn row(&self, id: i64) -> Result<Option<FieldRow>> {
                let $me = self;
                Rows::row($conn, id)
            }
            fn owner_of_row(&self, id: i64) -> Result<Option<Uuid>> {
                let $me = self;
                Rows::owner_of_row($conn, id)
            }
            fn metarecords(&self) -> Result<Vec<Uuid>> {
                let $me = self;
                Rows::metarecords($conn)
            }
            fn for_each_row(&self, f: &mut dyn FnMut(Uuid, FieldRow) -> Result<()>) -> Result<()> {
                let $me = self;
                Rows::for_each_row($conn, f)
            }
            fn max_row_id(&self) -> Result<i64> {
                let $me = self;
                Rows::max_row_id($conn)
            }
            fn value_types(&self, name: &str) -> Result<Vec<String>> {
                let $me = self;
                Rows::value_types($conn, name)
            }
            fn holders(&self, name: &str) -> Result<Vec<Uuid>> {
                let $me = self;
                Rows::holders($conn, name)
            }
            fn children(&self, field: &str, parent: Uuid) -> Result<Vec<(Uuid, String)>> {
                let $me = self;
                Rows::children($conn, field, parent)
            }
            fn forest(&self) -> Result<Vec<TreeRow>> {
                let $me = self;
                Rows::forest($conn)
            }
        }

        impl Log for $ty {
            fn head(&self) -> Result<Option<i64>> {
                let $me = self;
                Log::head($conn)
            }
            fn op(&self, id: i64) -> Result<Option<OpRow>> {
                let $me = self;
                Log::op($conn, id)
            }
            fn snapshots(&self, op_id: i64, after: bool) -> Result<Vec<FieldRow>> {
                let $me = self;
                Log::snapshots($conn, op_id, after)
            }
            fn ops_until(&self, from: i64, until: i64, max: usize) -> Result<Delta> {
                let $me = self;
                Log::ops_until($conn, from, until, max)
            }
            fn ancestry(&self, from: i64) -> Result<Vec<i64>> {
                let $me = self;
                Log::ancestry($conn, from)
            }
            fn restorations(&self) -> Result<Vec<(i64, Restoration)>> {
                let $me = self;
                Log::restorations($conn)
            }
            fn ancestry_ops(&self, from: i64, max: Option<usize>) -> Result<Vec<OpRow>> {
                let $me = self;
                Log::ancestry_ops($conn, from, max)
            }
            fn all_ops(&self) -> Result<Vec<OpRow>> {
                let $me = self;
                Log::all_ops($conn)
            }
            fn active_line(&self, head: i64) -> Result<Vec<OpRow>> {
                let $me = self;
                Log::active_line($conn, head)
            }
            fn has_children(&self, op: i64) -> Result<bool> {
                let $me = self;
                Log::has_children($conn, op)
            }
            fn revisions(&self, ids: &[i64]) -> Result<HashMap<i64, RevisionMeta>> {
                let $me = self;
                Log::revisions($conn, ids)
            }
            fn counts(&self) -> Result<(i64, i64)> {
                let $me = self;
                Log::counts($conn)
            }
            fn revision_ops(&self, rev: i64) -> Result<Vec<OpRow>> {
                let $me = self;
                Log::revision_ops($conn, rev)
            }
            fn entity_ops_after(&self, entity: Uuid, after: i64) -> Result<Vec<OpRow>> {
                let $me = self;
                Log::entity_ops_after($conn, entity, after)
            }
        }
    };
}

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
    /// first: [`Delta::Found`]; or `until` not met within `max` steps
    /// ([`Delta::Budget`]), or not an ancestor at all ([`Delta::Unrelated`]).
    fn ops_until(&self, from: i64, until: i64, max: usize) -> Result<Delta>;
    /// The whole ancestor chain from `from` (inclusive) to the root.
    fn ancestry(&self, from: i64) -> Result<Vec<i64>>;
    /// The restorations skipped navigation steps queued, oldest first, with
    /// the position each holds in the queue.
    fn restorations(&self) -> Result<Vec<(i64, Restoration)>>;
    /// The ancestor chain from `from` (inclusive), newest first — whole, or
    /// its first `max` operations.
    fn ancestry_ops(&self, from: i64, max: Option<usize>) -> Result<Vec<OpRow>>;
    /// Every operation, whatever branch it is on, oldest first.
    fn all_ops(&self) -> Result<Vec<OpRow>>;
    /// The active line through `head`: its ancestors, then the branch that
    /// continues below it (spec-event-log "Active line"), oldest first.
    fn active_line(&self, head: i64) -> Result<Vec<OpRow>>;
    /// Whether an operation has a child (a continuation below it).
    fn has_children(&self, op: i64) -> Result<bool>;
    /// The given revisions' metadata; ids that name none are left out.
    fn revisions(&self, ids: &[i64]) -> Result<HashMap<i64, RevisionMeta>>;
    /// How many operations and revisions the log holds.
    fn counts(&self) -> Result<(i64, i64)>;
    /// A revision's operations, oldest first.
    fn revision_ops(&self, rev: i64) -> Result<Vec<OpRow>>;
    /// An entity's operations newer than `after`, whatever branch they are on,
    /// oldest first.
    fn entity_ops_after(&self, entity: Uuid, after: i64) -> Result<Vec<OpRow>>;
}

/// A revision's metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionMeta {
    /// Unix milliseconds.
    pub timestamp: i64,
    pub label: Option<String>,
    /// Who wrote it: `watcher` for the daemon's own, `None` for a client's.
    pub origin: Option<String>,
}

/// A filesystem fact a skipped step of a coordinated navigation leaves to be
/// re-recorded once the navigation lock is released (spec-event-log "skip").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Restoration {
    /// The file is where this position says.
    SetPath { entity: Uuid, parent: Option<Uuid>, name: TreeName },
    /// The file is gone.
    ClearPath { entity: Uuid },
    /// The file's content changed: its hashes no longer hold.
    ClearHashes { entity: Uuid },
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
    /// Sets or clears a revision's label; `false` when there is no such
    /// revision.
    fn set_revision_label(&self, rev: i64, label: Option<&str>) -> Result<bool>;
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
    /// Removes every metarecord (a navigation to the empty state).
    fn clear_metarecords(&self) -> Result<()>;
    fn queue_restoration(&self, restoration: &Restoration) -> Result<()>;
    /// Drops the queued restorations up to position `up_to`, included.
    fn drop_restorations(&self, up_to: i64) -> Result<()>;

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

forward_to_connection!(SqliteTxn<'_>, |t| &*t.0);
forward_to_connection!(std::sync::MutexGuard<'_, Connection>, |g| &**g);

impl Begin for std::sync::MutexGuard<'_, Connection> {
    fn begin_write(&mut self) -> Result<Box<dyn WriteTxn + '_>> {
        Begin::begin_write(&mut **self)
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
    fn set_revision_label(&self, rev: i64, label: Option<&str>) -> Result<bool> {
        let changed =
            self.0.execute("UPDATE revision SET label = ?1 WHERE id = ?2", params![label, rev])?;
        Ok(changed > 0)
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
    fn clear_metarecords(&self) -> Result<()> {
        self.0.execute("DELETE FROM metarecord", [])?;
        Ok(())
    }
    fn queue_restoration(&self, restoration: &Restoration) -> Result<()> {
        let hex = |u: &Uuid| u.as_simple().to_string();
        match restoration {
            Restoration::SetPath { entity, parent, name } => self.0.execute(
                "INSERT INTO pending_operation (op_type, path, from_path, to_path)
                 VALUES ('restore_set_path', ?1, ?2, ?3)",
                params![
                    hex(entity),
                    parent.as_ref().map(hex).unwrap_or_default(),
                    name.display().as_ref()
                ],
            )?,
            Restoration::ClearPath { entity } => self.0.execute(
                "INSERT INTO pending_operation (op_type, path) VALUES ('restore_clear_path', ?1)",
                params![hex(entity)],
            )?,
            Restoration::ClearHashes { entity } => self.0.execute(
                "INSERT INTO pending_operation (op_type, path) VALUES ('restore_clear_hashes', ?1)",
                params![hex(entity)],
            )?,
        };
        Ok(())
    }
    fn drop_restorations(&self, up_to: i64) -> Result<()> {
        self.0.execute(
            "DELETE FROM pending_operation WHERE id <= ?1 AND op_type LIKE 'restore_%'",
            params![up_to],
        )?;
        Ok(())
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
    fn ops_until(&self, from: i64, until: i64, max: usize) -> Result<Delta> {
        log::delta_until(self, from, until, max)
    }
    fn ancestry(&self, from: i64) -> Result<Vec<i64>> {
        log::ancestry(self, from)
    }
    fn ancestry_ops(&self, from: i64, max: Option<usize>) -> Result<Vec<OpRow>> {
        match max {
            Some(max) => log::ancestry_ops_limited(self, from, max),
            None => log::ancestry_ops(self, from),
        }
    }
    fn all_ops(&self) -> Result<Vec<OpRow>> {
        log::all_ops(self)
    }
    fn active_line(&self, head: i64) -> Result<Vec<OpRow>> {
        log::active_line_ops(self, head)
    }
    fn has_children(&self, op: i64) -> Result<bool> {
        log::has_children(self, op)
    }
    /// A few hundred ids per `IN (…)` (SQLite allows 32 766 parameters),
    /// which keeps the prepared-statement cache useful on a large window.
    fn revisions(&self, ids: &[i64]) -> Result<HashMap<i64, RevisionMeta>> {
        const CHUNK: usize = 256;
        let mut out = HashMap::with_capacity(ids.len());
        for chunk in ids.chunks(CHUNK) {
            let placeholders = std::iter::repeat_n("?", chunk.len()).collect::<Vec<_>>().join(",");
            let mut stmt = self.prepare_cached(&format!(
                "SELECT id, timestamp, label, origin FROM revision WHERE id IN ({placeholders})"
            ))?;
            let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |r| {
                Ok((r.get::<_, i64>(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?;
            for row in rows {
                let (id, timestamp, label, origin) = row?;
                out.insert(id, RevisionMeta { timestamp, label, origin });
            }
        }
        Ok(out)
    }
    fn counts(&self) -> Result<(i64, i64)> {
        let ops = self.query_row("SELECT COUNT(*) FROM operation", [], |r| r.get(0))?;
        let revs = self.query_row("SELECT COUNT(*) FROM revision", [], |r| r.get(0))?;
        Ok((ops, revs))
    }
    fn revision_ops(&self, rev: i64) -> Result<Vec<OpRow>> {
        let ids: Vec<i64> = self
            .prepare_cached("SELECT id FROM operation WHERE rev_id = ?1 ORDER BY seq, id")?
            .query_map(params![rev], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        ids.into_iter()
            .map(|id| {
                log::get_op(self, id)?
                    .ok_or_else(|| anyhow::anyhow!("operation {id} vanished from revision {rev}"))
            })
            .collect()
    }
    /// Served by `idx_operation_entity (entity_uuid, id)`.
    fn entity_ops_after(&self, entity: Uuid, after: i64) -> Result<Vec<OpRow>> {
        let ids: Vec<i64> = self
            .prepare_cached(
                "SELECT id FROM operation WHERE entity_uuid = ?1 AND id > ?2 ORDER BY id",
            )?
            .query_map(params![db::uuid_to_bytes(entity), after], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        ids.into_iter()
            .map(|id| {
                log::get_op(self, id)?.ok_or_else(|| anyhow::anyhow!("operation {id} vanished"))
            })
            .collect()
    }
    fn restorations(&self) -> Result<Vec<(i64, Restoration)>> {
        let mut stmt = self.prepare(
            "SELECT id, op_type, path, from_path, to_path FROM pending_operation
             WHERE op_type LIKE 'restore_%' ORDER BY id",
        )?;
        type Raw = (i64, String, Option<String>, Option<String>, Option<String>);
        let raw: Vec<Raw> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let parse = |s: &str| -> Result<Uuid> {
            Uuid::parse_str(s).with_context(|| format!("invalid uuid in restoration op: {s}"))
        };
        raw.into_iter()
            .map(|(id, op_type, path, from_path, to_path)| {
                let entity = parse(path.as_deref().context("restoration op missing entity")?)?;
                let r = match op_type.as_str() {
                    "restore_set_path" => Restoration::SetPath {
                        entity,
                        parent: match from_path.as_deref() {
                            Some(p) if !p.is_empty() => Some(parse(p)?),
                            _ => None,
                        },
                        name: to_path.unwrap_or_default().into(),
                    },
                    "restore_clear_path" => Restoration::ClearPath { entity },
                    "restore_clear_hashes" => Restoration::ClearHashes { entity },
                    other => anyhow::bail!("unknown restoration op_type '{other}'"),
                };
                Ok((id, r))
            })
            .collect()
    }
}
