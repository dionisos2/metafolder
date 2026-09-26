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
use metafolder_core::metarecord::{Field, MetaRecord, TreeName, Value};
use rusqlite::{params, Connection, Transaction};
use uuid::Uuid;

use crate::db::{self, DuplicateGroup, FieldRow, OrphanCandidate, StoredHashes, TreeRow};
use crate::log::{self, Delta, OpRow, OpType, Retention};

/// Implements `Rows` and `Log` for a type holding a SQLite connection, by
/// handing every call to the `Connection` implementation.
macro_rules! forward_to_connection {
    ($ty:ty, |$me:ident| $conn:expr) => {
        forward_to_connection!(@rows_log $ty, |$me| $conn);
        forward_to_connection!(@questions $ty, |$me| $conn);
    };
    (@rows_log $ty:ty, |$me:ident| $conn:expr) => {
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
            fn versions_for(&self, uuids: &[Uuid]) -> Result<HashMap<Uuid, u64>> {
                let $me = self;
                Rows::versions_for($conn, uuids)
            }
            fn metarecord_count(&self) -> Result<usize> {
                let $me = self;
                Rows::metarecord_count($conn)
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
            fn field_rows(&self, name: &str) -> Result<Vec<(Uuid, FieldRow)>> {
                let $me = self;
                Rows::field_rows($conn, name)
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
            fn version_before_revision(&self, rev: i64, entity: Uuid) -> Result<Option<u64>> {
                let $me = self;
                Log::version_before_revision($conn, rev, entity)
            }
        }
    };
    (@questions $ty:ty, |$me:ident| $conn:expr) => {
        impl Questions for $ty {
            fn holding(&self, name: &str, value: &Value) -> Result<Vec<Uuid>> {
                let $me = self;
                Questions::holding($conn, name, value)
            }
            fn string_owners(&self, name: &str) -> Result<Vec<(Uuid, String)>> {
                let $me = self;
                Questions::string_owners($conn, name)
            }
            fn ref_map(&self, name: &str) -> Result<HashMap<Uuid, Uuid>> {
                let $me = self;
                Questions::ref_map($conn, name)
            }
            fn hash_cache(&self) -> Result<HashMap<Uuid, StoredHashes>> {
                let $me = self;
                Questions::hash_cache($conn)
            }
            fn tracked_files_with_size(&self) -> Result<Vec<(Uuid, i64)>> {
                let $me = self;
                Questions::tracked_files_with_size($conn)
            }
            fn duplicate_groups(&self) -> Result<HashMap<(i64, String), DuplicateGroup>> {
                let $me = self;
                Questions::duplicate_groups($conn)
            }
            fn duplicate_group_members(&self, group: Uuid) -> Result<Vec<Uuid>> {
                let $me = self;
                Questions::duplicate_group_members($conn, group)
            }
            fn hashed_orphans(&self) -> Result<Vec<OrphanCandidate>> {
                let $me = self;
                Questions::hashed_orphans($conn)
            }
            fn wrong_type(&self, field: &str, allowed: &str, limit: i64) -> Result<Vec<Uuid>> {
                let $me = self;
                Questions::wrong_type($conn, field, allowed, limit)
            }
            fn count_over(&self, field: &str, max: i64, limit: i64) -> Result<Vec<Uuid>> {
                let $me = self;
                Questions::count_over($conn, field, max, limit)
            }
            fn count_under(&self, field: &str, min: i64, limit: i64) -> Result<Vec<Uuid>> {
                let $me = self;
                Questions::count_under($conn, field, min, limit)
            }
            fn missing(&self, field: &str, limit: i64) -> Result<Vec<Uuid>> {
                let $me = self;
                Questions::missing($conn, field, limit)
            }
            fn typed_missing(
                &self,
                types: &[String],
                field: &str,
                limit: i64,
            ) -> Result<Vec<Uuid>> {
                let $me = self;
                Questions::typed_missing($conn, types, field, limit)
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
    /// Several metarecords' versions at once (the missing ones left out).
    fn versions_for(&self, uuids: &[Uuid]) -> Result<HashMap<Uuid, u64>>;
    /// How many metarecords there are.
    fn metarecord_count(&self) -> Result<usize>;
    /// A row by its id.
    fn row(&self, id: i64) -> Result<Option<FieldRow>>;
    /// The metarecord a row belongs to.
    fn owner_of_row(&self, id: i64) -> Result<Option<Uuid>>;
    /// Every metarecord.
    fn metarecords(&self) -> Result<Vec<Uuid>>;
    /// Every row of one field name, with its metarecord, in row-id order.
    fn field_rows(&self, name: &str) -> Result<Vec<(Uuid, FieldRow)>>;
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
    /// The child of `parent` (`None`: a root) whose name is exactly these
    /// bytes.
    fn child_by_bytes(
        &self,
        field: &str,
        parent: Option<Uuid>,
        name: &[u8],
    ) -> Result<Option<Uuid>>;
    /// The child of `parent` whose name, as text, is `name` — ignoring ASCII
    /// case when `nocase` (SQLite's `NOCASE`, which another backend must
    /// reproduce exactly: ASCII letters only).
    fn child_by_text(
        &self,
        field: &str,
        parent: Option<Uuid>,
        name: &str,
        nocase: bool,
    ) -> Result<Option<Uuid>>;
    /// Every `tree_ref` position, grouped by field name and metarecord, a
    /// metarecord's positions in row-id order (the order a load places them
    /// in; the groups themselves come in no promised order).
    fn forest(&self) -> Result<Vec<TreeRow>>;

    // ── Derived: every backend has these from the above ──────────────────

    /// A whole metarecord, `None` when it does not exist.
    fn metarecord(&self, uuid: Uuid) -> Result<Option<MetaRecord>> {
        let Some(version) = self.version(uuid)? else { return Ok(None) };
        let fields = self
            .rows(uuid)?
            .into_iter()
            .map(|r| Field { id: Some(r.id), name: r.name, value: r.value })
            .collect();
        Ok(Some(MetaRecord { uuid, version, fields }))
    }
    /// A metarecord's positions in `field`'s forest, `(parent, name)` (`None`
    /// for a root), in row-id order.
    fn positions(&self, field: &str, uuid: Uuid) -> Result<Vec<(Option<Uuid>, String)>> {
        Ok(self
            .rows_named(uuid, field)?
            .into_iter()
            .filter_map(|r| match r.value {
                Value::TreeRef { parent, name } => Some((parent, name.display().into_owned())),
                _ => None,
            })
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
    /// The version `entity` had when revision `rev` began — its first
    /// operation there's `entity_version_before` (a trash entry records the
    /// version from outside the revision, so a rollback matches on this one).
    fn version_before_revision(&self, rev: i64, entity: Uuid) -> Result<Option<u64>>;
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

/// A repository's database: its rows, its log, and the questions asked of
/// them.
pub trait Store: Rows + Log + Questions {}

impl<T: Rows + Log + Questions + ?Sized> Store for T {}

/// Whole-repository questions (the schema check, the duplicate and orphan
/// scans, the watch flags). Each has a default derived from `Rows` alone —
/// correct on any backend — which a backend may override with a faster
/// native answer, as SQLite does with its indexed queries.
/// `tests/store_contract.rs` holds every override to its default.
pub trait Questions: Rows {
    /// The metarecords holding a row of `name` equal to `value` (`Nothing`
    /// included), each once.
    fn holding(&self, name: &str, value: &Value) -> Result<Vec<Uuid>> {
        derive::holding(self, name, value)
    }
    /// Each metarecord's first string value of `name`, in row order.
    fn string_owners(&self, name: &str) -> Result<Vec<(Uuid, String)>> {
        derive::string_owners(self, name)
    }
    /// Every `Ref` row of `name`, as owner → target.
    fn ref_map(&self, name: &str) -> Result<HashMap<Uuid, Uuid>> {
        derive::ref_map(self, name)
    }
    /// The stored content hashes and their stamps, per metarecord.
    fn hash_cache(&self) -> Result<HashMap<Uuid, StoredHashes>> {
        derive::hash_cache(self)
    }
    /// Every tracked file (`mfr_type = "file"`, placed) with its `mfr_size`.
    fn tracked_files_with_size(&self) -> Result<Vec<(Uuid, i64)>> {
        derive::tracked_files_with_size(self)
    }
    /// The `duplicate_group` metarecords, by `(content size, content hash)`.
    fn duplicate_groups(&self) -> Result<HashMap<(i64, String), DuplicateGroup>> {
        derive::duplicate_groups(self)
    }
    /// The metarecords whose `mfr_duplicate_group` refers to `group`.
    fn duplicate_group_members(&self, group: Uuid) -> Result<Vec<Uuid>> {
        derive::duplicate_group_members(self, group)
    }
    /// The orphans (`mfr_path` = `Nothing`) that carry a size and both hashes.
    fn hashed_orphans(&self) -> Result<Vec<OrphanCandidate>> {
        derive::hashed_orphans(self)
    }
    /// Up to `limit` metarecords holding a non-`Nothing` `field` row of
    /// another type than `allowed`.
    fn wrong_type(&self, field: &str, allowed: &str, limit: i64) -> Result<Vec<Uuid>> {
        derive::wrong_type(self, field, allowed, limit)
    }
    /// Up to `limit` metarecords with more than `max` rows of `field`.
    fn count_over(&self, field: &str, max: i64, limit: i64) -> Result<Vec<Uuid>> {
        derive::count_over(self, field, max, limit)
    }
    /// Up to `limit` metarecords holding `field`, with fewer than `min` rows.
    fn count_under(&self, field: &str, min: i64, limit: i64) -> Result<Vec<Uuid>> {
        derive::count_under(self, field, min, limit)
    }
    /// Up to `limit` metarecords with no `field` row at all.
    fn missing(&self, field: &str, limit: i64) -> Result<Vec<Uuid>> {
        derive::missing(self, field, limit)
    }
    /// Up to `limit` metarecords declared one of `types` (`mf_schema`) with no
    /// `field` row.
    fn typed_missing(&self, types: &[String], field: &str, limit: i64) -> Result<Vec<Uuid>> {
        derive::typed_missing(self, types, field, limit)
    }
}

/// The derived answers of [`Questions`], from `Rows` alone.
pub mod derive {
    use std::collections::{HashMap, HashSet};

    use anyhow::Result;
    use metafolder_core::metarecord::Value;
    use uuid::Uuid;

    use super::Rows;
    use crate::db::{self, DuplicateGroup, FieldRow, OrphanCandidate, StoredHashes};

    fn type_of(v: &Value) -> &'static str {
        db::encode_value(v).value_type
    }
    fn int_of(v: &Value) -> Option<i64> {
        db::encode_value(v).int
    }
    fn text_of(v: &Value) -> Option<String> {
        match v {
            Value::String(s) => Some(s.clone()),
            _ => None,
        }
    }
    /// A field's rows grouped by metarecord (first-seen order kept apart).
    fn by_owner(rows: Vec<(Uuid, FieldRow)>) -> HashMap<Uuid, Vec<Value>> {
        let mut out: HashMap<Uuid, Vec<Value>> = HashMap::new();
        for (u, r) in rows {
            out.entry(u).or_default().push(r.value);
        }
        out
    }
    fn ints(rows: &HashMap<Uuid, Vec<Value>>, u: Uuid, ty: &str) -> Vec<i64> {
        rows.get(&u)
            .map(|vs| vs.iter().filter(|v| type_of(v) == ty).filter_map(int_of).collect())
            .unwrap_or_default()
    }
    fn strings(rows: &HashMap<Uuid, Vec<Value>>, u: Uuid) -> Vec<String> {
        rows.get(&u).map(|vs| vs.iter().filter_map(text_of).collect()).unwrap_or_default()
    }
    fn take(uuids: impl Iterator<Item = Uuid>, limit: i64) -> Vec<Uuid> {
        let mut seen = HashSet::new();
        uuids.filter(|u| seen.insert(*u)).take(limit.max(0) as usize).collect()
    }

    pub fn holding<S: Rows + ?Sized>(s: &S, name: &str, value: &Value) -> Result<Vec<Uuid>> {
        let rows = s.field_rows(name)?;
        Ok(take(rows.into_iter().filter(|(_, r)| &r.value == value).map(|(u, _)| u), i64::MAX))
    }
    pub fn string_owners<S: Rows + ?Sized>(s: &S, name: &str) -> Result<Vec<(Uuid, String)>> {
        let mut seen = HashSet::new();
        Ok(s.field_rows(name)?
            .into_iter()
            .filter_map(|(u, r)| text_of(&r.value).map(|t| (u, t)))
            .filter(|(u, _)| seen.insert(*u))
            .collect())
    }
    pub fn ref_map<S: Rows + ?Sized>(s: &S, name: &str) -> Result<HashMap<Uuid, Uuid>> {
        Ok(s.field_rows(name)?
            .into_iter()
            .filter_map(|(u, r)| match r.value {
                Value::Ref(t) => Some((u, t)),
                _ => None,
            })
            .collect())
    }
    pub fn hash_cache<S: Rows + ?Sized>(s: &S) -> Result<HashMap<Uuid, StoredHashes>> {
        let mut out: HashMap<Uuid, StoredHashes> = HashMap::new();
        let mut mtimes: HashMap<Uuid, i64> = HashMap::new();
        let mut sizes: HashMap<Uuid, i64> = HashMap::new();
        for name in ["mfr_partial_hash", "mfr_full_hash", "mfr_hash_mtime", "mfr_hash_size"] {
            for (u, r) in s.field_rows(name)? {
                let entry = out.entry(u).or_default();
                match (name, text_of(&r.value), int_of(&r.value)) {
                    ("mfr_partial_hash", Some(t), _) => entry.partial = Some(t),
                    ("mfr_full_hash", Some(t), _) => entry.full = Some(t),
                    ("mfr_hash_mtime", _, Some(n)) => {
                        mtimes.insert(u, n);
                    }
                    ("mfr_hash_size", _, Some(n)) => {
                        sizes.insert(u, n);
                    }
                    _ => {}
                }
            }
        }
        for (u, entry) in out.iter_mut() {
            if let (Some(m), Some(z)) = (mtimes.get(u), sizes.get(u)) {
                entry.stamp = Some((*m, *z));
            }
        }
        Ok(out)
    }
    pub fn tracked_files_with_size<S: Rows + ?Sized>(s: &S) -> Result<Vec<(Uuid, i64)>> {
        let sizes = by_owner(s.field_rows("mfr_size")?);
        let paths = by_owner(s.field_rows("mfr_path")?);
        let mut out = Vec::new();
        for (u, r) in s.field_rows("mfr_type")? {
            if r.value != Value::String("file".into()) {
                continue;
            }
            let placed = paths
                .get(&u)
                .map_or(0, |vs| vs.iter().filter(|v| matches!(v, Value::TreeRef { .. })).count());
            for size in ints(&sizes, u, "int") {
                for _ in 0..placed {
                    out.push((u, size));
                }
            }
        }
        Ok(out)
    }
    pub fn duplicate_groups<S: Rows + ?Sized>(
        s: &S,
    ) -> Result<HashMap<(i64, String), DuplicateGroup>> {
        let sizes = by_owner(s.field_rows("mfr_content_size")?);
        let counts = by_owner(s.field_rows("mfr_duplicate_count")?);
        let reclaim = by_owner(s.field_rows("mfr_duplicate_reclaimable")?);
        let mut out = HashMap::new();
        for (u, r) in s.field_rows("mfr_content_hash")? {
            let Some(hash) = text_of(&r.value) else { continue };
            let or_none = |v: Vec<i64>| -> Vec<Option<i64>> {
                if v.is_empty() {
                    vec![None]
                } else {
                    v.into_iter().map(Some).collect()
                }
            };
            for size in ints(&sizes, u, "int") {
                for count in or_none(ints(&counts, u, "int")) {
                    for reclaimable in or_none(ints(&reclaim, u, "int")) {
                        out.insert(
                            (size, hash.clone()),
                            DuplicateGroup { uuid: u, count, reclaimable },
                        );
                    }
                }
            }
        }
        Ok(out)
    }
    pub fn duplicate_group_members<S: Rows + ?Sized>(s: &S, group: Uuid) -> Result<Vec<Uuid>> {
        Ok(s.field_rows("mfr_duplicate_group")?
            .into_iter()
            .filter(|(_, r)| r.value == Value::Ref(group))
            .map(|(u, _)| u)
            .collect())
    }
    pub fn hashed_orphans<S: Rows + ?Sized>(s: &S) -> Result<Vec<OrphanCandidate>> {
        let sizes = by_owner(s.field_rows("mfr_size")?);
        let partial = by_owner(s.field_rows("mfr_partial_hash")?);
        let full = by_owner(s.field_rows("mfr_full_hash")?);
        let mut out = Vec::new();
        for (u, r) in s.field_rows("mfr_path")? {
            if r.value != Value::Nothing {
                continue;
            }
            for size in ints(&sizes, u, "int") {
                for p in strings(&partial, u) {
                    for f in strings(&full, u) {
                        out.push(OrphanCandidate {
                            uuid: u,
                            size,
                            partial_hash: p.clone(),
                            full_hash: f,
                        });
                    }
                }
            }
        }
        Ok(out)
    }
    pub fn wrong_type<S: Rows + ?Sized>(
        s: &S,
        field: &str,
        allowed: &str,
        limit: i64,
    ) -> Result<Vec<Uuid>> {
        let rows = s.field_rows(field)?;
        Ok(take(
            rows.into_iter()
                .filter(|(_, r)| {
                    let t = type_of(&r.value);
                    t != "nothing" && t != allowed
                })
                .map(|(u, _)| u),
            limit,
        ))
    }
    fn counts<S: Rows + ?Sized>(s: &S, field: &str) -> Result<Vec<(Uuid, i64)>> {
        let mut order = Vec::new();
        let mut n: HashMap<Uuid, i64> = HashMap::new();
        for (u, _) in s.field_rows(field)? {
            let c = n.entry(u).or_insert(0);
            if *c == 0 {
                order.push(u);
            }
            *c += 1;
        }
        Ok(order.into_iter().map(|u| (u, n[&u])).collect())
    }
    pub fn count_over<S: Rows + ?Sized>(
        s: &S,
        field: &str,
        max: i64,
        limit: i64,
    ) -> Result<Vec<Uuid>> {
        Ok(take(counts(s, field)?.into_iter().filter(|(_, c)| *c > max).map(|(u, _)| u), limit))
    }
    pub fn count_under<S: Rows + ?Sized>(
        s: &S,
        field: &str,
        min: i64,
        limit: i64,
    ) -> Result<Vec<Uuid>> {
        Ok(take(counts(s, field)?.into_iter().filter(|(_, c)| *c < min).map(|(u, _)| u), limit))
    }
    pub fn missing<S: Rows + ?Sized>(s: &S, field: &str, limit: i64) -> Result<Vec<Uuid>> {
        let holders: HashSet<Uuid> = s.field_rows(field)?.into_iter().map(|(u, _)| u).collect();
        Ok(take(s.metarecords()?.into_iter().filter(|u| !holders.contains(u)), limit))
    }
    pub fn typed_missing<S: Rows + ?Sized>(
        s: &S,
        types: &[String],
        field: &str,
        limit: i64,
    ) -> Result<Vec<Uuid>> {
        if types.is_empty() {
            return Ok(Vec::new());
        }
        let holders: HashSet<Uuid> = s.field_rows(field)?.into_iter().map(|(u, _)| u).collect();
        let declared = s
            .field_rows("mf_schema")?
            .into_iter()
            .filter(|(_, r)| matches!(&r.value, Value::String(t) if types.contains(t)));
        Ok(take(declared.map(|(u, _)| u).filter(|u| !holders.contains(u)), limit))
    }
}

/// The SQLite answers: the indexed queries of `db.rs` where there is one.
impl Questions for Connection {
    fn holding(&self, name: &str, value: &Value) -> Result<Vec<Uuid>> {
        match value {
            Value::Bool(b) => db::metarecords_with_bool(self, name, *b),
            Value::Nothing => db::metarecords_with_absent_field(self, name),
            other => derive::holding(self, name, other),
        }
    }
    fn string_owners(&self, name: &str) -> Result<Vec<(Uuid, String)>> {
        db::string_field_owners(self, name)
    }
    fn ref_map(&self, name: &str) -> Result<HashMap<Uuid, Uuid>> {
        db::ref_field_map(self, name)
    }
    fn hash_cache(&self) -> Result<HashMap<Uuid, StoredHashes>> {
        db::hash_cache(self)
    }
    fn tracked_files_with_size(&self) -> Result<Vec<(Uuid, i64)>> {
        db::tracked_files_with_size(self)
    }
    fn duplicate_groups(&self) -> Result<HashMap<(i64, String), DuplicateGroup>> {
        db::duplicate_groups(self)
    }
    fn duplicate_group_members(&self, group: Uuid) -> Result<Vec<Uuid>> {
        db::duplicate_group_members(self, group)
    }
    fn hashed_orphans(&self) -> Result<Vec<OrphanCandidate>> {
        db::hashed_orphans(self)
    }
    fn wrong_type(&self, field: &str, allowed: &str, limit: i64) -> Result<Vec<Uuid>> {
        db::uuids_field_wrong_type(self, field, allowed, limit)
    }
    fn count_over(&self, field: &str, max: i64, limit: i64) -> Result<Vec<Uuid>> {
        db::uuids_field_count_over(self, field, max, limit)
    }
    fn count_under(&self, field: &str, min: i64, limit: i64) -> Result<Vec<Uuid>> {
        db::uuids_field_count_under(self, field, min, limit)
    }
    fn missing(&self, field: &str, limit: i64) -> Result<Vec<Uuid>> {
        db::uuids_missing_field(self, field, limit)
    }
    fn typed_missing(&self, types: &[String], field: &str, limit: i64) -> Result<Vec<Uuid>> {
        db::uuids_typed_missing_field(self, types, field, limit)
    }
}

/// A SQLite store answering every [`Questions`] with its derived default —
/// what the contract test holds the SQLite overrides to.
pub struct Derived<'a>(pub &'a Connection);

forward_to_connection!(@rows_log Derived<'_>, |d| d.0);

impl Questions for Derived<'_> {}

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

/// An open repository database: it answers reads and opens writes.
pub trait Database: Begin + Store {}

impl<T: Begin + Store + ?Sized> Database for T {}

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
    fn versions_for(&self, uuids: &[Uuid]) -> Result<HashMap<Uuid, u64>> {
        db::versions_for(self, uuids)
    }
    fn metarecord_count(&self) -> Result<usize> {
        db::count_metarecords(self)
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
    fn field_rows(&self, name: &str) -> Result<Vec<(Uuid, FieldRow)>> {
        db::rows_of_field(self, name)
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
    fn child_by_bytes(
        &self,
        field: &str,
        parent: Option<Uuid>,
        name: &[u8],
    ) -> Result<Option<Uuid>> {
        db::find_tree_child_by_bytes(self, field, parent, name)
    }
    fn child_by_text(
        &self,
        field: &str,
        parent: Option<Uuid>,
        name: &str,
        nocase: bool,
    ) -> Result<Option<Uuid>> {
        db::find_tree_child_opts(self, field, parent, name, nocase)
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
    fn version_before_revision(&self, rev: i64, entity: Uuid) -> Result<Option<u64>> {
        log::entity_version_before_revision(self, rev, entity)
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
