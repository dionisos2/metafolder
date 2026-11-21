//! The storage boundary (doc "The storage boundary"):
//! what the daemon asks of a repository's database, as the traits a storage
//! backend implements. `Rows` is the data model's primary state — metarecords
//! and their field rows, whose ids are part of the model (`Field.id`); `Log`
//! is the event log. The questions a query answers are
//! not here: they belong to the index.
//!
//! The key-value store (`crate::kvstore`, LMDB) is the one backend; the traits
//! are what a second one would implement (`tests/store_contract.rs`).

use std::collections::HashMap;

use anyhow::Result;
use metafolder_core::metarecord::{Field, MetaRecord, Value};
use uuid::Uuid;

use crate::log::{OpRow, OpType, Retention};
use crate::rows::{FieldRow, TreeRow};

/// Implements `Rows` and `Log` for a type holding a store, by
/// handing every call to the store it holds.
macro_rules! forward_to_store {
    ($ty:ty, |$me:ident| $conn:expr) => {
        forward_to_store!(@rows_log $ty, |$me| $conn);
    };
    (@rows_log $ty:ty, |$me:ident| $conn:expr) => {
        impl Rows for $ty {
            fn as_kv(&self) -> Option<&crate::kvstore::KvStore> {
                let $me = self;
                Rows::as_kv($conn)
            }
            fn exists(&self, uuid: Uuid) -> Result<bool> {
                let $me = self;
                Rows::exists($conn, uuid)
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
            fn metarecord_count(&self) -> Result<usize> {
                let $me = self;
                Rows::metarecord_count($conn)
            }
            fn metarecords(&self) -> Result<Vec<Uuid>> {
                let $me = self;
                Rows::metarecords($conn)
            }
            fn for_each_row(&self, f: &mut dyn FnMut(Uuid, FieldRow) -> Result<()>) -> Result<()> {
                let $me = self;
                Rows::for_each_row($conn, f)
            }
            fn value_types(&self, name: &str) -> Result<Vec<String>> {
                let $me = self;
                Rows::value_types($conn, name)
            }
            fn holders(&self, name: &str) -> Result<Vec<Uuid>> {
                let $me = self;
                Rows::holders($conn, name)
            }
            fn holder_ids(&self, name: &str) -> Result<roaring::RoaringBitmap> {
                let $me = self;
                Rows::holder_ids($conn, name)
            }
            fn value_holder_ids(
                &self,
                name: &str,
                value: &Value,
            ) -> Result<roaring::RoaringBitmap> {
                let $me = self;
                Rows::value_holder_ids($conn, name, value)
            }
            fn uuids_of_ids(&self, ids: &roaring::RoaringBitmap) -> Result<Vec<Uuid>> {
                let $me = self;
                Rows::uuids_of_ids($conn, ids)
            }
            fn children(&self, field: &str, parent: Uuid) -> Result<Vec<(Uuid, String)>> {
                let $me = self;
                Rows::children($conn, field, parent)
            }
            fn children_named(
                &self,
                field: &str,
                parent: Uuid,
            ) -> Result<Vec<(Uuid, metafolder_core::metarecord::TreeName)>> {
                let $me = self;
                Rows::children_named($conn, field, parent)
            }
            fn children_page(
                &self,
                field: &str,
                parent: Uuid,
                after: Option<&[u8]>,
                descending: bool,
                limit: usize,
            ) -> Result<Vec<(Uuid, Vec<u8>)>> {
                let $me = self;
                Rows::children_page($conn, field, parent, after, descending, limit)
            }
            fn child_by_bytes(
                &self,
                field: &str,
                parent: Option<Uuid>,
                name: &[u8],
            ) -> Result<Option<Uuid>> {
                let $me = self;
                Rows::child_by_bytes($conn, field, parent, name)
            }
            fn child_by_text(
                &self,
                field: &str,
                parent: Option<Uuid>,
                name: &str,
                nocase: bool,
            ) -> Result<Option<Uuid>> {
                let $me = self;
                Rows::child_by_text($conn, field, parent, name, nocase)
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
            fn snapshots(&self, op_id: i64) -> Result<Vec<FieldRow>> {
                let $me = self;
                Log::snapshots($conn, op_id)
            }
            fn added(&self, op_id: i64) -> Result<Vec<FieldRow>> {
                let $me = self;
                Log::added($conn, op_id)
            }
            fn first_op(&self) -> Result<Option<i64>> {
                let $me = self;
                Log::first_op($conn)
            }
            fn ops_back(&self, from: i64, max: Option<usize>) -> Result<Vec<OpRow>> {
                let $me = self;
                Log::ops_back($conn, from, max)
            }
            fn all_ops(&self) -> Result<Vec<OpRow>> {
                let $me = self;
                Log::all_ops($conn)
            }
            fn labels(&self, ids: &[i64]) -> Result<HashMap<i64, LabelMeta>> {
                let $me = self;
                Log::labels($conn, ids)
            }
            fn counts(&self) -> Result<(i64, i64)> {
                let $me = self;
                Log::counts($conn)
            }
            fn label_ops(&self, label: i64) -> Result<Vec<OpRow>> {
                let $me = self;
                Log::label_ops($conn, label)
            }
            fn label_first_op(&self, label: i64) -> Result<Option<i64>> {
                let $me = self;
                Log::label_first_op($conn, label)
            }
            fn label_children(&self, label: i64) -> Result<Vec<i64>> {
                let $me = self;
                Log::label_children($conn, label)
            }
            fn label_op_count(&self, label: i64) -> Result<i64> {
                let $me = self;
                Log::label_op_count($conn, label)
            }
            fn label_op_ids(&self, label: i64) -> Result<Vec<i64>> {
                let $me = self;
                Log::label_op_ids($conn, label)
            }
            fn label_ids_before(&self, before: Option<i64>, max: usize) -> Result<Vec<i64>> {
                let $me = self;
                Log::label_ids_before($conn, before, max)
            }
            fn label_last_op(&self, label: i64) -> Result<Option<i64>> {
                let $me = self;
                Log::label_last_op($conn, label)
            }
            fn label_reverts(&self, label: i64) -> Result<Vec<(i64, bool)>> {
                let $me = self;
                Log::label_reverts($conn, label)
            }
            fn label_reverted_by(&self, label: i64) -> Result<Vec<(i64, bool)>> {
                let $me = self;
                Log::label_reverted_by($conn, label)
            }
            fn reverters(&self, op: i64) -> Result<Vec<i64>> {
                let $me = self;
                Log::reverters($conn, op)
            }
            fn label_of_command(&self, key: Uuid) -> Result<Option<i64>> {
                let $me = self;
                Log::label_of_command($conn, key)
            }
            fn watcher_label(&self) -> Result<Option<i64>> {
                let $me = self;
                Log::watcher_label($conn)
            }
            fn entity_ops_after(&self, entity: Uuid, after: i64) -> Result<Vec<OpRow>> {
                let $me = self;
                Log::entity_ops_after($conn, entity, after)
            }
            fn version_before_label(&self, label: i64, entity: Uuid) -> Result<Option<u64>> {
                let $me = self;
                Log::version_before_label($conn, label, entity)
            }
            fn ops_after(&self, op: i64) -> Result<Vec<OpRow>> {
                let $me = self;
                Log::ops_after($conn, op)
            }
            fn ops_after_count(&self, op: i64, cap: i64) -> Result<i64> {
                let $me = self;
                Log::ops_after_count($conn, op, cap)
            }
            fn last_op_at_or_before(&self, timestamp_ms: i64) -> Result<Option<i64>> {
                let $me = self;
                Log::last_op_at_or_before($conn, timestamp_ms)
            }
        }
    };
}

/// Metarecords and their field rows.
pub trait Rows {
    /// The key-value store behind this one, when it is one: a repository is
    /// queried from the store itself (doc "Storage").
    fn as_kv(&self) -> Option<&crate::kvstore::KvStore> {
        None
    }
    /// Whether a metarecord exists.
    fn exists(&self, uuid: Uuid) -> Result<bool>;
    /// A metarecord's version, `None` when it does not exist: a hash of its
    /// content, computed from its rows — nothing stores it (doc "Metarecord
    /// version").
    fn version(&self, uuid: Uuid) -> Result<Option<u64>> {
        if !self.exists(uuid)? {
            return Ok(None);
        }
        Ok(Some(crate::version::of_rows(uuid, &self.rows(uuid)?)))
    }
    /// A metarecord's field rows, in no particular order.
    fn rows(&self, uuid: Uuid) -> Result<Vec<FieldRow>>;
    /// A metarecord's rows of one field name, in no particular order.
    fn rows_named(&self, uuid: Uuid, name: &str) -> Result<Vec<FieldRow>>;
    /// Several metarecords' rows at once.
    fn rows_for(&self, uuids: &[Uuid]) -> Result<HashMap<Uuid, Vec<FieldRow>>>;
    /// Which of several metarecords exist.
    fn existing(&self, uuids: &[Uuid]) -> Result<std::collections::HashSet<Uuid>> {
        let mut out = std::collections::HashSet::new();
        for &u in uuids {
            if self.exists(u)? {
                out.insert(u);
            }
        }
        Ok(out)
    }
    /// How many metarecords there are.
    fn metarecord_count(&self) -> Result<usize>;
    /// Every metarecord.
    fn metarecords(&self) -> Result<Vec<Uuid>>;
    /// Every row with its metarecord, in row-id order — the load's one pass.
    fn for_each_row(&self, f: &mut dyn FnMut(Uuid, FieldRow) -> Result<()>) -> Result<()>;
    /// The value types a field name holds (`Nothing` aside), sorted — one,
    /// by the data model's invariant, save during a `retype`.
    fn value_types(&self, name: &str) -> Result<Vec<String>>;
    /// The metarecords holding at least one row of a field name.
    fn holders(&self, name: &str) -> Result<Vec<Uuid>>;
    /// The dense ids holding a row of a field name, a `Nothing` included —
    /// the field's counters and `absent` set, no record read.
    fn holder_ids(&self, name: &str) -> Result<roaring::RoaringBitmap>;
    /// The dense ids holding `value` in `name` — the value's posting, the
    /// writes to many records still pending replayed over it; for `Nothing`,
    /// the field's `absent` set (doc "Store tables"). No record read. A
    /// superset only for a text so long its key is hashed, on a collision.
    fn value_holder_ids(&self, name: &str, value: &Value) -> Result<roaring::RoaringBitmap>;
    /// The uuids of the live metarecords among dense ids, in id order.
    fn uuids_of_ids(&self, ids: &roaring::RoaringBitmap) -> Result<Vec<Uuid>>;
    /// The direct children of `parent` in `field`'s forest, `(uuid, name)`.
    fn children(&self, field: &str, parent: Uuid) -> Result<Vec<(Uuid, String)>>;
    /// [`Self::children`] with each name as its exact bytes — what a client is
    /// given to act on (doc "Tree names").
    fn children_named(
        &self,
        field: &str,
        parent: Uuid,
    ) -> Result<Vec<(Uuid, metafolder_core::metarecord::TreeName)>>;
    /// A page of `parent`'s children in `field`'s forest in the byte order of
    /// their names — reversed when `descending` — strictly after `after` in
    /// that order: `(uuid, name bytes)`. The default reads every child; a
    /// store keeping its forest ordered reads the page alone (a sorted walk
    /// of a folder then costs its page, doc "The forest in the store").
    fn children_page(
        &self,
        field: &str,
        parent: Uuid,
        after: Option<&[u8]>,
        descending: bool,
        limit: usize,
    ) -> Result<Vec<(Uuid, Vec<u8>)>> {
        let mut all: Vec<(Uuid, Vec<u8>)> =
            self.children(field, parent)?.into_iter().map(|(u, n)| (u, n.into_bytes())).collect();
        all.sort_by(|a, b| a.1.cmp(&b.1));
        if descending {
            all.reverse();
        }
        let later = |n: &[u8]| after.is_none_or(|a| if descending { n < a } else { n > a });
        Ok(all.into_iter().filter(|(_, n)| later(n)).take(limit).collect())
    }
    /// The child of `parent` (`None`: a root) whose name is exactly these
    /// bytes.
    fn child_by_bytes(
        &self,
        field: &str,
        parent: Option<Uuid>,
        name: &[u8],
    ) -> Result<Option<Uuid>>;
    /// The child of `parent` whose name, as text, is `name` — ignoring ASCII
    /// case when `nocase` (ASCII letters only — the case folding SQLite's
    /// `NOCASE` did, which repositories were written against).
    fn child_by_text(
        &self,
        field: &str,
        parent: Option<Uuid>,
        name: &str,
        nocase: bool,
    ) -> Result<Option<Uuid>>;
    /// Every `tree_ref` position, grouped by field name and metarecord, a
    /// metarecord's positions in row-id order (the groups themselves come in
    /// no promised order). A whole scan, for whole-repository questions only
    /// (`mf repo check`).
    fn forest(&self) -> Result<Vec<TreeRow>>;

    // ── Derived: every backend has these from the above ──────────────────

    /// A whole metarecord, `None` when it does not exist.
    fn metarecord(&self, uuid: Uuid) -> Result<Option<MetaRecord>> {
        if !self.exists(uuid)? {
            return Ok(None);
        }
        let rows = self.rows(uuid)?;
        let version = crate::version::of_rows(uuid, &rows);
        let fields = rows.into_iter().map(|r| Field { name: r.name, value: r.value }).collect();
        Ok(Some(MetaRecord { uuid, version, fields }))
    }
    /// A metarecord's positions in `field`'s forest, `(parent, name)` (`None`
    /// for a root), in row-id order.
    fn positions(&self, field: &str, uuid: Uuid) -> Result<Vec<(Option<Uuid>, String)>> {
        Ok(self
            .rows_named(uuid, field)?
            .into_iter()
            .filter_map(|r| r.value.position().map(|(p, name)| (p, name.display().into_owned())))
            .collect())
    }
    /// The metarecords holding a position in `field`'s forest (each once).
    fn placed(&self, field: &str) -> Result<Vec<Uuid>> {
        let mut seen = std::collections::HashSet::new();
        Ok(self
            .forest()?
            .into_iter()
            .filter(|t| t.field_name == field && seen.insert(t.uuid))
            .map(|t| t.uuid)
            .collect())
    }
    /// The first string value of a field, if any.
    fn string_field(&self, uuid: Uuid, name: &str) -> Result<Option<String>> {
        Ok(self.rows_named(uuid, name)?.into_iter().find_map(|r| match r.value {
            Value::String(s) => Some(s),
            _ => None,
        }))
    }
    /// Every `String` value of a multi-map field, in row order.
    fn string_fields(&self, uuid: Uuid, name: &str) -> Result<Vec<String>> {
        Ok(self
            .rows_named(uuid, name)?
            .into_iter()
            .filter_map(|r| match r.value {
                Value::String(s) => Some(s),
                _ => None,
            })
            .collect())
    }
    /// The first `Int` value of a field, if any.
    fn int_field(&self, uuid: Uuid, name: &str) -> Result<Option<i64>> {
        Ok(self.rows_named(uuid, name)?.into_iter().find_map(|r| match r.value {
            Value::Int(n) => Some(n),
            _ => None,
        }))
    }
    /// The first `Bool` value of a field, if any (`Nothing` does not count).
    fn bool_field(&self, uuid: Uuid, name: &str) -> Result<Option<bool>> {
        Ok(self.rows_named(uuid, name)?.into_iter().find_map(|r| match r.value {
            Value::Bool(b) => Some(b),
            _ => None,
        }))
    }
}

/// The event log (doc "The operation log"): a sequence of operations, in
/// the order they were written. There is no history tree — nothing moves back,
/// so "the log up to here" is simply every operation with a smaller id.
pub trait Log {
    /// The newest operation, `None` on an empty log.
    fn head(&self) -> Result<Option<i64>>;
    /// One operation.
    fn op(&self, id: i64) -> Result<Option<OpRow>>;
    /// The rows an operation took off its record: what it replaced, removed
    /// or deleted (its *before*; the log keeps no *after*).
    fn snapshots(&self, op_id: i64) -> Result<Vec<FieldRow>>;
    /// The rows an `append_field` operation added — what a revert takes off;
    /// none for any other operation.
    fn added(&self, op_id: i64) -> Result<Vec<FieldRow>>;
    /// The oldest operation still in the log, `None` on an empty log.
    fn first_op(&self) -> Result<Option<i64>>;
    /// The operations at or before `from`, newest first — all of them, or the
    /// first `max`.
    fn ops_back(&self, from: i64, max: Option<usize>) -> Result<Vec<OpRow>>;
    /// Every operation, oldest first.
    fn all_ops(&self) -> Result<Vec<OpRow>>;
    /// The given labels; ids that name none are left out.
    fn labels(&self, ids: &[i64]) -> Result<HashMap<i64, LabelMeta>>;
    /// How many operations and labels the log holds.
    fn counts(&self) -> Result<(i64, i64)>;
    /// A label's own operations — not its children's — oldest first.
    fn label_ops(&self, label: i64) -> Result<Vec<OpRow>>;
    /// A label's oldest operation of its own, `None` when it has none.
    fn label_first_op(&self, label: i64) -> Result<Option<i64>>;
    /// A label's children, oldest first.
    fn label_children(&self, label: i64) -> Result<Vec<i64>>;
    /// How many operations a label holds of its own, counted without reading
    /// them (a stub of a write to many records counts for all of its own).
    fn label_op_count(&self, label: i64) -> Result<i64>;
    /// The ids of a label's own operations, oldest first, without reading
    /// them (a stub's are the ids between its first and its last).
    fn label_op_ids(&self, label: i64) -> Result<Vec<i64>>;
    /// The ids of the labels older than `before` (all when none), newest
    /// first, at most `max`.
    fn label_ids_before(&self, before: Option<i64>, max: usize) -> Result<Vec<i64>>;
    /// A label's newest operation of its own, `None` when it has none.
    fn label_last_op(&self, label: i64) -> Result<Option<i64>>;
    /// The labels `label` reverts, each with whether the whole label (its
    /// children included) was reverted, or only some of its operations (doc
    /// "Label summaries").
    fn label_reverts(&self, label: i64) -> Result<Vec<(i64, bool)>>;
    /// The labels that revert `label`, as [`Self::label_reverts`] says them.
    fn label_reverted_by(&self, label: i64) -> Result<Vec<(i64, bool)>>;
    /// The operations that reverted `op` (their `reverts_op_id` is `op`),
    /// oldest first.
    fn reverters(&self, op: i64) -> Result<Vec<i64>>;
    /// The label the command `key` opened at its first write, if it did.
    fn label_of_command(&self, key: Uuid) -> Result<Option<i64>>;
    /// The label the watcher's flushes are opened under, once there is one
    /// (doc "Log contexts").
    fn watcher_label(&self) -> Result<Option<i64>>;
    /// An entity's operations newer than `after`, oldest first.
    fn entity_ops_after(&self, entity: Uuid, after: i64) -> Result<Vec<OpRow>>;
    /// The version `entity` had when label `label` began (a trash entry
    /// records the version from outside the watcher's flush, so a revert
    /// matches on this one); `None` when the label created it, or did not
    /// touch it. The log records no version: this is the entity's content
    /// now with every operation on it since the label's first undone, on the
    /// version itself — a sum of row terms (doc "Metarecord version").
    fn version_before_label(&self, label: i64, entity: Uuid) -> Result<Option<u64>>;
    /// The operations newer than `op`, oldest first (the change feed).
    fn ops_after(&self, op: i64) -> Result<Vec<OpRow>>;
    /// How many operations are newer than `op`, counted no further than
    /// `cap`: a client far behind asks whether the gap passes a limit, and
    /// counting the whole of it would read the log since.
    fn ops_after_count(&self, op: i64, cap: i64) -> Result<i64>;
    /// The newest operation written at or before `timestamp_ms`.
    fn last_op_at_or_before(&self, timestamp_ms: i64) -> Result<Option<i64>>;
}

/// A label (doc "Log contexts"): a name and a description for what
/// the operations under it — and under its children — belong to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelMeta {
    pub name: String,
    pub description: Option<String>,
    /// The label this one is nested in, `None` for a root.
    pub parent: Option<i64>,
    /// Unix milliseconds.
    pub created: i64,
}

/// A repository's database: its rows and its log.
pub trait Store: Rows + Log {}

impl<T: Rows + Log + ?Sized> Store for T {}

/// [`Log::version_before_label`], for any store: the entity's rows now, and
/// each of its operations since the label's first, newest first, undone on
/// them as a revert would — then the version of what that leaves (doc "Log
/// storage"). The log keeps no version. A metarecord with no rows — one
/// deleted then — has the version of no rows.
pub fn version_before_label<S: Rows + Log + ?Sized>(
    store: &S,
    label: i64,
    entity: Uuid,
) -> Result<Option<u64>> {
    let Some(first) = store.label_ops(label)?.into_iter().find(|o| o.entity_uuid == entity) else {
        return Ok(None);
    };
    if first.op_type == "create_metarecord" {
        return Ok(None);
    }
    let mut rows = store.rows(entity)?;
    for op in store.entity_ops_after(entity, first.id - 1)?.iter().rev() {
        let field = op.field_name.as_deref();
        match op.op_type.as_str() {
            "create_metarecord" => rows.clear(),
            "delete_metarecord" | "set_metarecord" => rows = store.snapshots(op.id)?,
            "append_field" => {
                for added in store.added(op.id)? {
                    if let Some(at) = rows.iter().position(|r| *r == added) {
                        rows.remove(at);
                    }
                }
            }
            "delete_field" => rows.extend(store.snapshots(op.id)?),
            // A set and the file operations: their field is what they took off.
            _ => {
                rows.retain(|r| Some(r.name.as_str()) != field);
                rows.extend(store.snapshots(op.id)?);
            }
        }
    }
    Ok(Some(crate::version::of_rows(entity, &rows)))
}

/// What a write to many records does to a field.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MassKind {
    /// Adds one value to the records that lack it.
    Add,
    /// Removes one value from the records that hold it.
    Remove,
    /// Replaces the field's rows by the given values — none, to remove it.
    Replace,
}

/// What [`WriteTxn::mass_apply`] did.
pub struct MassApplied {
    /// The records that changed (dense ids), one operation each.
    pub changed: roaring::RoaringBitmap,
    /// The last of their operations; they hold the ids before it.
    pub last_op: i64,
    /// Whether what changed decides which directories are watched: a deletion
    /// names no row, so the store says it.
    pub touches_watch: bool,
}

/// One operation a write transaction appends to the log.
pub struct NewOp {
    pub op_type: OpType,
    pub entity: Uuid,
    pub field_name: Option<String>,
    /// The rows it took off the record.
    pub before: Vec<FieldRow>,
    /// The rows it left: the value an append added is kept (a revert takes it
    /// off), the rest is only what the write's effects and its version
    /// delta are computed from.
    pub after: Vec<FieldRow>,
    /// The operation this one undoes, when a revert writes it.
    pub reverts_op_id: Option<i64>,
    /// Who wrote it (`log::ORIGIN_WATCHER`, `ORIGIN_TRASH`), `None` for a
    /// client's ordinary write — what retention sorts it by.
    pub origin: Option<String>,
}

/// A write transaction: everything it writes commits together or not at all,
/// and it reads its own writes. One `log::Writer` is one of these.
pub trait WriteTxn: Store {
    /// A new metarecord, with no row yet.
    fn create_metarecord(&self, uuid: Uuid) -> Result<()>;
    /// Gives back a deleted metarecord under the dense id it had, with the rows
    /// its record held when it was deleted, which are returned (doc "Deleting a
    /// metarecord in the store"). `None`, nothing written, when the store
    /// cannot: the uuid never had an id, or its metarecord exists.
    fn restore_metarecord(&self, _uuid: Uuid) -> Result<Option<Vec<FieldRow>>> {
        Ok(None)
    }
    /// Whether `uuid` is a deleted metarecord that [`Self::restore_metarecord`]
    /// can give back: it has a dense id, and nothing reserved it.
    fn was_deleted(&self, _uuid: Uuid) -> Result<bool> {
        Ok(false)
    }
    /// A write of `value` to, or off, `field` of many records, logged as one
    /// entry (doc "Mass operations in the log"): the records that change get
    /// an operation each, and nothing is read or written per record. `None`
    /// when the store does not do this one in bulk — nothing was written,
    /// and the caller writes the records one by one.
    #[allow(clippy::too_many_arguments)]
    fn mass_apply(
        &self,
        _kind: MassKind,
        _field: &str,
        _values: &[Value],
        _ids: &roaring::RoaringBitmap,
        _label: i64,
        _timestamp: i64,
        _origin: Option<&str>,
    ) -> Result<Option<MassApplied>> {
        Ok(None)
    }
    /// Deletes the records `ids` (those that exist) as one entry of the log,
    /// reading and writing none of them (doc "Mass operations in the log"):
    /// `None` — nothing written — when the store does not do it so, or when a
    /// record has children, which are unpicked one by one.
    fn mass_delete(
        &self,
        _ids: &roaring::RoaringBitmap,
        _label: i64,
        _timestamp: i64,
        _origin: Option<&str>,
    ) -> Result<Option<MassApplied>> {
        Ok(None)
    }
    /// Reverts the operations `first_op..=last_op` of one mass add or remove
    /// as one entry of the log whose operations each name the one they revert
    /// (doc "Mass operations in the log"); `None` — nothing written — when the
    /// store cannot, or the records are no longer as the operations left them.
    fn mass_revert(
        &self,
        _first_op: i64,
        _last_op: i64,
        _label: i64,
        _timestamp: i64,
        _origin: Option<&str>,
    ) -> Result<Option<MassApplied>> {
        Ok(None)
    }
    /// Gives back the records of the pending deletion whose operations are
    /// `first_op..=last_op`: their ids return to the universe and nothing else
    /// is written (doc "Mass operations in the log"). The ids given back;
    /// `None` — nothing written — when the store cannot do it so.
    fn mass_undelete(&self, _first_op: i64, _last_op: i64) -> Result<Option<MassApplied>> {
        Ok(None)
    }
    /// The ids holding more than `n` rows of `field` (a `Nothing` included),
    /// the transaction's own writes counted; `None` for a store that cannot
    /// say without reading the records.
    fn rows_over_ids(&self, _field: &str, _n: u64) -> Result<Option<roaring::RoaringBitmap>> {
        Ok(None)
    }

    /// The dense ids of `uuids`; `None` for a store with none. A uuid that is
    /// no metarecord is refused when `must_exist`, left out otherwise.
    fn dense_ids(
        &self,
        _uuids: &[Uuid],
        _must_exist: bool,
    ) -> Result<Option<roaring::RoaringBitmap>> {
        Ok(None)
    }
    /// The uuids of dense ids, in the order of the ids.
    fn uuids_of(&self, _ids: &roaring::RoaringBitmap) -> Result<Vec<Uuid>> {
        anyhow::bail!("this store has no dense ids")
    }
    /// Removes a metarecord and every row it holds.
    fn remove_metarecord(&self, uuid: Uuid) -> Result<()>;
    /// Inserts a row. Refuses a second row at one position of a forest, and a
    /// second `mfr_path`. A row is its name and value: nothing names it
    /// otherwise.
    fn insert_row(&self, uuid: Uuid, name: &str, value: &Value) -> Result<()>;
    /// Deletes the row of `uuid` holding `value` under `name`; nothing when
    /// it holds none.
    fn delete_row(&self, uuid: Uuid, name: &str, value: &Value) -> Result<()>;
    /// Deletes a metarecord's rows — all of them, or those of one name.
    fn delete_rows(&self, uuid: Uuid, name: Option<&str>) -> Result<()>;

    /// A new label; its id. Its parent, when it has one, is there.
    fn create_label(&self, meta: &LabelMeta) -> Result<i64>;
    /// A new label opened by the command `key`, which its later writes find
    /// with [`Log::label_of_command`].
    fn create_command_label(&self, meta: &LabelMeta, key: Uuid) -> Result<i64>;
    /// Removes a label that holds nothing (no operation, no child).
    fn drop_label(&self, id: i64) -> Result<()>;
    /// Notes that `reverter`'s operations revert `reverted`'s: the whole label
    /// when `tree`, some of its operations otherwise. Said again, a whole label
    /// stays whole.
    fn note_label_reverts(&self, _reverter: i64, _reverted: i64, _tree: bool) -> Result<()> {
        Ok(())
    }
    /// Records the label the watcher's flushes are opened under.
    fn set_watcher_label(&self, id: i64) -> Result<()>;
    /// Appends `ops` under `label`, written at `timestamp`; returns the id
    /// the last one got. Ids are never reused.
    fn append_ops(&self, label: i64, timestamp: i64, ops: &[NewOp]) -> Result<i64>;
    /// Records the newest operation (the change feed's cursor).
    fn set_head(&self, op: Option<i64>) -> Result<()>;
    /// Drops the history `retention` no longer keeps, never an operation
    /// numbered `keep_from` or later — the write committing, kept whole
    /// (doc "Automatic retention").
    fn trim(&self, retention: Retention, keep_from: i64) -> Result<usize>;
    /// Removes every metarecord.
    fn clear_metarecords(&self) -> Result<()>;
    /// Deletes operations and their snapshots, and the labels they leave
    /// empty — no operation, no child — but the watcher's; how many labels.
    fn delete_ops(&self, ids: &[i64]) -> Result<usize>;

    fn commit(self: Box<Self>) -> Result<()>;
}

/// Opens write transactions.
pub trait Begin {
    fn begin_write(&mut self) -> Result<Box<dyn WriteTxn + '_>>;
    /// What in the store no longer holds together, one line each — derived
    /// data differing from what its primary data derives, a damaged page;
    /// empty when healthy (`mf repo check`, doc "Checking and reindexing a repository").
    fn check(&self) -> Result<Vec<String>>;
    /// Derives again whatever the store derives from its primary data
    /// (`mf repo reindex`).
    fn reindex(&mut self) -> Result<()>;
    /// Writes a consistent copy of the store into the directory `dir`, as
    /// the store's own file layout (`kv/`) — taken while the
    /// store stays open (`mf repo backup`, doc "Backups and restore").
    fn backup_to(&self, dir: &std::path::Path) -> Result<()>;
    /// The key-value store behind this one, to change how it is held (doc
    /// "Holding a repository in memory").
    fn as_kv_mut(&mut self) -> Option<&mut crate::kvstore::KvStore> {
        None
    }
}

/// The metarecords holding more than one position in a forest, which the
/// daemon refuses to write since September 2026 but an older one let through
/// (doc "One position per forest") — one line each, for
/// `mf repo check`. Fixed by setting the field to the one position to keep.
/// A `hierarchy_ref` field is a hierarchy, where several are the point (doc
/// "Hierarchy references").
pub fn one_position_problems(store: &dyn Rows) -> Result<Vec<String>> {
    let mut count: std::collections::BTreeMap<(String, Uuid), usize> = Default::default();
    let mut forests: HashMap<String, bool> = HashMap::new();
    for row in store.forest()? {
        let forest = match forests.get(&row.field_name) {
            Some(forest) => *forest,
            None => {
                let types = store.value_types(&row.field_name)?;
                let forest = !types.iter().any(|t| t == "hierarchy_ref");
                forests.insert(row.field_name.clone(), forest);
                forest
            }
        };
        if forest {
            *count.entry((row.field_name, row.uuid)).or_default() += 1;
        }
    }
    Ok(count
        .into_iter()
        .filter(|(_, n)| *n > 1)
        .map(|((field, uuid), n)| {
            format!(
                "metarecord {uuid} holds {n} positions in the '{field}' forest (one per \
                 forest): set the field to the one to keep"
            )
        })
        .collect())
}

/// What a loaded repository holds: its database, whatever the backend.
pub type Handle = Box<dyn Database + Send>;

/// An open repository database: it answers reads and opens writes.
pub trait Database: Begin + Store {}

impl<T: Begin + Store + ?Sized> Database for T {}

forward_to_store!(Handle, |b| &**b);
forward_to_store!(std::sync::MutexGuard<'_, Handle>, |g| &***g);

impl Begin for Handle {
    fn begin_write(&mut self) -> Result<Box<dyn WriteTxn + '_>> {
        (**self).begin_write()
    }
    fn check(&self) -> Result<Vec<String>> {
        (**self).check()
    }
    fn reindex(&mut self) -> Result<()> {
        (**self).reindex()
    }
    fn backup_to(&self, dir: &std::path::Path) -> Result<()> {
        (**self).backup_to(dir)
    }
    fn as_kv_mut(&mut self) -> Option<&mut crate::kvstore::KvStore> {
        (**self).as_kv_mut()
    }
}

impl Begin for std::sync::MutexGuard<'_, Handle> {
    fn begin_write(&mut self) -> Result<Box<dyn WriteTxn + '_>> {
        (***self).begin_write()
    }
    fn check(&self) -> Result<Vec<String>> {
        (***self).check()
    }
    fn reindex(&mut self) -> Result<()> {
        (***self).reindex()
    }
    fn backup_to(&self, dir: &std::path::Path) -> Result<()> {
        (***self).backup_to(dir)
    }
    fn as_kv_mut(&mut self) -> Option<&mut crate::kvstore::KvStore> {
        (***self).as_kv_mut()
    }
}
