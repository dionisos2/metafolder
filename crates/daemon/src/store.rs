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

use anyhow::Result;
use rusqlite::Connection;
use uuid::Uuid;

use crate::db::{self, FieldRow, TreeRow};
use crate::log::{self, OpRow};

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
