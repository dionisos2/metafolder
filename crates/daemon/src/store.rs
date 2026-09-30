//! The storage boundary (doc "The storage boundary"):
//! what the daemon asks of a repository's database, as the traits a storage
//! backend implements. `Rows` is the data model's primary state — metarecords
//! and their field rows, whose ids are part of the model (`Field.id`, restored
//! by navigation); `Log` is the event log. The questions a query answers are
//! not here: they belong to the index.
//!
//! The key-value store (`crate::kvstore`, LMDB) is the one backend; the traits
//! are what a second one would implement (`tests/store_contract.rs`).

use std::collections::HashMap;

use anyhow::Result;
use metafolder_core::metarecord::{Field, MetaRecord, TreeName, Value};
use uuid::Uuid;

use crate::log::{Delta, OpRow, OpType, Retention};
use crate::rows::{DuplicateGroup, FieldRow, OrphanCandidate, StoredHashes, TreeRow};

/// Implements `Rows`, `Log` and `Questions` for a type holding a store, by
/// handing every call to the store it holds.
macro_rules! forward_to_store {
    ($ty:ty, |$me:ident| $conn:expr) => {
        forward_to_store!(@rows_log $ty, |$me| $conn);
        forward_to_store!(@questions $ty, |$me| $conn);
    };
    (@rows_log $ty:ty, |$me:ident| $conn:expr) => {
        impl Rows for $ty {
            fn as_kv(&self) -> Option<&crate::kvstore::KvStore> {
                let $me = self;
                Rows::as_kv($conn)
            }
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
            fn counters(&self) -> Result<Counters> {
                let $me = self;
                Log::counters($conn)
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
            fn ops_after(&self, op: i64) -> Result<Vec<OpRow>> {
                let $me = self;
                Log::ops_after($conn, op)
            }
            fn ops_after_count(&self, op: i64) -> Result<i64> {
                let $me = self;
                Log::ops_after_count($conn, op)
            }
            fn ancestor_at_or_before(&self, head: i64, timestamp_ms: i64) -> Result<Option<i64>> {
                let $me = self;
                Log::ancestor_at_or_before($conn, head, timestamp_ms)
            }
            fn ancestor_labelled(&self, head: i64, label: &str) -> Result<Option<i64>> {
                let $me = self;
                Log::ancestor_labelled($conn, head, label)
            }
            fn before_revision_of(&self, op: i64) -> Result<Option<i64>> {
                let $me = self;
                Log::before_revision_of($conn, op)
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
    /// The key-value store behind this one, when it is one: a repository is
    /// queried from the store itself (doc "Storage").
    fn as_kv(&self) -> Option<&crate::kvstore::KvStore> {
        None
    }
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
    /// (`mf repo check`, the default answers of [`Questions`]).
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
    /// continues below it (doc "Log endpoints"), oldest first.
    fn active_line(&self, head: i64) -> Result<Vec<OpRow>>;
    /// Whether an operation has a child (a continuation below it).
    fn has_children(&self, op: i64) -> Result<bool>;
    /// The given revisions' metadata; ids that name none are left out.
    fn revisions(&self, ids: &[i64]) -> Result<HashMap<i64, RevisionMeta>>;
    /// How many operations and revisions the log holds.
    fn counts(&self) -> Result<(i64, i64)>;
    /// The next id each counter hands out.
    fn counters(&self) -> Result<Counters>;
    /// A revision's operations, oldest first.
    fn revision_ops(&self, rev: i64) -> Result<Vec<OpRow>>;
    /// An entity's operations newer than `after`, whatever branch they are on,
    /// oldest first.
    fn entity_ops_after(&self, entity: Uuid, after: i64) -> Result<Vec<OpRow>>;
    /// The version `entity` had when revision `rev` began — its first
    /// operation there's `entity_version_before` (a trash entry records the
    /// version from outside the revision, so a rollback matches on this one).
    fn version_before_revision(&self, rev: i64, entity: Uuid) -> Result<Option<u64>>;
    /// The operations newer than `op`, oldest first (the change feed).
    fn ops_after(&self, op: i64) -> Result<Vec<OpRow>>;
    /// How many operations are newer than `op`.
    fn ops_after_count(&self, op: i64) -> Result<i64>;
    /// Walking back from `head`, the first operation whose revision is at or
    /// before `timestamp_ms`.
    fn ancestor_at_or_before(&self, head: i64, timestamp_ms: i64) -> Result<Option<i64>>;
    /// Walking back from `head`, the first operation of a revision labelled
    /// `label` (the last operation of the most recent such revision).
    fn ancestor_labelled(&self, head: i64, label: &str) -> Result<Option<i64>>;
    /// The parent of the first operation of `op`'s revision: the state just
    /// before that whole revision (`None` = the empty state).
    fn before_revision_of(&self, op: i64) -> Result<Option<i64>>;
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
/// native answer (the key-value store overrides none today).
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
    use crate::rows::{self, DuplicateGroup, FieldRow, OrphanCandidate, StoredHashes};

    fn type_of(v: &Value) -> &'static str {
        rows::encode_value(v).value_type
    }
    fn int_of(v: &Value) -> Option<i64> {
        rows::encode_value(v).int
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
    /// Makes `op` a root of the history (its parent is pruned away).
    fn detach_op(&self, op: i64) -> Result<()>;
    /// Deletes operations and their snapshots, children before parents.
    fn delete_ops(&self, ids: &[i64]) -> Result<()>;
    /// Deletes the revisions no operation belongs to any more.
    fn drop_empty_revisions(&self) -> Result<()>;
    fn queue_restoration(&self, restoration: &Restoration) -> Result<()>;
    /// Drops the queued restorations up to position `up_to`, included.
    fn drop_restorations(&self, up_to: i64) -> Result<()>;

    /// Puts a revision under its own id — a conversion copying a history
    /// (doc "How the storage backend was built").
    fn import_revision(&self, id: i64, meta: &RevisionMeta) -> Result<()>;
    /// Puts an operation and its snapshots under their own ids (a
    /// conversion); its parent and its revision are already there.
    fn import_op(&self, op: &OpRow, before: &[FieldRow], after: &[FieldRow]) -> Result<()>;
    /// Moves the counters to at least `counters`, so no id below them is
    /// handed out again.
    fn raise_counters(&self, counters: Counters) -> Result<()>;

    fn commit(self: Box<Self>) -> Result<()>;
}

/// The next id each of a store's counters hands out: rows, operations,
/// revisions. Ids are never reused, so a copy carries these along.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counters {
    pub next_row: i64,
    pub next_op: i64,
    pub next_rev: i64,
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
}

/// The metarecords holding more than one position in a forest, which the
/// daemon refuses to write since September 2026 but an older one let through
/// (doc "One position per forest") — one line each, for
/// `mf repo check`. Fixed by setting the field to the one position to keep.
pub fn one_position_problems(store: &dyn Rows) -> Result<Vec<String>> {
    let mut count: std::collections::BTreeMap<(String, Uuid), usize> = Default::default();
    for row in store.forest()? {
        *count.entry((row.field_name, row.uuid)).or_default() += 1;
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
}
