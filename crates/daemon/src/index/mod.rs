//! In-memory bitmap/BSI query index (spec-indexing.org).
//!
//! A *derived, read-only* accelerator built from the `field` table. It answers
//! a [`Query`] as a `RoaringBitmap` of dense metarecord ids and is validated
//! against the SQL oracle (the `metafolder-query-oracle` dev crate) by an
//! equivalence battery (`tests/index_oracle.rs`). The oracle is a *test
//! fixture*, not a second engine: nothing falls back to it, and a shape that
//! comes back `Unsupported` is a daemon bug, not a slow answer (see [`Gap`],
//! spec-indexing "No operand runs in SQL").
//!
//! It is built at repo load and refreshed to HEAD per query
//! (`run_query_filter`). Shapes it cannot resolve on its own are handled with
//! caller-supplied seeds ([`QueryRoots`]): `Path`-target follows resolve to a
//! root metarecord through the tree cache, and a single-term `Osm` `Path`
//! resolves to its "term nodes" (the nodes whose *name* contains the term) by
//! scanning the in-memory name partition, which the index then expands into a
//! subtree union — the exact match set, no per-path check. Text predicates
//! (`Matches`, `Osm` `Direct`) run their regex over the field's *distinct
//! values* in memory; the leaves no bitmap can answer — the `:path` aspect, an
//! order-sensitive `Osm` `Path` — are resolved against the forest and rewritten
//! into `UuidIn` sets before the index sees them (`crate::forest_query`). Not
//! persisted — rebuilt each session.

pub mod field_index;
pub mod id_registry;
pub mod source;

use std::borrow::Cow;
use std::collections::HashMap;

use base64::Engine;
use metafolder_core::metarecord::{Value, ZERO_UUID};
use metafolder_core::query::{Aspect, FollowTarget, Query};
use roaring::{MultiOps, RoaringBitmap};

use crate::store::Store;
use uuid::Uuid;

use crate::db;
use field_index::{CmpOp, FieldIndex, SortRep, SortReps};
use id_registry::IdRegistry;
pub use source::{Follow, RepReader, Source};

/// One sort key: a field name and its direction.
#[derive(Debug)]
pub struct SortBy {
    pub field: String,
    pub ascending: bool,
}

/// Pre-resolved `(field, path)` → root metarecord uuid for the `Path`-target
/// `Follows`/`FollowsTransitive` nodes of a query. The index has no tree
/// structure of its own, so the caller resolves path targets through the (now
/// eagerly populated) tree cache and hands the roots in; a path absent from the
/// map resolved to nothing and yields an empty result, matching the oracle.
pub type PathRoots = HashMap<(String, String), Uuid>;

/// Pre-resolved `(field, path)` → the metarecord at exactly that TreeRef path,
/// for the *exact-node* `Eq` operands of a query (spec-query "Exact-node
/// equality": a `/`-bearing string operand on a `tree_ref` field). Resolution
/// lives in the tree cache, so — as with [`PathRoots`] — the caller does it and
/// hands the node in. Unlike the other two maps the value is an `Option`: an
/// entry mapping to `None` says "the caller resolved this path and it is not a
/// node" (an empty result), while a *missing* entry says "nobody resolved it",
/// which keeps the operand `Unsupported` — a daemon bug on the serving path,
/// where `routes::prepare_indexed_query` resolves every one of them.
pub type NodeRoots = HashMap<(String, String), Option<Uuid>>;

/// The caller-resolved seeds a query needs the index to evaluate the shapes it
/// cannot resolve on its own — both are tree-cache lookups: `Path` targets and
/// exact-node `Eq`/`Neq` operands. Bundled so the evaluation threads one
/// context.
#[derive(Default)]
pub struct QueryRoots<'a> {
    pub path: PathRoots,
    pub node: NodeRoots,
    /// Resolver for the full-path sort keys of a `tree_ref` sort key. Like the
    /// two maps above it is a tree-cache lookup the index cannot do itself, but
    /// it is a *resolver* rather than a map: which metarecords need a key is
    /// only known once the query has been evaluated. `None` — or a resolver
    /// whose forest is not fully resident — makes a `tree_ref` sort
    /// [`Unsupported`] with a [`Gap::State`]: a daemon bug, reported as one,
    /// *not* a fall back to SQL — a repository that serves at all has a
    /// resident forest (`RepoState::warmup`).
    pub keys: Option<&'a crate::tree_cache::SortKeys<'a>>,
}

impl QueryRoots<'_> {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Collects the `(field, path)` of every `Path`-target `Follows`/
/// `FollowsTransitive` in `q`, so the caller can resolve them in one pass
/// before evaluation.
pub fn collect_path_targets(q: &Query, out: &mut Vec<(String, String)>) {
    match q {
        Query::Follows { field, target } | Query::FollowsTransitive { field, target, .. } => {
            if let FollowTarget::Path(p) = target {
                out.push((field.clone(), p.clone()));
            }
            if let FollowTarget::Condition(c) = target {
                collect_path_targets(c, out);
            }
        }
        Query::And { operands } | Query::Or { operands } => {
            operands.iter().for_each(|o| collect_path_targets(o, out));
        }
        Query::Not { operand } => collect_path_targets(operand, out),
        Query::SameAs { target, .. } => collect_path_targets(target, out),
        _ => {}
    }
}

/// Collects the `(field, path)` of every *exact-node* `Eq`/`Neq` operand in `q`
/// — a string operand containing the path separator, which on a `tree_ref` field
/// is a node match rather than a `value_name` compare (spec-query "Exact-node
/// equality"). The caller resolves each through the tree cache into
/// [`NodeRoots`]; the index then answers `mfr_path = "/a/b.txt"` from a single
/// interned id, where the oracle scans every row of the field.
///
/// A `/`-bearing operand on a plain *string* field is ordinary literal equality
/// and is collected too — harmlessly, since it resolves to no node and the
/// index's type check never consults the entry.
pub fn collect_node_paths(q: &Query, out: &mut Vec<(String, String)>) {
    match q {
        Query::Eq { field, value: Value::String(s), aspect: Aspect::Raw | Aspect::Parent } => {
            out.push((field.clone(), s.clone()));
        }
        Query::Neq { field, value: Value::String(s), aspect: Aspect::Raw | Aspect::Parent } => {
            out.push((field.clone(), s.clone()));
        }
        Query::And { operands } | Query::Or { operands } => {
            operands.iter().for_each(|o| collect_node_paths(o, out));
        }
        Query::Not { operand } => collect_node_paths(operand, out),
        Query::Follows { target, .. } | Query::FollowsTransitive { target, .. } => {
            if let FollowTarget::Condition(c) = target {
                collect_node_paths(c, out);
            }
        }
        Query::SameAs { target, .. } => collect_node_paths(target, out),
        _ => {}
    }
}

/// Whether a `terms` list is the single term the index serves natively for
/// `Osm` `Path`: the union of the subtrees rooted at the nodes whose name
/// contains it *is* the match set, no ordered verification needed. Any length —
/// the name scan runs in memory over the distinct names, so no minimum term
/// length applies (the FTS trigram index that once imposed a three-character
/// floor is gone).
///
/// Two shapes are excluded. Several terms are order-sensitive. And a term
/// *containing the separator* (`path = "music/jazz"` is one term, the tag
/// syntax's anchored form) can only match across segments, so no single node
/// name ever contains it — seeding from name matches would silently answer
/// "nothing". Both stay with the caller, which checks the assembled path.
pub fn osm_path_indexable(terms: &[String]) -> Option<&str> {
    match terms {
        [only] if !only.contains('/') => Some(only.as_str()),
        _ => None,
    }
}

/// One sort key's resolved lookups: the source's representative reader — or,
/// on a `tree_ref` field, the tree-cache resolver that rebuilds full-path keys.
struct KeyLookup<'a> {
    reps: Option<RepReader<'a>>,
    /// Set on a `tree_ref` field: `(field name, resolver)`. A tree value sorts
    /// on its whole path, which is not stored anywhere (a directory rename
    /// would stale it), so it is rebuilt per query from the tree cache.
    tree: Option<(&'a str, &'a crate::tree_cache::SortKeys<'a>)>,
    want_max: bool,
}

impl KeyLookup<'_> {
    /// A metarecord's representative for this key: a tree field's from the
    /// path resolver, any other from the source.
    fn rep(&self, id: u32, uuid: Uuid) -> Option<SortRep> {
        if let Some((field, keys)) = self.tree {
            return keys.pick(field, uuid, self.want_max).map(SortRep::Tree);
        }
        self.reps.as_ref().and_then(|reps| reps(id))
    }
}

/// The metarecord whose descendants hold every match of `q` in `field`'s
/// forest, when the query says so: a path-target follow (direct, or strict
/// transitive) on that field, alone or as an operand of an `and`. A walk of
/// the forest can then start there instead of at the roots.
fn walk_bound(q: &Query, field: &str, roots: &QueryRoots<'_>) -> Option<Uuid> {
    let resolved = |f: &str, target: &FollowTarget| match target {
        FollowTarget::Path(p) if f == field => roots.path.get(&(f.to_string(), p.clone())).copied(),
        _ => None,
    };
    match q {
        Query::Follows { field: f, target } => resolved(f, target),
        Query::FollowsTransitive { field: f, target, inclusive: false } => resolved(f, target),
        Query::And { operands } => operands.iter().find_map(|o| walk_bound(o, field, roots)),
        _ => None,
    }
}

/// Text leaves a page without a count defers to the ids a walk visits
/// (spec-indexing "A page costs the page"): each is a regex over the names of
/// a `tree_ref` field, which the resident forest holds per metarecord.
struct Residual<'q, 'k> {
    leaves: Vec<&'q Query>,
    checks: Vec<(&'q str, regex::Regex)>,
    keys: &'k crate::tree_cache::SortKeys<'k>,
}

impl Residual<'_, '_> {
    fn accepts(&self, uuid: Uuid) -> bool {
        self.checks.iter().all(|(field, re)| self.keys.any_name(field, uuid, &|n| re.is_match(n)))
    }
}

/// What reading one match's sort key costs in a fetch, in walk steps.
const FETCH_COST: u64 = 4;

/// Whether a node is a text predicate — one answered by scanning the field's
/// distinct values. These are the only operands a candidate restriction makes
/// cheaper, so [`RepoIndex::intersect`] evaluates them last.
fn is_text_predicate(q: &Query) -> bool {
    matches!(
        q,
        Query::Matches { .. } | Query::Osm { mode: metafolder_core::query::OsmMode::Direct, .. }
    )
}

/// A metarecord's position in a sort order: one representative per sort key
/// (`None` = the field is absent, which sorts last) plus the uuid tiebreak.
/// This is what a keyset cursor encodes, so pagination resumes *after* a known
/// position rather than at an absolute offset — stable under concurrent edits.
type SortEntry = (Vec<Option<SortRep>>, Uuid);

/// Why the bitmap path declined a query, and what the caller may do about it.
///
/// Three reasons, which used to be one: back when the daemon had a second
/// engine they all led to it. They are not the same thing at all — work the
/// index has not been taught, the index not being in the state it is supposed
/// to be in, and a cursor the client brought from another query. Only the last
/// is the user's doing, and it is the only one that is not a `500`
/// (spec-indexing "Three kinds of gap").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gap {
    /// A query shape the index does not serve. Nothing answers it any more —
    /// the SQL engine is off the serving path (spec-indexing "No operand runs
    /// in SQL") — so this is a daemon bug reported as a `500`, like `State`.
    /// It survives as its own variant because the two say different things to
    /// whoever reads the log: work never taught, against an accelerator in the
    /// wrong state.
    Coverage,
    /// The cursor does not belong to this (query, sort). The one gap that is
    /// the *client's* mistake, so the route answers `400` — as the SQL engine
    /// used to, from its own cursor encoding, when it inherited these.
    Cursor,
    /// The index or the forest is not in the state the engine requires. Through
    /// the API this cannot happen: a repository serves no data until it is warm
    /// (spec-main "POST /repos/load"), so reaching this is a bug in the daemon,
    /// not a query the user asked wrong — and it is reported as one instead of
    /// being absorbed by a silent fall back to SQL.
    State,
}

/// A query the bitmap path declined. See [`Gap`].
#[derive(Debug)]
pub struct Unsupported {
    pub what: String,
    pub gap: Gap,
}

impl Unsupported {
    /// Whether this is the client's cursor mistake (a `400`) rather than a
    /// daemon bug (a `500`).
    pub fn is_cursor(&self) -> bool {
        self.gap == Gap::Cursor
    }
}

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.what)
    }
}

/// A shape the index does not implement yet.
pub(crate) fn unsupported(what: impl Into<String>) -> Unsupported {
    Unsupported { what: what.into(), gap: Gap::Coverage }
}

/// An accelerator that is not in the state the engine requires.
fn not_ready(what: impl Into<String>) -> Unsupported {
    Unsupported { what: what.into(), gap: Gap::State }
}

/// A cursor this (query, sort) cannot resume from — the client's mistake.
fn bad_cursor(what: impl Into<String>) -> Unsupported {
    Unsupported { what: what.into(), gap: Gap::Cursor }
}

/// Whether a value lands in a BSI encoding (Int / Float / DateTime) — whose
/// sort representative is read from the bit-slices, so it is *not* mirrored in
/// the separate sort store.
fn is_bsi_value(value: &Value) -> bool {
    matches!(value, Value::Int(_) | Value::Float(_) | Value::DateTime(_))
}

/// Whether a value needs an entry in the separate sort store. Two kinds do not:
/// a BSI value reads its representative from the bit-slices, and a `TreeRef`
/// sorts on its whole path, rebuilt per query from the tree cache
/// (`tree_cache::SortKeys`) — the bare name the store would hold is not that
/// key, and on `mfr_path` it would be the store's single biggest population.
fn stores_sort_rep(value: &Value) -> bool {
    !is_bsi_value(value) && !matches!(value, Value::TreeRef { .. })
}

/// How a sorted page is produced (spec-indexing "A page costs the page").
/// `Fetch` reads every match's sort key and partially sorts them; `Walk` reads
/// an ordered structure until the page is full; `Auto` — what the daemon uses —
/// picks by estimated cost. Forcing one is for the tests that hold both to the
/// oracle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PageStrategy {
    #[default]
    Auto,
    Fetch,
    Walk,
}

pub struct RepoIndex {
    registry: IdRegistry,
    strategy: PageStrategy,
    /// All interned ids: every metarecord of this repository's database
    /// (`db::list_entries`). Complement base for `Not` / `IsUnknown`.
    ///
    /// This is the whole universe, with nothing to filter out: ownership is
    /// implicit in which database file holds a metarecord (one repository per
    /// file), so a metarecord has exactly one owner and a second one is not
    /// representable. The oracle's `_repo` CTE is the same expression.
    universe: RoaringBitmap,
    /// Per field name: ids with ≥1 non-`Nothing` row.
    present: HashMap<String, RoaringBitmap>,
    /// Per field name: ids with ≥1 `Nothing` row. Independent of `present` —
    /// a metarecord may hold both a real value and a `Nothing` for one field.
    absent: HashMap<String, RoaringBitmap>,
    /// Per field name: the value encoding answering comparisons / traversal.
    fields: HashMap<String, FieldIndex>,
    /// Per field name: the exact `value_type` of its values (e.g. `"string"`
    /// vs `"bool"`, `"int"` vs `"datetime"` — a distinction `fields` collapses).
    /// Backs the field catalog (`GET /repos/:repo/fields`). The repo-wide
    /// single-type invariant keeps this a function of the name; stale entries for
    /// emptied names are harmless (the catalog gates on `present` non-emptiness).
    types: HashMap<String, &'static str>,
    /// Per field name: min/max sort representatives, for `ORDER BY`.
    sort: HashMap<String, SortReps>,
    /// Per `tree_ref` field name: the ids with at least one child in its
    /// forest. A subtree expansion asks only these for their children — the
    /// directories, not every file below them ([`Self::expand_subtrees`]).
    parents: HashMap<String, RoaringBitmap>,
    /// The log HEAD (`log_head.op_id`) this index reflects. The caller only
    /// uses the index while this matches the current HEAD, then rebuilds.
    built_at_head: Option<i64>,
}

impl RepoIndex {
    /// Builds the index from a single pass over the repository's field rows
    /// (every metarecord — one repository per database file).
    pub fn build(store: &dyn Store) -> anyhow::Result<RepoIndex> {
        Self::build_reported(store, &|_, _| {}, &|| false)
    }

    /// [`Self::build`] reporting progress as `(done, total)` metarecords scanned,
    /// for the load progress bar. `progress` is called every few thousand rows
    /// and once at completion. `cancel` is polled at the same cadence: when it
    /// returns true the build bails (spec-tasks "Cancellation"), so a query that
    /// triggered a rebuild can be stopped. Pass `&|| false` for uncancellable
    /// callers (the load warmup).
    pub fn build_reported(
        store: &dyn Store,
        progress: &dyn Fn(u64, u64),
        cancel: &dyn Fn() -> bool,
    ) -> anyhow::Result<RepoIndex> {
        Self::build_inner(store, None, progress, cancel)
    }

    /// [`Self::build_reported`] that also collects every TreeRef position it
    /// scans into `forest` (in `field.id` order), so the caller can populate the
    /// tree cache from the *same* single pass over the `field` table instead of a
    /// second full scan (`db::load_tree_forest`). See `RepoState::warmup`.
    pub fn build_reported_collecting(
        store: &dyn Store,
        forest: &mut Vec<db::TreeRow>,
        progress: &dyn Fn(u64, u64),
        cancel: &dyn Fn() -> bool,
    ) -> anyhow::Result<RepoIndex> {
        Self::build_inner(store, Some(forest), progress, cancel)
    }

    fn build_inner(
        store: &dyn Store,
        mut forest: Option<&mut Vec<db::TreeRow>>,
        progress: &dyn Fn(u64, u64),
        cancel: &dyn Fn() -> bool,
    ) -> anyhow::Result<RepoIndex> {
        let built_at_head = store.head()?;
        let mut registry = IdRegistry::new();
        let mut universe = RoaringBitmap::new();
        for uuid in store.metarecords()? {
            universe.insert(registry.intern(uuid));
        }

        let mut present: HashMap<String, RoaringBitmap> = HashMap::new();
        let mut absent: HashMap<String, RoaringBitmap> = HashMap::new();
        let mut fields: HashMap<String, FieldIndex> = HashMap::new();
        let mut sort: HashMap<String, SortReps> = HashMap::new();
        let mut types: HashMap<String, &'static str> = HashMap::new();
        // One sequential scan of the whole `field` table — routing each row to
        // its owner's dense id — instead of a query per metarecord. The scan is
        // in rowid (`id`) order, so progress against `MAX(id)` is a near-linear
        // bar (`MAX(id)` is an O(1) read, unlike counting the rows); cancellation
        // is polled at the same cadence.
        let total = store.max_row_id()?.max(0) as u64;
        if cancel() {
            anyhow::bail!("index build cancelled");
        }
        let mut scanned_rows: u64 = 0;
        store.for_each_row(&mut |uuid, row| {
            scanned_rows += 1;
            if scanned_rows.is_multiple_of(4096) {
                progress((row.id.max(0) as u64).min(total), total);
                if cancel() {
                    anyhow::bail!("index build cancelled");
                }
            }
            let Some(id) = registry.id(uuid) else { return Ok(()) };
            // Collect TreeRef positions so the caller can populate the tree cache
            // from this pass — the rows arrive in `field.id` order (rowid scan),
            // which is exactly what the cache's position grouping needs.
            if let (Some(sink), Value::TreeRef { parent, name }) =
                (forest.as_deref_mut(), &row.value)
            {
                sink.push(db::TreeRow {
                    id: row.id,
                    field_name: row.name.clone(),
                    uuid,
                    parent: *parent,
                    name: name.clone(),
                });
            }
            match row.value {
                Value::Nothing => {
                    absent.entry(row.name).or_default().insert(id);
                }
                value => {
                    present.entry(row.name.clone()).or_default().insert(id);
                    types.entry(row.name.clone()).or_insert_with(|| value.type_str());
                    if stores_sort_rep(&value) {
                        sort.entry(row.name.clone()).or_default().insert(&value, id);
                    }
                    fields
                        .entry(row.name)
                        .or_insert_with(|| FieldIndex::for_value(&value))
                        .insert(&value, id);
                }
            }
            Ok(())
        })?;
        for fi in fields.values_mut() {
            fi.finalize();
        }
        let parents = fields
            .iter()
            .filter(|(_, fi)| fi.supports_transitive())
            .map(|(name, fi)| {
                let ids = fi.referenced_targets().into_iter().filter_map(|u| registry.id(u));
                (name.clone(), ids.collect())
            })
            .collect();
        progress(total, total);

        Ok(RepoIndex {
            registry,
            strategy: PageStrategy::Auto,
            universe,
            present,
            absent,
            fields,
            sort,
            parents,
            types,
            built_at_head,
        })
    }

    /// Forces a page strategy (tests only: the daemon keeps `Auto`).
    pub fn set_page_strategy(&mut self, strategy: PageStrategy) {
        self.strategy = strategy;
    }

    /// The log HEAD this index reflects (see [`Self::build`]).
    pub fn built_at_head(&self) -> Option<i64> {
        self.built_at_head
    }

    /// Interned dense ids no longer in the universe — deleted metarecords whose
    /// id the incremental path never frees. Heavy ⇒ a rebuild compacts them.
    fn tombstones(&self) -> usize {
        self.registry.len().saturating_sub(self.universe.len() as usize)
    }

    fn tombstones_heavy(&self) -> bool {
        let dead = self.tombstones();
        dead > 4096 && dead * 4 > self.registry.len()
    }

    /// Brings the index up to the current log HEAD. When the new HEAD is a
    /// forward extension of [`Self::built_at_head`] (the common case: writes
    /// appended), the operations in between are replayed incrementally —
    /// recomputing only the touched `(metarecord, field)` cells from the current
    /// DB state. Anything else (a rollback / prune that rewrote history, an
    /// unrecognised op, or `built_at_head` no longer on the chain) triggers a
    /// full rebuild, which is always correct.
    /// `cancel` is polled during a full rebuild (the heavy case), so a query
    /// that triggered one can be stopped (spec-tasks "Cancellation"). The
    /// incremental path is bounded (`REBUILD_OVER` ops) and runs to completion.
    pub fn refresh(&mut self, store: &dyn Store, cancel: &dyn Fn() -> bool) -> anyhow::Result<()> {
        if !self.refresh_incremental(store)? {
            *self = Self::build_reported(store, &|_, _| {}, cancel)?;
        }
        Ok(())
    }

    /// The incremental half of [`Self::refresh`]: brings the index up to HEAD
    /// when the catch-up is a forward replay, and **leaves it untouched**
    /// otherwise, reporting whether it is now at HEAD.
    ///
    /// This is what a writer calls once its revision is committed
    /// ([`crate::state::RepoState::settle_index`]). A writer must not rebuild:
    /// a rebuild is one scan of the whole `field` table with the connection
    /// held, which is precisely what the tree cache stopped doing on the write
    /// path. Declining leaves the rebuild to the next reader — and since every
    /// commit catches up, the delta is one revision and that case stops
    /// arising.
    pub fn refresh_incremental(&mut self, store: &dyn Store) -> anyhow::Result<bool> {
        let head = store.head()?;
        if head == self.built_at_head {
            return Ok(true);
        }
        // HEAD reset to empty: not a forward extension.
        let Some(current) = head else { return Ok(false) };
        let Some(delta) = self.forward_delta(store, current)? else { return Ok(false) };
        // Dead dense ids (deleted metarecords, never reused) have piled up; only
        // a rebuild re-interns the live set and reclaims them.
        if self.tombstones_heavy() {
            return Ok(false);
        }
        self.apply_ops(store, &delta)?;
        self.built_at_head = head;
        Ok(true)
    }

    /// The operations strictly between `built_at_head` and `current_head` along
    /// the HEAD parent chain, oldest first — or `None` if `built_at_head` is not
    /// an ancestor of `current_head` (history was rewritten), an op type is not
    /// one we replay, or the delta is large enough that a rebuild is cheaper.
    ///
    /// The walk *stops at* `built_at_head` ([`Log::ops_until`])
    /// rather than materialising the whole ancestor chain, so it costs the delta
    /// — normally one or two operations — and not the length of the log. This
    /// runs on the read path before every query that follows a write, so walking
    /// to the root here made every such query cost O(total log length) no matter
    /// how few metarecords it matched (measurably: ~90 ms on a 50 k-operation
    /// log, ~6 ms once pruned). Not reaching `built_at_head` within
    /// `REBUILD_OVER` operations is treated like an oversized delta: a full
    /// rebuild, always correct.
    fn forward_delta(
        &self,
        store: &dyn Store,
        current_head: i64,
    ) -> anyhow::Result<Option<Vec<crate::log::OpRow>>> {
        const KNOWN: &[&str] = &[
            "create_metarecord",
            "delete_metarecord",
            "set_metarecord",
            "set_field",
            "append_field",
            "delete_field",
            "file_deleted",
            "file_moved",
            "file_modified",
        ];
        const REBUILD_OVER: usize = 20_000;

        // No anchor to replay from (the index was built on an empty log): the
        // unbounded walk could never have matched either, so rebuild.
        let built_at_head = match self.built_at_head {
            Some(id) => id,
            None => return Ok(None),
        };

        let mut delta = match store.ops_until(current_head, built_at_head, REBUILD_OVER)? {
            crate::log::Delta::Found(ops) => ops,
            // Not on the chain (history was rewritten) or beyond the budget.
            crate::log::Delta::Budget | crate::log::Delta::Unrelated => return Ok(None),
        };
        if delta.iter().any(|op| !KNOWN.contains(&op.op_type.as_str())) {
            return Ok(None);
        }
        delta.reverse();
        Ok(Some(delta))
    }

    /// Applies a forward delta: updates universe membership for created/deleted
    /// metarecords, then recomputes every touched `(metarecord, field)` cell
    /// from its current DB rows (the before-snapshots supply the old values to
    /// clear, so buckets a value left are emptied).
    fn apply_ops(&mut self, store: &dyn Store, delta: &[crate::log::OpRow]) -> anyhow::Result<()> {
        use std::collections::{HashMap, HashSet};

        let mut created: Vec<Uuid> = Vec::new();
        let mut deleted: HashSet<Uuid> = HashSet::new();
        let mut touched: HashMap<(Uuid, String), Vec<Value>> = HashMap::new();

        for op in delta {
            let before = store.snapshots(op.id, false)?;
            match op.op_type.as_str() {
                "create_metarecord" => {
                    created.push(op.entity_uuid);
                    for row in store.snapshots(op.id, true)? {
                        touched.entry((op.entity_uuid, row.name)).or_default();
                    }
                }
                "delete_metarecord" => {
                    deleted.insert(op.entity_uuid);
                    for row in before {
                        touched.entry((op.entity_uuid, row.name)).or_default().push(row.value);
                    }
                }
                "set_metarecord" => {
                    // Whole-record replacement: every old field name (clear its
                    // old values) and every new field name (recompute) is touched.
                    for row in before {
                        touched.entry((op.entity_uuid, row.name)).or_default().push(row.value);
                    }
                    for row in store.snapshots(op.id, true)? {
                        touched.entry((op.entity_uuid, row.name)).or_default();
                    }
                }
                _ => {
                    // A field-scoped op (set/append/delete_field/file_*): the
                    // before-rows are this field's pre-change values.
                    let field = op.field_name.clone().unwrap_or_default();
                    let entry = touched.entry((op.entity_uuid, field)).or_default();
                    for row in before {
                        entry.push(row.value);
                    }
                }
            }
        }

        for uuid in created {
            let id = self.registry.intern(uuid);
            self.universe.insert(id);
        }
        for uuid in &deleted {
            if let Some(id) = self.registry.id(*uuid) {
                self.universe.remove(id);
            }
        }
        for ((uuid, field), old_values) in touched {
            let Some(id) = self.registry.id(uuid) else { continue };
            let new_values: Vec<Value> =
                store.rows_named(uuid, &field)?.into_iter().map(|row| row.value).collect();
            self.recompute_field(id, &field, &old_values, &new_values);
        }
        Ok(())
    }

    /// Replaces metarecord `id`'s contribution to one field: clears it from the
    /// buckets of its old + new values (so emptied values drop it), then re-adds
    /// it for its current non-`Nothing` values. Mirrors the `build` row routing.
    fn recompute_field(&mut self, id: u32, field: &str, old: &[Value], new: &[Value]) {
        if let Some(b) = self.present.get_mut(field) {
            b.remove(id);
        }
        if let Some(b) = self.absent.get_mut(field) {
            b.remove(id);
        }
        if let Some(sr) = self.sort.get_mut(field) {
            sr.remove(id);
        }
        let clear: Vec<&Value> = old.iter().chain(new.iter()).collect();
        if let Some(enc) = self.fields.get_mut(field) {
            enc.clear_member(id, &clear);
        }

        let non_nothing: Vec<&Value> =
            new.iter().filter(|v| !matches!(v, Value::Nothing)).collect();
        if new.iter().any(|v| matches!(v, Value::Nothing)) {
            self.absent.entry(field.to_string()).or_default().insert(id);
        }
        if let Some(&first) = non_nothing.first() {
            self.present.entry(field.to_string()).or_default().insert(id);
            self.types.insert(field.to_string(), first.type_str());
            {
                let enc = self
                    .fields
                    .entry(field.to_string())
                    .or_insert_with(|| FieldIndex::for_value(first));
                enc.set_member(id, &non_nothing);
            }
            if non_nothing.iter().any(|v| stores_sort_rep(v)) {
                let sr = self.sort.entry(field.to_string()).or_default();
                for &v in &non_nothing {
                    if stores_sort_rep(v) {
                        sr.insert(v, id);
                    }
                }
            }
        }
        // The parents this cell left or joined: whether each still has a child.
        if self.fields.get(field).is_some_and(|fi| fi.supports_transitive()) {
            for v in old.iter().chain(new) {
                if let Value::TreeRef { parent: Some(p), .. } = v {
                    self.note_parent(field, *p);
                }
            }
        }
    }

    /// Brings `field`'s parents bitmap in line with whether `parent` has any
    /// child now.
    fn note_parent(&mut self, field: &str, parent: Uuid) {
        let Some(pid) = self.registry.id(parent) else { return };
        let has = self
            .fields
            .get(field)
            .and_then(|fi| fi.referrers_of(parent))
            .is_some_and(|b| !b.is_empty());
        let parents = self.parents.entry(field.to_string()).or_default();
        if has {
            parents.insert(pid);
        } else {
            parents.remove(pid);
        }
    }

    /// The evaluator over this index, with its page strategy.
    pub fn evaluator(&self) -> Eval<'_> {
        Eval { src: self, strategy: self.strategy }
    }

    pub fn count(&self, q: &Query) -> Result<u64, Unsupported> {
        self.evaluator().count(q)
    }

    /// [`Eval::count_with_roots`].
    pub fn count_with_roots(&self, q: &Query, roots: &QueryRoots) -> Result<u64, Unsupported> {
        self.evaluator().count_with_roots(q, roots)
    }

    /// [`Eval::evaluate_sorted`].
    pub fn evaluate_sorted(
        &self,
        q: &Query,
        sort: &[SortBy],
        limit: Option<usize>,
    ) -> Result<Vec<Uuid>, Unsupported> {
        self.evaluator().evaluate_sorted(q, sort, limit)
    }

    /// [`Eval::evaluate_page`].
    pub fn evaluate_page(
        &self,
        q: &Query,
        sort: &[SortBy],
        limit: Option<usize>,
        cursor: Option<&str>,
    ) -> Result<(Vec<Uuid>, Option<String>), Unsupported> {
        self.evaluator().evaluate_page(q, sort, limit, cursor)
    }

    /// [`Eval::evaluate_page_with_roots`].
    pub fn evaluate_page_with_roots(
        &self,
        q: &Query,
        sort: &[SortBy],
        limit: Option<usize>,
        cursor: Option<&str>,
        roots: &QueryRoots,
    ) -> Result<(Vec<Uuid>, Option<String>), Unsupported> {
        self.evaluator().evaluate_page_with_roots(q, sort, limit, cursor, roots)
    }

    /// [`Eval::page_and_count`].
    pub fn page_and_count(
        &self,
        q: &Query,
        sort: &[SortBy],
        limit: Option<usize>,
        cursor: Option<&str>,
        roots: &QueryRoots,
    ) -> Result<(Vec<Uuid>, Option<String>, u64), Unsupported> {
        self.evaluator().page_and_count(q, sort, limit, cursor, roots)
    }

    /// [`Eval::evaluate`].
    pub fn evaluate(&self, q: &Query) -> Result<RoaringBitmap, Unsupported> {
        self.evaluator().evaluate(q)
    }

    /// The `value_type` this field holds, `None` for a field with no
    /// non-`Nothing` row. The type source of
    /// [`crate::query_validate::validate_query_types`] on the serving path —
    /// where the SQL oracle asks the database for the same answer.
    pub fn value_type(&self, field: &str) -> Option<String> {
        self.types.get(field).map(|t| (*t).to_string())
    }

    pub fn to_uuids(&self, bm: &RoaringBitmap) -> Vec<Uuid> {
        bm.iter().filter_map(|id| self.registry.uuid(id)).collect()
    }

    pub fn universe_len(&self) -> usize {
        self.universe.len() as usize
    }

    /// Number of distinct field names indexed.
    pub fn field_count(&self) -> usize {
        self.fields.len()
    }

    /// The distinct `(field_name, value_type)` pairs of the exclusively-owned
    /// universe, optionally restricted to a single value type — the in-memory
    /// equivalent of `db::distinct_field_names` (backs `GET /repos/:repo/fields`).
    /// A name is reported iff it has ≥1 non-`Nothing` row (`present` non-empty),
    /// so emptied names drop out; ordered by name (each name has one type, so the
    /// secondary key is moot). Served from memory, no DB scan.
    pub fn field_catalog(&self, type_filter: Option<&str>) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .present
            .iter()
            .filter(|(_, ids)| !ids.is_empty())
            .filter_map(|(name, _)| {
                let ty = *self.types.get(name)?;
                match type_filter {
                    Some(want) if want != ty => None,
                    _ => Some((name.clone(), ty.to_string())),
                }
            })
            .collect();
        out.sort();
        out
    }

    /// Total number of sort representatives held (min + max per metarecord per
    /// field) — the extra resident cost of `ORDER BY` support.
    pub fn sort_rep_count(&self) -> usize {
        self.sort.values().map(|s| s.len()).sum()
    }

    /// Number of interned dense ids (live + not-yet-reclaimed tombstones).
    pub fn dense_id_count(&self) -> usize {
        self.registry.len()
    }

    /// Approximate resident size of all bitmaps (serialized size), the figure
    /// the memory-budget gate measures (spec-indexing "What to measure").
    pub fn approx_serialized_bytes(&self) -> usize {
        self.universe.serialized_size()
            + field_index::sum_bytes(self.present.values())
            + field_index::sum_bytes(self.absent.values())
            + self.fields.values().map(|f| f.approx_serialized_bytes()).sum::<usize>()
    }
}

/// The query evaluator: the semantics of every query shape, over any
/// [`Source`] of bitmaps.
pub struct Eval<'s> {
    pub src: &'s dyn Source,
    pub strategy: PageStrategy,
}

impl Eval<'_> {
    /// Number of metarecords matching `q` — `O(1)` from the result bitmap,
    /// where a SQL `COUNT` is `O(n)` (the irreducible count wall).
    pub fn count(&self, q: &Query) -> Result<u64, Unsupported> {
        Ok(self.eval(q, None)?.len())
    }

    /// [`Self::count`] with pre-resolved path-target roots (see [`PathRoots`]).
    pub fn count_with_roots(&self, q: &Query, roots: &QueryRoots) -> Result<u64, Unsupported> {
        Ok(self.eval(q, Some(roots))?.len())
    }

    /// Evaluates a query and returns the matching uuids in sort order, truncated
    /// to `limit` (no pagination). See [`Self::evaluate_page`].
    pub fn evaluate_sorted(
        &self,
        q: &Query,
        sort: &[SortBy],
        limit: Option<usize>,
    ) -> Result<Vec<Uuid>, Unsupported> {
        Ok(self.evaluate_page(q, sort, limit, None)?.0)
    }

    /// Evaluates a query into one sorted, paginated page and the cursor for the
    /// next one (present only when `limit` is set and more rows remain).
    /// Reproduces the specified sort semantics (spec-data-model "Sort
    /// specification"): per key the multi-map representative (min ascending /
    /// max descending), the fixed type-group precedence, metarecords lacking
    /// the field last, uuid tiebreak. The cursor is an opaque offset bound to a
    /// hash of (query, sort) — reused against a different query/sort it is
    /// rejected, as it is in the oracle.
    pub fn evaluate_page(
        &self,
        q: &Query,
        sort: &[SortBy],
        limit: Option<usize>,
        cursor: Option<&str>,
    ) -> Result<(Vec<Uuid>, Option<String>), Unsupported> {
        self.page(q, sort, limit, cursor, None)
    }

    /// [`Self::evaluate_page`] with pre-resolved path-target roots ([`PathRoots`]).
    pub fn evaluate_page_with_roots(
        &self,
        q: &Query,
        sort: &[SortBy],
        limit: Option<usize>,
        cursor: Option<&str>,
        roots: &QueryRoots,
    ) -> Result<(Vec<Uuid>, Option<String>), Unsupported> {
        self.page(q, sort, limit, cursor, Some(roots))
    }

    /// One evaluation answering both the page and the total, for a request that
    /// asks for `count`. Evaluating twice — once for the rows, once to count
    /// them — doubled the cost of every counted query, and the list asks for a
    /// count on its first page.
    pub fn page_and_count(
        &self,
        q: &Query,
        sort: &[SortBy],
        limit: Option<usize>,
        cursor: Option<&str>,
        roots: &QueryRoots,
    ) -> Result<(Vec<Uuid>, Option<String>, u64), Unsupported> {
        let matched = self.eval(q, Some(roots))?;
        let total = matched.len();
        let (page, next) = self.page_of(matched, q, sort, limit, cursor, Some(roots))?;
        Ok((page, next, total))
    }

    fn page(
        &self,
        q: &Query,
        sort: &[SortBy],
        limit: Option<usize>,
        cursor: Option<&str>,
        roots: Option<&QueryRoots>,
    ) -> Result<(Vec<Uuid>, Option<String>), Unsupported> {
        // A page without a count need not know every match: a text leaf on a
        // forest's names is checked on the ids a walk visits.
        let keys = roots.and_then(|r| r.keys).filter(|k| k.is_resident());
        if let (Some(_), Some(keys)) = (limit.filter(|&l| l > 0), keys) {
            if let Some((rest, leaves)) = self.deferrable_text(q) {
                let (candidates, residual) = self.defer_text(&rest, leaves, roots, keys)?;
                return self.page_of_with(
                    candidates,
                    Some(&residual),
                    q,
                    sort,
                    limit,
                    cursor,
                    roots,
                );
            }
        }
        let matched = self.eval(q, roots)?;
        self.page_of(matched, q, sort, limit, cursor, roots)
    }

    /// Splits `q` into the operands evaluated as bitmaps and the text leaves
    /// that can wait: a `Matches` on the `value` aspect or an `osm direct` on a
    /// `tree_ref` field (their text is the names the resident forest holds),
    /// alone or among the operands of an `and`. `None` when there is none.
    fn deferrable_text<'q>(&self, q: &'q Query) -> Option<(Vec<&'q Query>, Vec<&'q Query>)> {
        let deferrable = |q: &Query| match q {
            Query::Matches { field, aspect: Aspect::Value, .. }
            | Query::Osm { field, mode: metafolder_core::query::OsmMode::Direct, .. } => {
                self.src.value_type(field.as_str()) == Some("tree_ref")
            }
            _ => false,
        };
        let (rest, leaves): (Vec<&Query>, Vec<&Query>) = match q {
            Query::And { operands } => operands.iter().partition(|o| !deferrable(o)),
            q if deferrable(q) => (Vec::new(), vec![q]),
            _ => return None,
        };
        (!leaves.is_empty()).then_some((rest, leaves))
    }

    /// The candidates of a query whose text leaves wait — its other operands
    /// (text last, as [`Self::intersect`] does), within the holders of each
    /// leaf's field — and the checks the leaves leave to the page.
    fn defer_text<'q, 'k>(
        &self,
        rest: &[&Query],
        leaves: Vec<&'q Query>,
        roots: Option<&QueryRoots>,
        keys: &'k crate::tree_cache::SortKeys<'k>,
    ) -> Result<(RoaringBitmap, Residual<'q, 'k>), Unsupported> {
        let (text, plain): (Vec<&Query>, Vec<&Query>) =
            rest.iter().copied().partition(|o| is_text_predicate(o));
        let mut acc: Option<RoaringBitmap> = None;
        for operand in plain.into_iter().chain(text) {
            let bm = self.eval_within(operand, roots, acc.as_ref())?;
            acc = Some(match acc {
                None => bm,
                Some(prev) => prev & bm,
            });
        }
        let mut candidates = acc.unwrap_or_else(|| self.src.universe().into_owned());
        let mut checks = Vec::new();
        for leaf in &leaves {
            let (field, pattern) = match leaf {
                Query::Matches { field, pattern, .. } => (field, pattern.clone()),
                Query::Osm { field, terms, .. } => (field, crate::query_result::osm_regex(terms)),
                _ => unreachable!("a deferrable leaf"),
            };
            candidates &= &*self.src.present(field.as_str());
            let re = crate::regexp::compile(&pattern)
                .map_err(|e| unsupported(format!("pattern the index cannot compile: {e}")))?;
            checks.push((field.as_str(), re));
        }
        Ok((candidates, Residual { leaves, checks, keys }))
    }

    /// Sorts, cuts and paginates an already-evaluated match set.
    fn page_of(
        &self,
        matched: RoaringBitmap,
        q: &Query,
        sort: &[SortBy],
        limit: Option<usize>,
        cursor: Option<&str>,
        roots: Option<&QueryRoots<'_>>,
    ) -> Result<(Vec<Uuid>, Option<String>), Unsupported> {
        self.page_of_with(matched, None, q, sort, limit, cursor, roots)
    }

    /// [`Self::page_of`] over a candidate set, of which `residual`'s checks
    /// keep only some: a walk applies them to what it visits; otherwise they
    /// are evaluated over every candidate first.
    #[allow(clippy::too_many_arguments)]
    fn page_of_with(
        &self,
        matched: RoaringBitmap,
        residual: Option<&Residual<'_, '_>>,
        q: &Query,
        sort: &[SortBy],
        limit: Option<usize>,
        cursor: Option<&str>,
        roots: Option<&QueryRoots<'_>>,
    ) -> Result<(Vec<Uuid>, Option<String>), Unsupported> {
        use std::cmp::Ordering;
        let guard = page_guard(q, sort);
        let after: Option<SortEntry> = match cursor {
            None => None,
            Some(c) => {
                let (g, entry) =
                    decode_cursor(c, sort.len()).ok_or_else(|| bad_cursor("invalid cursor"))?;
                if g != guard {
                    return Err(bad_cursor(
                        "invalid cursor: it was issued for a different query or sort",
                    ));
                }
                Some(entry)
            }
        };

        // A page read from an ordered structure until it is full, when that is
        // cheaper than sorting the whole match set (spec-indexing "A page costs
        // the page"). `None` means the fetch below does it.
        let mut matched = matched;
        if let Some(limit) = limit.filter(|&l| l > 0) {
            let walked =
                self.walk_page(&matched, q, sort, limit, after.as_ref(), roots, residual)?;
            if let Some(ids) = walked {
                return self.walked_page(ids, limit, guard, sort, roots);
            }
        }
        if let Some(residual) = residual {
            // Not walked: the deferred leaves are evaluated after all, over the
            // candidates — and the exact set may walk where the candidates
            // could not (a top-k over the bit-slices takes no per-id check).
            for leaf in &residual.leaves {
                let bm = self.eval_within(leaf, roots, Some(&matched))?;
                matched &= bm;
            }
            if let Some(limit) = limit.filter(|&l| l > 0) {
                let walked =
                    self.walk_page(&matched, q, sort, limit, after.as_ref(), roots, None)?;
                if let Some(ids) = walked {
                    return self.walked_page(ids, limit, guard, sort, roots);
                }
            }
        }

        // Column-major: one flat run of representatives (`keys.len()` per
        // metarecord) and one of uuids, ordered by a permutation of indices.
        // A `Vec` per metarecord — the obvious shape — meant one heap allocation
        // for every row of the match set, on every page.
        let keys = self.key_lookups(sort, roots)?;
        let width = keys.len();
        let mut reps: Vec<Option<SortRep>> = Vec::with_capacity(matched.len() as usize * width);
        let mut uuids: Vec<Uuid> = Vec::with_capacity(matched.len() as usize);
        for id in &matched {
            let uuid = self.src.uuid(id).expect("interned id");
            reps.extend(keys.iter().map(|k| k.rep(id, uuid)));
            uuids.push(uuid);
        }
        let row = |i: usize| &reps[i * width..(i + 1) * width];
        let cmp = |a: &u32, b: &u32| {
            let (a, b) = (*a as usize, *b as usize);
            cmp_reps(row(a), uuids[a], row(b), uuids[b], sort)
        };

        let mut order: Vec<u32> = (0..uuids.len() as u32).collect();
        // Keyset: keep only what sorts strictly after the cursor position.
        if let Some(after) = &after {
            order.retain(|&i| {
                let i = i as usize;
                cmp_reps(row(i), uuids[i], &after.0, after.1, sort) == Ordering::Greater
            });
        }

        let total = order.len();
        let end = match limit {
            Some(l) => l.min(total),
            None => total,
        };
        // Only the page's `end` smallest entries need to be in order. Partition
        // the rest out in O(n) (`select_nth`) and sort just the page, so a broad
        // query with a small limit no longer pays a full O(n log n) sort of the
        // whole match set. When the page is the whole set the partition is a
        // no-op and this is the plain sort.
        if end > 0 && end < total {
            order.select_nth_unstable_by(end - 1, cmp);
        }
        order[..end].sort_by(cmp);

        let page = order[..end].iter().map(|&i| uuids[i as usize]).collect();
        let next = match limit {
            Some(_) if end > 0 && end < total => {
                let last = order[end - 1] as usize;
                Some(encode_cursor(guard, &(row(last).to_vec(), uuids[last])))
            }
            _ => None,
        };
        Ok((page, next))
    }

    /// Up to `limit + 1` ids of `matched` in sort order after `after`, read
    /// from an ordered structure — or `None` when the fetch is cheaper, or the
    /// walk ran over its budget, or the sort has no walk. The extra id says
    /// whether another page follows.
    #[allow(clippy::too_many_arguments)]
    fn walk_page(
        &self,
        matched: &RoaringBitmap,
        q: &Query,
        sort: &[SortBy],
        limit: usize,
        after: Option<&SortEntry>,
        roots: Option<&QueryRoots<'_>>,
        residual: Option<&Residual<'_, '_>>,
    ) -> Result<Option<Vec<u32>>, Unsupported> {
        let accepts =
            |id: u32, uuid: Uuid| matched.contains(id) && residual.is_none_or(|r| r.accepts(uuid));
        if sort.is_empty() {
            // The default order is the uuid order, which the registry keeps.
            let span = self.src.id_count();
            let Some(budget) = self.walk_budget(matched.len(), span, limit) else {
                return Ok(None);
            };
            let mut out = Vec::new();
            for (steps, (uuid, id)) in self.src.in_uuid_order(after.map(|a| a.1)).enumerate() {
                if steps >= budget {
                    return Ok(None);
                }
                if accepts(id, uuid) {
                    out.push(id);
                    if out.len() > limit {
                        break;
                    }
                }
            }
            return Ok(Some(out));
        }
        if let [key] = sort {
            // A numeric or date key is answered from the bit-slices — unless the
            // field also holds values the slices do not (mixed historical data,
            // which the sort store keeps).
            if let Some(fi) = self.src.bsi(&key.field) {
                // The top-k takes no per-id check: with text still to check,
                // the leaves are evaluated first (see `page_of_with`).
                if self.strategy == PageStrategy::Fetch || residual.is_some() {
                    return Ok(None);
                }
                return Ok(self.bsi_page(matched, fi, !key.ascending, limit, after));
            }
            // A source with an ordered index of the values walks it.
            if self.src.value_type(&key.field).is_some_and(|t| t != "tree_ref") {
                let walked =
                    self.value_page(matched, &accepts, &key.field, !key.ascending, limit, after);
                if walked.is_some() {
                    return Ok(walked);
                }
            }
            // A path sort walks the resident forest in key order.
            let tree = self.src.value_type(key.field.as_str()) == Some("tree_ref");
            let keys = roots.and_then(|r| r.keys).filter(|k| k.is_resident());
            if let (true, Some(keys)) = (tree, keys) {
                let within = roots.and_then(|r| walk_bound(q, &key.field, r));
                let descending = !key.ascending;
                return Ok(self.tree_page(
                    matched, &accepts, &key.field, descending, keys, within, limit, after,
                ));
            }
        }
        Ok(None)
    }

    /// Up to `limit + 1` ids of `matched` sorted on one key, after `after`,
    /// read from the source's ordered values ([`Source::walk_values`]): each
    /// run of equal representatives ordered by uuid, then the ids without a
    /// value, by uuid. `None` to fetch instead: the walk is not expected to be
    /// cheaper, ran over its budget, or the source cannot walk this field.
    #[allow(clippy::too_many_arguments)]
    fn value_page(
        &self,
        matched: &RoaringBitmap,
        accepts: &dyn Fn(u32, Uuid) -> bool,
        field: &str,
        want_max: bool,
        limit: usize,
        after: Option<&SortEntry>,
    ) -> Option<Vec<u32>> {
        let valued = self.src.present(field);
        let budget = self.walk_budget(matched.len(), valued.len(), limit)?;
        let want = limit + 1;
        // Where the walk starts, and where the valueless tail resumes.
        let (walk, start, tail_after) = match after {
            None => (true, None, None),
            Some((reps, cursor)) => match reps.first() {
                Some(Some(rep)) => (true, Some((rep.clone(), *cursor)), None),
                Some(None) => (false, None, Some(*cursor)),
                _ => return None,
            },
        };
        let mut out: Vec<u32> = Vec::new();
        if walk {
            // One run of equal representatives at a time: sorted by uuid, and
            // at the cursor's own value only what follows the cursor.
            let flush = |run: &mut Vec<(Uuid, u32)>, rep: &SortRep, out: &mut Vec<u32>| {
                run.sort_unstable();
                let from = start.as_ref().filter(|(r, _)| r == rep).map(|(_, u)| *u);
                out.extend(run.drain(..).filter(|(u, _)| from.is_none_or(|c| *u > c)).map(|x| x.1));
            };
            let mut run: Vec<(Uuid, u32)> = Vec::new();
            let mut run_rep: Option<SortRep> = None;
            let (mut steps, mut over) = (0usize, false);
            let walked = self.src.walk_values(
                field,
                want_max,
                start.as_ref().map(|(r, _)| r),
                &mut |rep, id| {
                    steps += 1;
                    if steps > budget {
                        over = true;
                        return false;
                    }
                    if run_rep.as_ref() != Some(rep) {
                        if let Some(done) = run_rep.take() {
                            flush(&mut run, &done, &mut out);
                            if out.len() >= want {
                                return false;
                            }
                        }
                        run_rep = Some(rep.clone());
                    }
                    if let Some(uuid) = self.src.uuid(id).filter(|&u| accepts(id, u)) {
                        run.push((uuid, id));
                    }
                    true
                },
            );
            if !walked || over {
                return None;
            }
            if let Some(done) = run_rep {
                flush(&mut run, &done, &mut out);
            }
        }
        if out.len() < want {
            let tail = matched - &*valued;
            out.extend(self.by_uuid_after(&tail, tail_after, want - out.len(), accepts));
        }
        Some(out)
    }

    /// Up to `limit + 1` ids of `matched` in path order (either way) after
    /// `after`: the forest walked in key order from the cursor's node
    /// ([`crate::tree_cache::SortKeys::walk_sorted`]), then the ids in no
    /// forest position, by uuid. `None` to fetch instead: the walk is not
    /// expected to be cheaper, ran over its budget, or refused.
    #[allow(clippy::too_many_arguments)]
    fn tree_page(
        &self,
        matched: &RoaringBitmap,
        accepts: &dyn Fn(u32, Uuid) -> bool,
        field: &str,
        descending: bool,
        keys: &crate::tree_cache::SortKeys<'_>,
        within: Option<Uuid>,
        limit: usize,
        after: Option<&SortEntry>,
    ) -> Option<Vec<u32>> {
        let placed = self.src.present(field);
        let budget = self.walk_budget(matched.len(), placed.len(), limit)?;
        let want = limit + 1;
        let mut out: Vec<u32> = Vec::new();
        let (walk, resume, tail_after) = match after {
            None => (true, None, None),
            Some((reps, cursor)) => match reps.first() {
                Some(Some(SortRep::Tree(k))) => (true, Some((*cursor, k.as_ref())), None),
                Some(None) => (false, None, Some(*cursor)),
                _ => return None,
            },
        };
        if walk {
            let mut steps = 0usize;
            let mut over = false;
            let end = keys.walk_sorted(field, descending, within, resume, &mut |uuid| {
                steps += 1;
                if steps > budget {
                    over = true;
                    return false;
                }
                if let Some(id) = self.src.id(uuid).filter(|&id| accepts(id, uuid)) {
                    out.push(id);
                }
                out.len() < want
            });
            if over || end == crate::tree_cache::WalkEnd::Refused {
                return None;
            }
        }
        if out.len() < want {
            let tail = matched - &*placed;
            out.extend(self.by_uuid_after(&tail, tail_after, want - out.len(), accepts));
        }
        Some(out)
    }

    /// The `n` smallest-uuid ids of `set` after `after`, in uuid order — the
    /// tail of a sort (the ids lacking its key).
    fn by_uuid_after(
        &self,
        set: &RoaringBitmap,
        after: Option<Uuid>,
        n: usize,
        accepts: &dyn Fn(u32, Uuid) -> bool,
    ) -> Vec<u32> {
        let mut rest: Vec<(Uuid, u32)> = set
            .iter()
            .map(|id| (self.src.uuid(id).expect("interned id"), id))
            .filter(|(u, _)| after.is_none_or(|a| *u > a))
            .filter(|&(u, id)| accepts(id, u))
            .collect();
        if n == 0 {
            return Vec::new();
        }
        if rest.len() > n {
            rest.select_nth_unstable(n - 1);
            rest.truncate(n);
        }
        rest.sort_unstable();
        rest.into_iter().map(|(_, id)| id).collect()
    }

    /// Up to `limit + 1` ids of `matched` sorted on one BSI key, after `after`:
    /// the ids equal to the cursor's key that follow it by uuid, then the best
    /// of those after it ([`FieldIndex::bsi_top_k`] — only the page's ids ever
    /// have their key read), then the ids without a value, by uuid. `None` for
    /// a cursor whose key is of another type (the fetch rejects or serves it).
    fn bsi_page(
        &self,
        matched: &RoaringBitmap,
        fi: &FieldIndex,
        want_max: bool,
        limit: usize,
        after: Option<&SortEntry>,
    ) -> Option<Vec<u32>> {
        let want = limit + 1;
        let uuid = |id: u32| self.src.uuid(id).expect("interned id");
        let mut out: Vec<u32> = Vec::new();
        // The valued ids still to place, and where the valueless tail resumes.
        let (rest, tail_after) = match after {
            None => (Some(matched.clone()), None),
            Some((reps, cursor)) => match reps.first() {
                Some(Some(rep)) => {
                    let (later, equal) = fi.bsi_split_after(matched, rep, want_max)?;
                    let mut ties: Vec<(Uuid, u32)> =
                        equal.iter().map(|id| (uuid(id), id)).filter(|(u, _)| u > cursor).collect();
                    ties.sort_unstable();
                    out.extend(ties.into_iter().map(|(_, id)| id).take(want));
                    (Some(later), None)
                }
                _ => (None, Some(*cursor)),
            },
        };
        if let Some(rest) = rest.filter(|_| out.len() < want) {
            let need = want - out.len();
            let (sure, ties) = fi.bsi_top_k(&rest, need as u64, want_max)?;
            let mut ranked: Vec<(SortRep, Uuid, u32)> = sure
                .iter()
                .map(|id| (fi.bsi_sort_rep(id, want_max).expect("valued id"), uuid(id), id))
                .collect();
            ranked.sort_unstable_by(|a, b| {
                let by_rep = if want_max { b.0.cmp(&a.0) } else { a.0.cmp(&b.0) };
                by_rep.then(a.1.cmp(&b.1))
            });
            out.extend(ranked.into_iter().map(|(_, _, id)| id));
            let mut ties: Vec<(Uuid, u32)> = ties.iter().map(|id| (uuid(id), id)).collect();
            ties.sort_unstable();
            out.extend(ties.into_iter().map(|(_, id)| id).take(want - out.len().min(want)));
        }
        if out.len() < want {
            // Lacking the field (or holding only `Nothing`): last, by uuid.
            let tail = matched - fi.bsi_valued().expect("a BSI field");
            out.extend(self.by_uuid_after(&tail, tail_after, want - out.len(), &|_, _| true));
        }
        Some(out)
    }

    /// The steps a walk may take before giving up on it, or `None` to fetch.
    /// A walk over `span` ordered entries finds `limit + 1` of `matches` ids
    /// after ≈ `(limit + 1) × span / matches` steps if they are spread evenly;
    /// the fetch touches every match, at [`FETCH_COST`] steps each. The budget
    /// is twice the fetch, so a walk that meets unevenly spread matches costs
    /// at most three times what the fetch would have.
    fn walk_budget(&self, matches: u64, span: u64, limit: usize) -> Option<usize> {
        let fetch = matches.saturating_mul(FETCH_COST);
        match self.strategy {
            PageStrategy::Fetch => None,
            PageStrategy::Walk => Some(usize::MAX),
            PageStrategy::Auto => {
                let wanted = limit as u64 + 1;
                let walk = if wanted >= matches { span } else { wanted * span / matches.max(1) };
                (walk < fetch).then(|| (fetch.saturating_mul(2)).min(usize::MAX as u64) as usize)
            }
        }
    }

    /// A walked page, finished as the fetch finishes one: the uuids, and the
    /// cursor from the last one when another page follows.
    fn walked_page(
        &self,
        mut ids: Vec<u32>,
        limit: usize,
        guard: u64,
        sort: &[SortBy],
        roots: Option<&QueryRoots<'_>>,
    ) -> Result<(Vec<Uuid>, Option<String>), Unsupported> {
        let more = ids.len() > limit;
        ids.truncate(limit);
        let uuids: Vec<Uuid> =
            ids.iter().map(|&id| self.src.uuid(id).expect("interned id")).collect();
        let next = match (more, ids.last()) {
            (true, Some(&last)) => {
                let uuid = self.src.uuid(last).expect("interned id");
                let keys = self.key_lookups(sort, roots)?;
                let reps = keys.iter().map(|k| k.rep(last, uuid)).collect();
                Some(encode_cursor(guard, &(reps, uuid)))
            }
            _ => None,
        };
        Ok((uuids, next))
    }

    /// A sort key with its two lookups already resolved. Resolving them per
    /// metarecord meant two string-keyed hash lookups for every row of the match
    /// set on every page; the field is the same for all of them.
    fn key_lookups<'a>(
        &'a self,
        sort: &'a [SortBy],
        roots: Option<&'a QueryRoots<'a>>,
    ) -> Result<Vec<KeyLookup<'a>>, Unsupported> {
        sort.iter()
            .map(|k| {
                let tree = if self.src.value_type(k.field.as_str()) == Some("tree_ref") {
                    // A tree sort rebuilds the paths from the resident forest.
                    // The forest is always resident on a repository that
                    // serves at all, so this is an invariant, not a fallback.
                    let keys = roots
                        .and_then(|r| r.keys)
                        .filter(|keys| keys.is_resident())
                        .ok_or_else(|| not_ready("tree_ref sort without a resident forest"))?;
                    Some((k.field.as_str(), keys))
                } else {
                    None
                };
                let reps = tree.is_none().then(|| self.src.sort_reps(&k.field, !k.ascending));
                Ok(KeyLookup { reps, tree, want_max: !k.ascending })
            })
            .collect()
    }

    /// Evaluates a query to the bitmap of matching dense ids (path targets
    /// unsupported; use [`Self::evaluate_page_with_roots`] for those).
    pub fn evaluate(&self, q: &Query) -> Result<RoaringBitmap, Unsupported> {
        self.eval(q, None)
    }

    /// Evaluates a query to the bitmap of matching dense ids. `roots` is
    /// `Some(map)` once the caller has resolved the query's `Path` targets (see
    /// [`PathRoots`]); `None` means they have not, so a `Path` target is
    /// reported `Unsupported` — which on the serving path is a daemon bug, the
    /// route having resolved them before it evaluates anything.
    ///
    /// Every variant of the IR is handled here — the match is exhaustive on
    /// purpose, so a new one cannot be added without deciding whether the index
    /// serves it. What comes back `Unsupported` is a property of the *operand*
    /// (an unresolved path, a pattern that will not compile, a comparison the
    /// encoding cannot answer), never of the node type.
    fn eval(&self, q: &Query, roots: Option<&QueryRoots>) -> Result<RoaringBitmap, Unsupported> {
        self.eval_within(q, roots, None)
    }

    /// [`Self::eval`] restricted to a candidate set: the answer is intersected
    /// with `restrict`, and a text predicate uses it to skip the values that
    /// cannot contribute. Only the text scans read it; everything else is a
    /// bitmap operation already, and intersecting afterwards costs the same.
    ///
    /// It propagates through `And` and `Or` — `(a ∪ b) ∩ r = (a ∩ r) ∪ (b ∩ r)`
    /// — and stops at `Not` and at a `Follows` target, where a restricted
    /// operand would give the wrong complement or the wrong traversal seed.
    fn eval_within(
        &self,
        q: &Query,
        roots: Option<&QueryRoots>,
        restrict: Option<&RoaringBitmap>,
    ) -> Result<RoaringBitmap, Unsupported> {
        match q {
            // The `parent`/`path` aspects read a component the bitmaps do not
            // hold (the parent uuid, the assembled path), so each has its own
            // path here. The 400 for an aspect the field's type cannot serve is
            // raised upstream by `query_validate`, before any of this runs, so
            // there is one answer per query whatever serves it.
            Query::IsPresent { field, aspect } => match aspect {
                Aspect::Parent => self.parent_presence(field, true),
                Aspect::Path => self.path_presence(field, true),
                _ => {
                    Self::index_servable_aspect(*aspect)?;
                    Ok(self.present_of(field))
                }
            },
            Query::IsAbsent { field, aspect } => match aspect {
                Aspect::Parent => self.parent_presence(field, false),
                Aspect::Path => self.path_presence(field, false),
                _ => {
                    Self::index_servable_aspect(*aspect)?;
                    Ok(self.absent_of(field))
                }
            },
            Query::IsUnknown { field } => {
                // universe − {records with any row of `field`} (present ∪ absent),
                // matching the oracle's `_repo WHERE uuid NOT IN (any field row)`.
                let mut r = self.src.universe().into_owned();
                r -= &self.present_of(field);
                r -= &self.absent_of(field);
                Ok(r)
            }

            Query::Eq { field, value, aspect } => {
                self.compare(field, CmpOp::Eq, value, roots, *aspect)
            }
            Query::Neq { field, value, aspect } => {
                self.compare(field, CmpOp::Neq, value, roots, *aspect)
            }
            Query::Lt { field, value, aspect } => {
                self.compare(field, CmpOp::Lt, value, roots, *aspect)
            }
            Query::Lte { field, value, aspect } => {
                self.compare(field, CmpOp::Lte, value, roots, *aspect)
            }
            Query::Gt { field, value, aspect } => {
                self.compare(field, CmpOp::Gt, value, roots, *aspect)
            }
            Query::Gte { field, value, aspect } => {
                self.compare(field, CmpOp::Gte, value, roots, *aspect)
            }

            Query::And { operands } => self.intersect(operands, roots, restrict),
            Query::Or { operands } => {
                let mut acc = RoaringBitmap::new();
                for operand in operands {
                    acc |= self.eval_within(operand, roots, restrict)?;
                }
                if operands.is_empty() {
                    return Err(unsupported("'and'/'or' need an operand"));
                }
                Ok(acc)
            }
            Query::Not { operand } => {
                let mut r = self.src.universe().into_owned();
                r -= &self.eval(operand, roots)?;
                if let Some(restrict) = restrict {
                    r &= restrict;
                }
                Ok(r)
            }

            Query::Follows { field, target } => self.follows(field, target, roots),
            // Like a traversal seed, the target is evaluated *unrestricted*:
            // narrowing it would drop the very records whose values define the
            // answer. The answer itself is then intersected by the shared tail.
            Query::SameAs { field, target } => {
                let seed = self.eval(target, roots)?;
                Ok(if seed.is_empty() {
                    RoaringBitmap::new()
                } else {
                    self.src.same_as(field, &seed)
                })
            }
            Query::FollowsTransitive { field, target, inclusive } => {
                self.follows_transitive(field, target, *inclusive, roots)
            }

            Query::Osm { field, terms, mode: metafolder_core::query::OsmMode::Path } => {
                self.osm_path(field, terms)
            }

            Query::UuidIn { uuids } => {
                // Interned ids of the given uuids, restricted to the universe
                // (unknown / non-owned uuids drop out).
                let mut r = RoaringBitmap::new();
                for u in uuids {
                    if let Some(id) = self.src.id(*u) {
                        r.insert(id);
                    }
                }
                r &= &*self.src.universe();
                Ok(r)
            }

            Query::Matches { field, pattern, aspect } => {
                Self::index_servable_aspect(*aspect)?;
                // `raw` on a tree_ref is an error, not a name match — rejected
                // upstream by `query_validate` (spec-query "Field aspects"), so
                // this is a backstop. Under `value` the scan is the ordinary
                // name scan.
                if *aspect == Aspect::Raw && self.src.value_type(field) == Some("tree_ref") {
                    return Err(unsupported("MATCHES on a tree_ref field needs an aspect"));
                }
                self.text_scan(field, pattern, restrict)
            }
            // OSM `Direct` matches the row's own text with the very regex the
            // oracle hands its `REGEXP` UDF, so the two cannot drift — in
            // particular over `.`, which does not cross a newline.
            Query::Osm { field, terms, mode: metafolder_core::query::OsmMode::Direct } => {
                self.text_scan(field, &crate::query_result::osm_regex(terms), restrict)
            }
        }
        .map(|mut bm| {
            // Every branch above either consumed `restrict` itself (the text
            // scans, the combinators) or ignored it; intersecting again is
            // idempotent and keeps the contract in one place.
            if let Some(restrict) = restrict {
                bm &= restrict;
            }
            bm
        })
    }

    /// A regex text predicate (`Matches`, OSM `Direct`) answered by scanning the
    /// field's *distinct* values in memory — its cardinality, not its row count,
    /// and no SQL at all. An invalid or oversized pattern is a `400` raised
    /// upstream by `query_validate`, so the `Unsupported` here is a backstop. A
    /// field with no indexed value matches nothing, which is what the oracle
    /// answers too.
    fn text_scan(
        &self,
        field: &str,
        pattern: &str,
        restrict: Option<&RoaringBitmap>,
    ) -> Result<RoaringBitmap, Unsupported> {
        if self.src.follow(field).is_none() {
            return Ok(RoaringBitmap::new());
        }
        let re = crate::regexp::compile(pattern)
            .map_err(|e| unsupported(format!("pattern the index cannot compile: {e}")))?;
        let literals = crate::regexp::required_literals(pattern);
        Ok(self.src.scan_text(field, &|text| re.is_match(text), &literals, restrict))
    }

    /// Direct `Follows`: referrers of every metarecord matching the sub-query.
    /// Direct referrers of the target metarecords. A `Path` target is resolved
    /// through `roots` (the tree cache, upstream) to a single root metarecord;
    /// a `Condition` target is evaluated to its match set.
    /// The root metarecord a `Path` target resolves to, looked up in the
    /// caller-supplied `roots`. `None` roots means the caller did not resolve
    /// path targets, so this shape is `Unsupported` (a daemon bug on the
    /// serving path); a path absent from a supplied map resolved to nothing
    /// (`Ok(None)`, empty result).
    fn resolved_root(
        &self,
        field: &str,
        path: &str,
        roots: Option<&QueryRoots>,
    ) -> Result<Option<Uuid>, Unsupported> {
        match roots {
            None => Err(unsupported("path-target follows")),
            Some(roots) => Ok(roots.path.get(&(field.to_string(), path.to_string())).copied()),
        }
    }

    /// `field:parent IS ABSENT` / `IS PRESENT` (spec-query "Forest roots"):
    /// answered from the reverse index's parent partition, where the roots are
    /// the sentinel's own bucket — one hash lookup for the question the oracle
    /// answers by scanning every row of the field.
    ///
    /// The `400` the aspect deserves on a non-`tree_ref` field is raised
    /// upstream by `query_validate`, so a field the index does not hold as a
    /// forest reaching here is a backstop; a field with no indexed value at all
    /// is vacuously empty in both engines.
    fn parent_presence(&self, field: &str, present: bool) -> Result<RoaringBitmap, Unsupported> {
        match self.src.follow(field) {
            None => Ok(RoaringBitmap::new()),
            Some(Follow::Tree) if present => {
                Ok(self.src.tree_parents_except(field, Some(ZERO_UUID)))
            }
            Some(Follow::Tree) => Ok(self.src.tree_roots(field)),
            Some(_) => Err(unsupported("the ':parent' aspect")),
        }
    }

    /// `field:path IS PRESENT` / `IS ABSENT`: a node has an assembled path
    /// exactly when it has a `tree_ref` row, so the aspect adds nothing to read
    /// and this is the raw presence — once the field is known to be a forest.
    /// On anything else `:path` is a `400`, raised upstream by
    /// `query_validate`.
    fn path_presence(&self, field: &str, present: bool) -> Result<RoaringBitmap, Unsupported> {
        if self.src.value_type(field).is_some_and(|t| t != "tree_ref") {
            return Err(unsupported("the ':path' aspect"));
        }
        Ok(if present { self.present_of(field) } else { self.absent_of(field) })
    }

    /// `field:parent = "<path>"`: the direct children of the node the caller
    /// resolved through the tree cache — the very bucket a path-target
    /// `Follows` reads, which is what the spec means by "the same set as
    /// `field -> \"<path>\"`, spelled as a comparison".
    ///
    /// `Neq` is the mirror: every node under *another* parent — a forest root
    /// included, and everybody when the path resolves to nothing. It is not the
    /// complement of `Eq` (the predicate asks for one differing row, so a
    /// multi-position node is in both sets). A regex or an ordered operand
    /// reads a uuid and is a `400`, raised upstream by `query_validate`.
    fn parent_compare(
        &self,
        field: &str,
        op: CmpOp,
        value: &Value,
        roots: Option<&QueryRoots>,
    ) -> Result<RoaringBitmap, Unsupported> {
        if !matches!(op, CmpOp::Eq | CmpOp::Neq) {
            return Err(unsupported("the ':parent' aspect"));
        }
        let Value::String(path) = value else {
            return Err(unsupported("the ':parent' aspect"));
        };
        match self.src.follow(field) {
            None => return Ok(RoaringBitmap::new()),
            Some(Follow::Tree) => {}
            Some(_) => return Err(unsupported("the ':parent' aspect")),
        }
        // A missing entry means nobody resolved this path: `Unsupported`,
        // exactly as the exact-node equality is. An entry mapping to `None`
        // resolved to no node, a "0" predicate — an empty result.
        let Some(resolved) = roots.and_then(|r| r.node.get(&(field.to_string(), path.clone())))
        else {
            return Err(unsupported("unresolved ':parent' path"));
        };
        Ok(match op {
            CmpOp::Eq => match resolved {
                None => RoaringBitmap::new(),
                Some(node) => {
                    self.src.referrers(field, *node).map(Cow::into_owned).unwrap_or_default()
                }
            },
            // `*resolved` is `None` for a path that is no node: the predicate
            // is then `NOT (0)`, every row of the field — which is exactly what
            // excluding no bucket gives.
            _ => self.src.tree_parents_except(field, *resolved),
        })
    }

    fn follows(
        &self,
        field: &str,
        target: &FollowTarget,
        roots: Option<&QueryRoots>,
    ) -> Result<RoaringBitmap, Unsupported> {
        let target_uuids: Vec<Uuid> = match target {
            FollowTarget::Path(p) => match self.resolved_root(field, p, roots)? {
                Some(root) => vec![root],
                None => return Ok(RoaringBitmap::new()), // path resolved to nothing
            },
            FollowTarget::Condition(cond) => {
                self.eval(cond, roots)?.iter().filter_map(|tid| self.src.uuid(tid)).collect()
            }
        };
        if !matches!(self.src.follow(field), Some(Follow::Direct | Follow::Tree)) {
            return Ok(RoaringBitmap::new());
        }
        let referrers: Vec<Cow<'_, RoaringBitmap>> =
            target_uuids.into_iter().filter_map(|uuid| self.src.referrers(field, uuid)).collect();
        Ok(referrers.iter().map(|b| &**b).union())
    }

    /// Transitive `Follows`: all descendants of the sub-query's matches, by
    /// iterative bitmap expansion over the reverse (direct-children) index
    /// (spec-indexing "FollowsTransitive by iterative bitmap expansion").
    fn follows_transitive(
        &self,
        field: &str,
        target: &FollowTarget,
        inclusive: bool,
        roots: Option<&QueryRoots>,
    ) -> Result<RoaringBitmap, Unsupported> {
        // Seed the expansion with the matching roots' dense ids. For a path
        // target that is the single metarecord resolved through the tree cache;
        // for a condition it is the sub-query's match set. (Resolve the seed
        // before the index-support check so an unsupported sub-query still
        // surfaces rather than being masked by an empty answer.)
        let frontier = match target {
            FollowTarget::Path(p) => match self.resolved_root(field, p, roots)? {
                Some(root) => match self.src.id(root) {
                    Some(id) => RoaringBitmap::from_iter([id]),
                    None => return Ok(RoaringBitmap::new()), // root not in the index
                },
                None => return Ok(RoaringBitmap::new()), // path resolved to nothing
            },
            FollowTarget::Condition(cond) => self.eval(cond, roots)?,
        };
        if self.src.follow(field) != Some(Follow::Tree) {
            return Ok(RoaringBitmap::new());
        }
        // The inclusive form (`=>*`) keeps the root(s) in the result (whole
        // subtree); the strict form (`->*`) grows only downward from them.
        Ok(self.expand_subtrees(field, frontier, inclusive))
    }

    /// Grows `frontier` downward over the reverse (direct-children) index of
    /// `fi` until fixpoint, returning every reachable node. `inclusive` keeps the
    /// seed nodes themselves (whole subtree); otherwise only their descendants.
    /// The shared core of `FollowsTransitive` and single-term `Osm` `Path`.
    fn expand_subtrees(
        &self,
        field: &str,
        mut frontier: RoaringBitmap,
        inclusive: bool,
    ) -> RoaringBitmap {
        let mut result =
            if inclusive { &frontier & &*self.src.universe() } else { RoaringBitmap::new() };
        // A source keeping descendant bitmaps answers in one read per node.
        if let Some(below) = self.src.descendants(field, &frontier) {
            result |= below;
            return result;
        }
        let parents = self.src.parents(field);
        while !frontier.is_empty() {
            // Only the frontier's directories have children to ask for, and
            // one union takes them all: `|=` per directory copied the growing
            // level once per directory.
            let asked = &frontier & &*parents;
            let children: Vec<Cow<'_, RoaringBitmap>> = asked
                .iter()
                .filter_map(|nid| self.src.uuid(nid))
                .filter_map(|uuid| self.src.referrers(field, uuid))
                .collect();
            let mut next: RoaringBitmap = children.iter().map(|b| &**b).union();
            next -= &result; // only newly discovered nodes; also breaks cycles
            result |= &next;
            frontier = next;
        }
        result
    }

    /// Single-term `Osm` `Path`: the union of the subtrees rooted at the term
    /// nodes — the nodes whose name contains the term. Every such node's
    /// descendants have the term in their path, so the inclusive subtree union
    /// *is* the match set, with no per-path verification. Multi-term is
    /// order-sensitive and stays `Unsupported` (the caller resolves it).
    fn osm_path(&self, field: &str, terms: &[String]) -> Result<RoaringBitmap, Unsupported> {
        // `osm` path mode is tree_ref-only: a field holding any other type is a
        // user error `query_validate` reports as a 400 with the "use osmd" hint
        // (spec-query), before this runs. Decline rather than answer with an
        // empty bitmap, which would turn that mistake into a silent "no rows".
        // A field with no values at all is vacuously empty in both engines.
        if self.src.value_type(field).is_some_and(|t| t != "tree_ref") {
            return Err(unsupported("osm path on a non-tree_ref field"));
        }
        // A blank query (the search box emptied) matches every metarecord with a
        // path in this forest — the oracle scans for `value_type='tree_ref'`,
        // which on a tree_ref field is exactly the `present` set.
        if terms.is_empty() {
            return Ok(self.present_of(field));
        }
        let Some(term) = osm_path_indexable(terms) else {
            return Err(unsupported("multi-term osm path"));
        };
        if self.src.follow(field) != Some(Follow::Tree) {
            return Ok(RoaringBitmap::new());
        }
        // The "term nodes" — those whose *name* contains the term — resolved
        // from the in-memory name partition. The oracle finds them with
        // `value_name REGEXP '(?i)<escaped term>'`, so use that very regex on
        // each distinct name: same case folding, same escaping, no divergence.
        // It also works below the three-character floor of the old FTS trigram
        // index, where the first keystrokes of a search used to fall off a
        // cliff.
        let re = crate::regexp::compile(&format!("(?i){}", regex::escape(term)))
            .map_err(|e| unsupported(format!("osm term is not a usable pattern: {e}")))?;
        let literals = [term.to_lowercase()];
        let seeds = self.src.scan_names(field, &|name| re.is_match(name), &literals, None);
        Ok(self.expand_subtrees(field, seeds, true))
    }

    /// `And`, evaluated cheapest-first: the operands that are pure bitmap work
    /// go first, and the running intersection is then handed to the text scans
    /// as their candidate set. This is what makes "this folder, name matching X"
    /// cost the folder's size rather than the repository's — the scan skips
    /// every value whose bitmap misses the candidates.
    ///
    /// The order is a property of the query shape, so it does not change what a
    /// paginated session is served by.
    fn intersect(
        &self,
        operands: &[Query],
        roots: Option<&QueryRoots>,
        restrict: Option<&RoaringBitmap>,
    ) -> Result<RoaringBitmap, Unsupported> {
        if operands.is_empty() {
            return Err(unsupported("'and'/'or' need an operand"));
        }
        let (text, rest): (Vec<&Query>, Vec<&Query>) =
            operands.iter().partition(|o| is_text_predicate(o));

        let mut acc: Option<RoaringBitmap> = restrict.cloned();
        for operand in rest.into_iter().chain(text) {
            let bm = self.eval_within(operand, roots, acc.as_ref())?;
            acc = Some(match acc {
                None => bm,
                Some(prev) => prev & bm,
            });
        }
        Ok(acc.expect("at least one operand"))
    }

    /// Dispatches a comparison to the field's encoding. A field with no
    /// non-`Nothing` rows has no encoding, so the comparison is empty — exactly
    /// the oracle's result (the `value_type` filter excludes every `Nothing`
    /// row). The aspects this generic gate lets through: `raw` and `value` read
    /// the row's own data, which the per-field index holds. `path` reads the
    /// assembled path, which lives in the tree cache, so it is declined here and
    /// answered before the index runs, by `crate::forest_query` rewriting the
    /// leaf into a `uuid_in` set. So is `parent` *here* — its two servable
    /// shapes (presence and equality) have their own paths
    /// ([`Self::parent_presence`], [`Self::parent_compare`]), and every other
    /// one (a regex over a uuid) is a `400` (spec-query "Field aspects").
    fn index_servable_aspect(aspect: Aspect) -> Result<(), Unsupported> {
        match aspect {
            Aspect::Raw | Aspect::Value => Ok(()),
            Aspect::Parent => Err(unsupported("the ':parent' aspect")),
            Aspect::Path => Err(unsupported("the ':path' aspect")),
        }
    }

    fn compare(
        &self,
        field: &str,
        op: CmpOp,
        value: &Value,
        roots: Option<&QueryRoots>,
        aspect: Aspect,
    ) -> Result<RoaringBitmap, Unsupported> {
        if matches!(value, Value::Nothing) {
            return Err(unsupported("comparison with 'nothing'"));
        }
        if aspect == Aspect::Parent {
            return self.parent_compare(field, op, value, roots);
        }
        Self::index_servable_aspect(aspect)?;
        // A bare ordered comparison on a tree_ref is an error rather than a
        // name compare — a `400` raised upstream by `query_validate`, so this
        // is a backstop.
        if aspect == Aspect::Raw
            && !matches!(op, CmpOp::Eq | CmpOp::Neq)
            && self.src.value_type(field) == Some("tree_ref")
        {
            return Err(unsupported("ordered comparison on a tree_ref field needs an aspect"));
        }
        // Exact-node path (spec-query "Field aspects"): under the default `raw`
        // aspect, an Eq/Neq string operand on a tree_ref field is a path-resolved
        // node match. The resolution lives in the tree cache, not
        // the index, so both `Eq` and `Neq` are served only from a caller-supplied
        // [`NodeRoots`] entry (`Neq` as "present minus the node", below); without
        // one, decline rather than answer with the (wrong, value_name-based)
        // bitmap. A string field keeps literal equality (the index handles it).
        if matches!(op, CmpOp::Eq | CmpOp::Neq) && aspect == Aspect::Raw {
            if let Value::String(s) = value {
                if self.src.value_type(field) == Some("tree_ref") {
                    // A missing entry means nobody resolved this path: decline.
                    let resolved = roots.and_then(|r| r.node.get(&(field.to_string(), s.clone())));
                    let Some(node) = resolved else {
                        return Err(unsupported("exact-node tree_ref path equality"));
                    };
                    // The node itself, restricted to the metarecords that do
                    // carry a value for this field — the match is on a
                    // `field_name` row of type tree_ref, so a node whose rows
                    // are all `Nothing` matches nothing either. An entry
                    // mapping to `None` resolved to no node: no match at all.
                    let eq = match node {
                        Some(node) => match self.src.id(*node) {
                            Some(id) => RoaringBitmap::from_iter([id]) & self.present_of(field),
                            None => RoaringBitmap::new(),
                        },
                        None => RoaringBitmap::new(),
                    };
                    if matches!(op, CmpOp::Eq) {
                        return Ok(eq);
                    }
                    // `Neq` is *not* the complement: it asks for ≥1 non-Nothing
                    // row that is not the `Eq` match, so a metarecord with no
                    // value for the field is in neither. On a tree_ref field
                    // that is every path-bearing metarecord but the node.
                    let mut out = self.present_of(field);
                    out -= &eq;
                    return Ok(out);
                }
            }
        }
        self.src.compare(field, op, value)
    }

    fn present_of(&self, field: &str) -> RoaringBitmap {
        self.src.present(field).into_owned()
    }

    fn absent_of(&self, field: &str) -> RoaringBitmap {
        self.src.absent(field).into_owned()
    }
}

/// The resident index as a [`Source`]: every answer from memory.
impl Source for RepoIndex {
    fn universe(&self) -> Cow<'_, RoaringBitmap> {
        Cow::Borrowed(&self.universe)
    }

    fn present(&self, field: &str) -> Cow<'_, RoaringBitmap> {
        self.present.get(field).map_or_else(|| Cow::Owned(RoaringBitmap::new()), Cow::Borrowed)
    }

    fn absent(&self, field: &str) -> Cow<'_, RoaringBitmap> {
        self.absent.get(field).map_or_else(|| Cow::Owned(RoaringBitmap::new()), Cow::Borrowed)
    }

    fn value_type(&self, field: &str) -> Option<&str> {
        self.types.get(field).copied()
    }

    fn id(&self, uuid: Uuid) -> Option<u32> {
        self.registry.id(uuid)
    }

    fn uuid(&self, id: u32) -> Option<Uuid> {
        self.registry.uuid(id)
    }

    fn id_count(&self) -> u64 {
        self.registry.len() as u64
    }

    fn in_uuid_order(&self, after: Option<Uuid>) -> Box<dyn Iterator<Item = (Uuid, u32)> + '_> {
        Box::new(self.registry.in_uuid_order(after))
    }

    fn compare(&self, field: &str, op: CmpOp, value: &Value) -> Result<RoaringBitmap, Unsupported> {
        match self.fields.get(field) {
            Some(fi) => fi.compare(op, value),
            None => Ok(RoaringBitmap::new()),
        }
    }

    fn same_as(&self, field: &str, seed: &RoaringBitmap) -> RoaringBitmap {
        self.fields.get(field).map(|fi| fi.same_as(seed)).unwrap_or_default()
    }

    fn scan_text(
        &self,
        field: &str,
        keep: &dyn Fn(&str) -> bool,
        _literals: &[String],
        restrict: Option<&RoaringBitmap>,
    ) -> RoaringBitmap {
        self.fields.get(field).map(|fi| fi.scan_text(keep, restrict)).unwrap_or_default()
    }

    fn scan_names(
        &self,
        field: &str,
        keep: &dyn Fn(&str) -> bool,
        _literals: &[String],
        restrict: Option<&RoaringBitmap>,
    ) -> RoaringBitmap {
        self.fields.get(field).map(|fi| fi.scan_names(keep, restrict)).unwrap_or_default()
    }

    fn follow(&self, field: &str) -> Option<Follow> {
        let fi = self.fields.get(field)?;
        Some(if fi.supports_transitive() {
            Follow::Tree
        } else if fi.supports_follows() {
            Follow::Direct
        } else {
            Follow::None
        })
    }

    fn referrers(&self, field: &str, target: Uuid) -> Option<Cow<'_, RoaringBitmap>> {
        self.fields.get(field)?.referrers_of(target).map(Cow::Borrowed)
    }

    fn tree_roots(&self, field: &str) -> RoaringBitmap {
        self.fields.get(field).and_then(|fi| fi.tree_roots()).unwrap_or_default()
    }

    fn tree_parents_except(&self, field: &str, except: Option<Uuid>) -> RoaringBitmap {
        self.fields.get(field).and_then(|fi| fi.tree_parents_except(except)).unwrap_or_default()
    }

    fn parents(&self, field: &str) -> Cow<'_, RoaringBitmap> {
        self.parents.get(field).map_or_else(|| Cow::Owned(RoaringBitmap::new()), Cow::Borrowed)
    }

    fn sort_reps(&self, field: &str, want_max: bool) -> RepReader<'_> {
        // A numeric or date value's representative is read from the
        // bit-slices, any other from the sort store (see `stores_sort_rep`).
        let fi = self.fields.get(field);
        let store = self.sort.get(field);
        Box::new(move |id| {
            fi.and_then(|fi| fi.bsi_sort_rep(id, want_max))
                .or_else(|| store.and_then(|s| s.rep(id, want_max)).cloned())
        })
    }

    fn bsi(&self, field: &str) -> Option<&FieldIndex> {
        // Only when the field holds no value the slices do not (mixed
        // historical data, which the sort store keeps).
        let mixed = self.sort.get(field).is_some_and(|s| !s.is_empty());
        self.fields.get(field).filter(|fi| fi.bsi_valued().is_some() && !mixed)
    }
}

// ── Pagination cursor ───────────────────────────────────────────────────────

/// A deterministic hash binding a cursor to its (query, sort) so a token from
/// one query cannot be replayed against another (matches the oracle).
fn page_guard(q: &Query, sort: &[SortBy]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    feed(format!("{q:?}").as_bytes());
    for key in sort {
        feed(key.field.as_bytes());
        feed(&[key.ascending as u8]);
    }
    h
}

/// Total order over [`SortEntry`]s: per key the representative compared in the
/// key's direction (`None`/field-absent last in both), then uuid ascending.
fn cmp_reps(
    a: &[Option<SortRep>],
    a_uuid: Uuid,
    b: &[Option<SortRep>],
    b_uuid: Uuid,
    sort: &[SortBy],
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    for (idx, key) in sort.iter().enumerate() {
        let ord = match (&a[idx], &b[idx]) {
            (Some(x), Some(y)) => {
                if key.ascending {
                    x.cmp(y)
                } else {
                    y.cmp(x)
                }
            }
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    a_uuid.cmp(&b_uuid)
}

/// Keyset cursor: the guard, then the last returned entry's sort key (one
/// representative per key) and uuid, so the next page resumes strictly after it.
fn encode_cursor(guard: u64, entry: &SortEntry) -> String {
    let mut bytes = Vec::with_capacity(32);
    bytes.extend_from_slice(&guard.to_le_bytes());
    bytes.extend_from_slice(&(entry.0.len() as u32).to_le_bytes());
    for rep in &entry.0 {
        match rep {
            None => bytes.push(0),
            Some(rep) => {
                bytes.push(1);
                encode_rep(&mut bytes, rep);
            }
        }
    }
    bytes.extend_from_slice(entry.1.as_bytes());
    base64::engine::general_purpose::STANDARD_NO_PAD.encode(bytes)
}

fn encode_rep(out: &mut Vec<u8>, rep: &SortRep) {
    let mut text = |tag: u8, s: &str| {
        out.push(tag);
        out.extend_from_slice(&(s.len() as u32).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    };
    match rep {
        SortRep::Bool(b) => out.extend_from_slice(&[0, *b as u8]),
        SortRep::Num(f) => {
            out.push(1);
            out.extend_from_slice(&f.to_bits().to_le_bytes());
        }
        SortRep::Str(s) => text(2, s),
        SortRep::DateTime(ms) => {
            out.push(3);
            out.extend_from_slice(&ms.to_le_bytes());
        }
        SortRep::Ref(bytes) => {
            out.push(4);
            out.extend_from_slice(bytes);
        }
        SortRep::Tree(s) => text(5, s),
    }
}

/// A cursor byte reader; every accessor is bounds-checked so a malformed or
/// truncated token decodes to `None` rather than panicking.
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let slice = self.bytes.get(self.pos..self.pos + n)?;
        self.pos += n;
        Some(slice)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
}

fn decode_rep(r: &mut Reader<'_>) -> Option<SortRep> {
    let text = |r: &mut Reader<'_>| -> Option<String> {
        let len = r.u32()? as usize;
        String::from_utf8(r.take(len)?.to_vec()).ok()
    };
    Some(match r.u8()? {
        0 => SortRep::Bool(r.u8()? != 0),
        1 => SortRep::Num(f64::from_bits(r.u64()?)),
        2 => SortRep::Str(text(r)?.into()),
        3 => SortRep::DateTime(r.u64()? as i64),
        4 => SortRep::Ref(r.take(16)?.try_into().ok()?),
        5 => SortRep::Tree(text(r)?.into()),
        _ => return None,
    })
}

fn decode_cursor(token: &str, expected_keys: usize) -> Option<(u64, SortEntry)> {
    let bytes = base64::engine::general_purpose::STANDARD_NO_PAD.decode(token).ok()?;
    let mut r = Reader { bytes: &bytes, pos: 0 };
    let guard = r.u64()?;
    let n = r.u32()? as usize;
    if n != expected_keys {
        return None;
    }
    let mut reps = Vec::with_capacity(n);
    for _ in 0..n {
        reps.push(match r.u8()? {
            0 => None,
            1 => Some(decode_rep(&mut r)?),
            _ => return None,
        });
    }
    let uuid = Uuid::from_slice(r.take(16)?).ok()?;
    if r.pos != bytes.len() {
        return None; // trailing garbage
    }
    Some((guard, (reps, uuid)))
}
