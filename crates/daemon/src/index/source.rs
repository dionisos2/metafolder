//! What the query evaluator asks of the data it evaluates against
//! (docs/spec-storage.org, "Increment 4, concretely"): one evaluator —
//! [`super::Eval`], which holds the query semantics the oracle validates —
//! and as many sources as there are ways to hold the bitmaps. The resident
//! [`super::RepoIndex`] is one; the key-value store's derived key spaces are
//! the other.
//!
//! Every answer is a set of dense ids. A field the source knows nothing about
//! answers empty everywhere, as a field without rows does in the oracle.

use std::borrow::Cow;

use metafolder_core::metarecord::Value;
use roaring::RoaringBitmap;
use uuid::Uuid;

use super::field_index::{CmpOp, FieldIndex, SortRep};
use super::Unsupported;

/// Which traversals a field's values support.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Follow {
    /// Neither: its values point nowhere (or nowhere the log follows).
    None,
    /// `Follows`: a `ref` field, whose values name a referent.
    Direct,
    /// `Follows` and `FollowsTransitive`: a `tree_ref` field, a forest.
    Tree,
}

/// A field's sort representative per dense id, for one direction: the min of
/// its values ascending, the max descending; `None` without a value.
pub type RepReader<'a> = Box<dyn Fn(u32) -> Option<SortRep> + 'a>;

/// The data a query is evaluated against.
pub trait Source {
    /// Every metarecord's id.
    fn universe(&self) -> Cow<'_, RoaringBitmap>;
    /// The ids with at least one non-`Nothing` row of `field`.
    fn present(&self, field: &str) -> Cow<'_, RoaringBitmap>;
    /// The ids with at least one `Nothing` row of `field`.
    fn absent(&self, field: &str) -> Cow<'_, RoaringBitmap>;
    /// The value type `field` holds (`"string"`, `"tree_ref"`, …).
    fn value_type(&self, field: &str) -> Option<&str>;

    /// The id of a metarecord.
    fn id(&self, uuid: Uuid) -> Option<u32>;
    /// The metarecord of an id.
    fn uuid(&self, id: u32) -> Option<Uuid>;
    /// How many ids there are in uuid order (the span of a uuid-order walk).
    fn id_count(&self) -> u64;
    /// Every `(uuid, id)` in uuid order, starting strictly after `after`.
    fn in_uuid_order(&self, after: Option<Uuid>) -> Box<dyn Iterator<Item = (Uuid, u32)> + '_>;

    /// The ids with a row of `field` satisfying `op value` (multi-map: any
    /// row). The row semantics are the oracle's `scalar_predicate`.
    fn compare(&self, field: &str, op: CmpOp, value: &Value) -> Result<RoaringBitmap, Unsupported>;
    /// The ids sharing a value of `field` with one of `seed`'s.
    fn same_as(&self, field: &str, seed: &RoaringBitmap) -> RoaringBitmap;
    /// The ids whose text (a string value, a `tree_ref` name) satisfies
    /// `keep`, within `restrict` when given. `literals` are substrings every
    /// text `keep` accepts contains, ignoring case — what a source with a text
    /// index narrows the search with (it may ignore them).
    fn scan_text(
        &self,
        field: &str,
        keep: &dyn Fn(&str) -> bool,
        literals: &[String],
        restrict: Option<&RoaringBitmap>,
    ) -> RoaringBitmap;
    /// The ids whose `tree_ref` name satisfies `keep`, within `restrict`.
    fn scan_names(
        &self,
        field: &str,
        keep: &dyn Fn(&str) -> bool,
        literals: &[String],
        restrict: Option<&RoaringBitmap>,
    ) -> RoaringBitmap;

    /// What `field` can be followed along; `None` for a field without values.
    fn follow(&self, field: &str) -> Option<Follow>;
    /// The ids whose value of `field` points at `target` (the referent of a
    /// `ref`, the parent of a `tree_ref`).
    fn referrers(&self, field: &str, target: Uuid) -> Option<Cow<'_, RoaringBitmap>>;
    /// A forest's roots: the ids placed under the root sentinel.
    fn tree_roots(&self, field: &str) -> RoaringBitmap;
    /// The ids with a position under a parent other than `except`.
    fn tree_parents_except(&self, field: &str, except: Option<Uuid>) -> RoaringBitmap;
    /// The ids with at least one child in `field`'s forest.
    fn parents(&self, field: &str) -> Cow<'_, RoaringBitmap>;

    /// A reader of `field`'s sort representatives (not for a `tree_ref`
    /// field, which sorts on whole paths the evaluator rebuilds).
    fn sort_reps(&self, field: &str, want_max: bool) -> RepReader<'_>;
    /// Walks `field`'s values in sort order — ascending on each id's smallest
    /// value, or descending on its largest (`want_max`) — from `start` on,
    /// inclusive: `visit` receives each id once, with its representative, and
    /// returns `false` to stop. Ties come in no particular order (the
    /// evaluator orders them by uuid). `false` when the source has no ordered
    /// structure to walk for this field, and the page is fetched instead.
    fn walk_values(
        &self,
        _field: &str,
        _want_max: bool,
        _start: Option<&SortRep>,
        _visit: &mut dyn FnMut(&SortRep, u32) -> bool,
    ) -> bool {
        false
    }
    /// The resident bit-sliced index of a numeric or date field holding no
    /// value of another type, which serves a page without reading every
    /// match's key; `None` on a source without one.
    fn bsi(&self, field: &str) -> Option<&FieldIndex>;
}
