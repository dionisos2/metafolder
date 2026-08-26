//! In-memory bitmap/BSI query index (spec-indexing.org), increment 1.
//!
//! A *derived, read-only* accelerator built from the `field` table. It answers
//! a [`Query`] as a `RoaringBitmap` of dense metarecord ids and is validated
//! against the SQL engine ([`crate::query_exec`]) by an equivalence oracle
//! (`tests/index_oracle.rs`). It is built at repo load and refreshed to HEAD
//! per query (`run_query_filter`), which falls back to the SQL engine on any
//! `Unsupported` shape. Shapes the index cannot resolve on its own are handled
//! with caller-supplied seeds ([`QueryRoots`]): `Path`-target follows resolve to
//! a root metarecord through the tree cache, and a single-term `Osm` `Path`
//! resolves to its "term nodes" (name-substring matches) through FTS, which the
//! index then expands into a subtree union — the exact match set, no per-path
//! check. Remaining text leaves (`Matches`, `Osm` `Direct`, multi-term `Osm`
//! `Path`) are pre-resolved to `UuidIn` sets by the caller
//! (`query_exec::resolve_index_leaves`), so a query that merely *contains* one is
//! still served whole by the index. Not persisted — rebuilt each session.

pub mod field_index;
pub mod id_registry;

use std::collections::HashMap;

use base64::Engine;
use metafolder_core::metarecord::Value;
use metafolder_core::query::{FollowTarget, Query};
use roaring::RoaringBitmap;
use rusqlite::Connection;
use uuid::Uuid;

use crate::db;
use field_index::{CmpOp, FieldIndex, SortRep, SortReps};
use id_registry::IdRegistry;

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
/// map resolved to nothing and yields an empty result, matching the SQL engine.
pub type PathRoots = HashMap<(String, String), Uuid>;

/// Pre-resolved `(field, path)` → the metarecord at exactly that TreeRef path,
/// for the *exact-node* `Eq` operands of a query (spec-query "Exact-node
/// equality": a `/`-bearing string operand on a `tree_ref` field). Resolution
/// lives in the tree cache, so — as with [`PathRoots`] — the caller does it and
/// hands the node in. Unlike the other two maps the value is an `Option`: an
/// entry mapping to `None` says "the caller resolved this path and it is not a
/// node" (an empty result), while a *missing* entry says "nobody resolved it",
/// which keeps the operand `Unsupported` and defers to the SQL engine.
pub type NodeRoots = HashMap<(String, String), Option<Uuid>>;

/// The caller-resolved seeds a query needs the index to evaluate the shapes it
/// cannot resolve on its own — both are tree-cache lookups: `Path` targets and
/// exact-node `Eq`/`Neq` operands. Bundled so the evaluation threads one
/// context.
#[derive(Default)]
pub struct QueryRoots {
    pub path: PathRoots,
    pub node: NodeRoots,
}

impl QueryRoots {
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
        _ => {}
    }
}

/// Collects the `(field, path)` of every *exact-node* `Eq`/`Neq` operand in `q`
/// — a string operand containing the path separator, which on a `tree_ref` field
/// is a node match rather than a `value_name` compare (spec-query "Exact-node
/// equality"). The caller resolves each through the tree cache into
/// [`NodeRoots`]; the index then answers `mfr_path = "/a/b.txt"` from a single
/// interned id instead of deferring the whole query to a full SQL scan.
///
/// A `/`-bearing operand on a plain *string* field is ordinary literal equality
/// and is collected too — harmlessly, since it resolves to no node and the
/// index's type check never consults the entry.
pub fn collect_node_paths(q: &Query, out: &mut Vec<(String, String)>) {
    match q {
        Query::Eq { field, value: Value::String(s) }
        | Query::Neq { field, value: Value::String(s) } => {
            if s.contains('/') {
                out.push((field.clone(), s.clone()));
            }
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
        _ => {}
    }
}

/// Whether a `terms` list is the single term the index serves natively for
/// `Osm` `Path`: the union of the subtrees rooted at the nodes whose name
/// contains it *is* the match set, no ordered verification needed. Any length —
/// the name scan is in memory, so the FTS trigram's three-character floor no
/// longer applies. Several terms are order-sensitive and stay with the caller.
fn osm_path_indexable(terms: &[String]) -> Option<&str> {
    match terms {
        [only] => Some(only.as_str()),
        _ => None,
    }
}

/// Whether `q` contains an `Osm` `Path` the index serves natively. The caller
/// uses it to decide whether resolving the query's remaining text leaves is
/// worth it, so the choice stays a function of the query *shape* alone.
pub fn contains_index_served_osm_path(q: &Query) -> bool {
    match q {
        Query::Osm { terms, mode: metafolder_core::query::OsmMode::Path, .. } => {
            osm_path_indexable(terms).is_some()
        }
        Query::And { operands } | Query::Or { operands } => {
            operands.iter().any(contains_index_served_osm_path)
        }
        Query::Not { operand } => contains_index_served_osm_path(operand),
        Query::Follows { target, .. } | Query::FollowsTransitive { target, .. } => {
            matches!(target, FollowTarget::Condition(c) if contains_index_served_osm_path(c))
        }
        _ => false,
    }
}

/// A metarecord's position in a sort order: one representative per sort key
/// (`None` = the field is absent, which sorts last) plus the uuid tiebreak.
/// This is what a keyset cursor encodes, so pagination resumes *after* a known
/// position rather than at an absolute offset — stable under concurrent edits.
type SortEntry = (Vec<Option<SortRep>>, Uuid);

/// A query shape (or operand type) the bitmap path does not accelerate in this
/// increment (e.g. `Matches`, a `Path`-target `Follows`). The caller may fall
/// back to the SQL engine; the oracle battery simply excludes these shapes.
#[derive(Debug)]
pub struct Unsupported(pub String);

fn unsupported(what: impl Into<String>) -> Unsupported {
    Unsupported(what.into())
}

/// Whether a value lands in a BSI encoding (Int / Float / DateTime) — whose
/// sort representative is read from the bit-slices, so it is *not* mirrored in
/// the separate sort store.
fn is_bsi_value(value: &Value) -> bool {
    matches!(value, Value::Int(_) | Value::Float(_) | Value::DateTime(_))
}

pub struct RepoIndex {
    registry: IdRegistry,
    /// All interned ids — the exclusively-owned universe (`_repo`). Complement
    /// base for `Not` / `IsUnknown`.
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
    /// The log HEAD (`log_head.op_id`) this index reflects. The caller only
    /// uses the index while this matches the current HEAD, then rebuilds.
    built_at_head: Option<i64>,
}

impl RepoIndex {
    /// Builds the index from a single pass over the repository's field rows
    /// (every metarecord — one repository per database file).
    pub fn build(conn: &Connection) -> anyhow::Result<RepoIndex> {
        Self::build_reported(conn, &|_, _| {}, &|| false)
    }

    /// [`Self::build`] reporting progress as `(done, total)` metarecords scanned,
    /// for the load progress bar. `progress` is called every few thousand rows
    /// and once at completion. `cancel` is polled at the same cadence: when it
    /// returns true the build bails (spec-tasks "Cancellation"), so a query that
    /// triggered a rebuild can be stopped. Pass `&|| false` for uncancellable
    /// callers (the load warmup).
    pub fn build_reported(
        conn: &Connection,
        progress: &dyn Fn(u64, u64),
        cancel: &dyn Fn() -> bool,
    ) -> anyhow::Result<RepoIndex> {
        Self::build_inner(conn, None, progress, cancel)
    }

    /// [`Self::build_reported`] that also collects every TreeRef position it
    /// scans into `forest` (in `field.id` order), so the caller can populate the
    /// tree cache from the *same* single pass over the `field` table instead of a
    /// second full scan (`db::load_tree_forest`). See `RepoState::warmup`.
    pub fn build_reported_collecting(
        conn: &Connection,
        forest: &mut Vec<db::TreeRow>,
        progress: &dyn Fn(u64, u64),
        cancel: &dyn Fn() -> bool,
    ) -> anyhow::Result<RepoIndex> {
        Self::build_inner(conn, Some(forest), progress, cancel)
    }

    fn build_inner(
        conn: &Connection,
        mut forest: Option<&mut Vec<db::TreeRow>>,
        progress: &dyn Fn(u64, u64),
        cancel: &dyn Fn() -> bool,
    ) -> anyhow::Result<RepoIndex> {
        let built_at_head = db::current_head(conn)?;
        let mut registry = IdRegistry::new();
        let mut universe = RoaringBitmap::new();
        for uuid in db::list_entries(conn)? {
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
        let total = db::max_field_id(conn)?.max(0) as u64;
        if cancel() {
            anyhow::bail!("index build cancelled");
        }
        let mut scanned_rows: u64 = 0;
        db::for_each_field_row(conn, |uuid, row| {
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
            if let (Some(sink), Value::TreeRef { parent, name }) = (forest.as_deref_mut(), &row.value)
            {
                sink.push(db::TreeRow {
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
                    // BSI fields derive their sort representative from the
                    // bit-slices, so they skip the separate sort store.
                    if !is_bsi_value(&value) {
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
        progress(total, total);

        Ok(RepoIndex { registry, universe, present, absent, fields, sort, types, built_at_head })
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
    pub fn refresh(&mut self, conn: &Connection, cancel: &dyn Fn() -> bool) -> anyhow::Result<()> {
        let head = db::current_head(conn)?;
        if head == self.built_at_head {
            return Ok(());
        }
        let delta = match head {
            Some(current) => self.forward_delta(conn, current)?,
            None => None, // HEAD reset to empty: not a forward extension.
        };
        match delta {
            // Incremental, unless dead dense ids (deleted metarecords, never
            // reused) have piled up — a rebuild re-interns only the live set
            // and reclaims them.
            Some(delta) if !self.tombstones_heavy() => {
                self.apply_ops(conn, &delta)?;
                self.built_at_head = head;
            }
            _ => *self = Self::build_reported(conn, &|_, _| {}, cancel)?,
        }
        Ok(())
    }

    /// The operations strictly between `built_at_head` and `current_head` along
    /// the HEAD parent chain, oldest first — or `None` if `built_at_head` is not
    /// an ancestor of `current_head` (history was rewritten), an op type is not
    /// one we replay, or the delta is large enough that a rebuild is cheaper.
    ///
    /// The walk *stops at* `built_at_head` ([`crate::log::ancestry_ops_until`])
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
        conn: &Connection,
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

        let mut delta =
            match crate::log::ancestry_ops_until(conn, current_head, built_at_head, REBUILD_OVER)? {
                Some(ops) => ops,
                // Not on the chain (history was rewritten) or beyond the budget.
                None => return Ok(None),
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
    fn apply_ops(&mut self, conn: &Connection, delta: &[crate::log::OpRow]) -> anyhow::Result<()> {
        use std::collections::{HashMap, HashSet};

        let mut created: Vec<Uuid> = Vec::new();
        let mut deleted: HashSet<Uuid> = HashSet::new();
        let mut touched: HashMap<(Uuid, String), Vec<Value>> = HashMap::new();

        for op in delta {
            let before = crate::log::snapshots(conn, op.id, 0)?;
            match op.op_type.as_str() {
                "create_metarecord" => {
                    created.push(op.entity_uuid);
                    for row in crate::log::snapshots(conn, op.id, 1)? {
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
                    for row in crate::log::snapshots(conn, op.id, 1)? {
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
            let new_values: Vec<Value> = db::get_field_rows_named(conn, uuid, &field)?
                .into_iter()
                .map(|row| row.value)
                .collect();
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
            let is_bsi = {
                let enc = self
                    .fields
                    .entry(field.to_string())
                    .or_insert_with(|| FieldIndex::for_value(first));
                enc.set_member(id, &non_nothing);
                enc.is_bsi()
            };
            // BSI fields read their sort representative from the bit-slices,
            // so they keep no entry in the separate sort store.
            if !is_bsi {
                let sr = self.sort.entry(field.to_string()).or_default();
                for &v in &non_nothing {
                    sr.insert(v, id);
                }
            }
        }
    }

    /// Number of metarecords matching `q` — `O(1)` from the result bitmap,
    /// where the SQL `COUNT` is `O(n)` (the irreducible count wall).
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
    /// Reproduces the SQL sort semantics: per key the multi-map representative
    /// (min ascending / max descending), the fixed type-group precedence,
    /// metarecords lacking the field last, uuid tiebreak. The cursor is an
    /// opaque offset bound to a hash of (query, sort) — reused against a
    /// different query/sort it is rejected, matching the SQL engine.
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

    fn page(
        &self,
        q: &Query,
        sort: &[SortBy],
        limit: Option<usize>,
        cursor: Option<&str>,
        roots: Option<&QueryRoots>,
    ) -> Result<(Vec<Uuid>, Option<String>), Unsupported> {
        use std::cmp::Ordering;
        let guard = page_guard(q, sort);
        let after: Option<SortEntry> = match cursor {
            None => None,
            Some(c) => {
                let (g, entry) =
                    decode_cursor(c, sort.len()).ok_or_else(|| unsupported("malformed cursor"))?;
                if g != guard {
                    return Err(unsupported("cursor does not match this query and sort"));
                }
                Some(entry)
            }
        };

        let matched = self.eval(q, roots)?;
        let mut entries: Vec<SortEntry> = matched.iter().map(|id| self.entry_of(id, sort)).collect();
        // Keyset: keep only what sorts strictly after the cursor position.
        if let Some(after) = &after {
            entries.retain(|e| cmp_entry(e, after, sort) == Ordering::Greater);
        }

        let total = entries.len();
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
            entries.select_nth_unstable_by(end - 1, |a, b| cmp_entry(a, b, sort));
        }
        entries[..end].sort_by(|a, b| cmp_entry(a, b, sort));

        let page = entries[..end].iter().map(|e| e.1).collect();
        let next = match limit {
            Some(_) if end > 0 && end < total => Some(encode_cursor(guard, &entries[end - 1])),
            _ => None,
        };
        Ok((page, next))
    }

    /// A metarecord's [`SortEntry`] under `sort` (its representative per key +
    /// uuid). The representative is the max for a descending key, the min for an
    /// ascending one — the same one the SQL sort picks.
    fn entry_of(&self, id: u32, sort: &[SortBy]) -> SortEntry {
        let reps = sort
            .iter()
            .map(|k| {
                let want_max = !k.ascending;
                // A BSI field reads its representative from the bit-slices;
                // every other encoding uses the small sort store.
                self.fields
                    .get(&k.field)
                    .and_then(|fi| fi.bsi_sort_rep(id, want_max))
                    .or_else(|| self.sort.get(&k.field).and_then(|s| s.rep(id, want_max)).cloned())
            })
            .collect();
        (reps, self.registry.uuid(id).expect("interned id"))
    }

    /// Evaluates a query to the bitmap of matching dense ids (path targets
    /// unsupported; use [`Self::evaluate_page_with_roots`] for those).
    pub fn evaluate(&self, q: &Query) -> Result<RoaringBitmap, Unsupported> {
        self.eval(q, None)
    }

    /// Evaluates a query to the bitmap of matching dense ids. `roots` is
    /// `Some(map)` once the caller has resolved the query's `Path` targets (see
    /// [`PathRoots`]); `None` means they have not, so a `Path` target is
    /// reported `Unsupported` and the caller falls back to the SQL engine.
    fn eval(&self, q: &Query, roots: Option<&QueryRoots>) -> Result<RoaringBitmap, Unsupported> {
        match q {
            Query::IsPresent { field } => Ok(self.present_of(field)),
            Query::IsAbsent { field } => Ok(self.absent_of(field)),
            Query::IsUnknown { field } => {
                // universe − {records with any row of `field`} (present ∪ absent),
                // matching the SQL `_repo WHERE uuid NOT IN (any field row)`.
                let mut r = self.universe.clone();
                r -= &self.present_of(field);
                r -= &self.absent_of(field);
                Ok(r)
            }

            Query::Eq { field, value } => self.compare(field, CmpOp::Eq, value, roots),
            Query::Neq { field, value } => self.compare(field, CmpOp::Neq, value, roots),
            Query::Lt { field, value } => self.compare(field, CmpOp::Lt, value, roots),
            Query::Lte { field, value } => self.compare(field, CmpOp::Lte, value, roots),
            Query::Gt { field, value } => self.compare(field, CmpOp::Gt, value, roots),
            Query::Gte { field, value } => self.compare(field, CmpOp::Gte, value, roots),

            Query::And { operands } => self.combine(operands, true, roots),
            Query::Or { operands } => self.combine(operands, false, roots),
            Query::Not { operand } => {
                let mut r = self.universe.clone();
                r -= &self.eval(operand, roots)?;
                Ok(r)
            }

            Query::Follows { field, target } => self.follows(field, target, roots),
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
                    if let Some(id) = self.registry.id(*u) {
                        r.insert(id);
                    }
                }
                r &= &self.universe;
                Ok(r)
            }

            other => Err(unsupported(format!("{other:?}"))),
        }
    }

    /// Direct `Follows`: referrers of every metarecord matching the sub-query.
    /// Direct referrers of the target metarecords. A `Path` target is resolved
    /// through `roots` (the tree cache, upstream) to a single root metarecord;
    /// a `Condition` target is evaluated to its match set.
    /// The root metarecord a `Path` target resolves to, looked up in the
    /// caller-supplied `roots`. `None` roots means the caller did not resolve
    /// path targets, so this shape is `Unsupported` (fall back to SQL); a path
    /// absent from a supplied map resolved to nothing (`Ok(None)`, empty result).
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
                self.eval(cond, roots)?.iter().filter_map(|tid| self.registry.uuid(tid)).collect()
            }
        };
        let Some(fi) = self.fields.get(field) else { return Ok(RoaringBitmap::new()) };
        if !fi.supports_follows() {
            return Ok(RoaringBitmap::new());
        }
        let mut out = RoaringBitmap::new();
        for uuid in target_uuids {
            if let Some(referrers) = fi.referrers_of(uuid) {
                out |= referrers;
            }
        }
        Ok(out)
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
        // surfaces, matching the SQL fallback contract.)
        let frontier = match target {
            FollowTarget::Path(p) => match self.resolved_root(field, p, roots)? {
                Some(root) => match self.registry.id(root) {
                    Some(id) => RoaringBitmap::from_iter([id]),
                    None => return Ok(RoaringBitmap::new()), // root not in the index
                },
                None => return Ok(RoaringBitmap::new()), // path resolved to nothing
            },
            FollowTarget::Condition(cond) => self.eval(cond, roots)?,
        };
        let Some(fi) = self.fields.get(field) else { return Ok(RoaringBitmap::new()) };
        if !fi.supports_transitive() {
            return Ok(RoaringBitmap::new());
        }
        // The inclusive form (`=>*`) keeps the root(s) in the result (whole
        // subtree); the strict form (`->*`) grows only downward from them.
        Ok(self.expand_subtrees(fi, frontier, inclusive))
    }

    /// Grows `frontier` downward over the reverse (direct-children) index of
    /// `fi` until fixpoint, returning every reachable node. `inclusive` keeps the
    /// seed nodes themselves (whole subtree); otherwise only their descendants.
    /// The shared core of `FollowsTransitive` and single-term `Osm` `Path`.
    fn expand_subtrees(
        &self,
        fi: &FieldIndex,
        mut frontier: RoaringBitmap,
        inclusive: bool,
    ) -> RoaringBitmap {
        let mut result =
            if inclusive { &frontier & &self.universe } else { RoaringBitmap::new() };
        while !frontier.is_empty() {
            let mut next = RoaringBitmap::new();
            for nid in &frontier {
                if let Some(uuid) = self.registry.uuid(nid) {
                    if let Some(children) = fi.referrers_of(uuid) {
                        next |= children;
                    }
                }
            }
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
        // user error the SQL engine reports as a 400 with the "use osmd" hint
        // (spec-query). Defer to it rather than answering with an empty bitmap,
        // which would turn that mistake into a silent "no rows". A field with no
        // values at all is vacuously empty in both engines.
        if self.types.get(field).is_some_and(|t| *t != "tree_ref") {
            return Err(unsupported("osm path on a non-tree_ref field"));
        }
        // A blank query (the search box emptied) matches every metarecord with a
        // path in this forest — the SQL engine scans for `value_type='tree_ref'`,
        // which on a tree_ref field is exactly the `present` set.
        if terms.is_empty() {
            return Ok(self.present_of(field));
        }
        let Some(term) = osm_path_indexable(terms) else {
            return Err(unsupported("multi-term osm path"));
        };
        let Some(fi) = self.fields.get(field) else { return Ok(RoaringBitmap::new()) };
        if !fi.supports_transitive() {
            return Ok(RoaringBitmap::new());
        }
        // The "term nodes" — those whose *name* contains the term — resolved
        // from the in-memory name partition. The SQL engine finds them with
        // `value_name REGEXP '(?i)<escaped term>'`, so use that very regex on
        // each distinct name: same case folding, same escaping, no divergence.
        // It also works below the FTS trigram's three-character floor, which is
        // where the first keystrokes of a search used to fall off a cliff.
        let re = crate::regexp::compile(&format!("(?i){}", regex::escape(term)))
            .map_err(|e| unsupported(format!("osm term is not a usable pattern: {e}")))?;
        let seeds = fi.scan_names(&|name| re.is_match(name), None);
        Ok(self.expand_subtrees(fi, seeds, true))
    }

    fn combine(
        &self,
        operands: &[Query],
        is_and: bool,
        roots: Option<&QueryRoots>,
    ) -> Result<RoaringBitmap, Unsupported> {
        let mut it = operands.iter();
        let first = it.next().ok_or_else(|| unsupported("'and'/'or' need an operand"))?;
        let mut acc = self.eval(first, roots)?;
        for operand in it {
            let bm = self.eval(operand, roots)?;
            if is_and {
                acc &= &bm;
            } else {
                acc |= &bm;
            }
        }
        Ok(acc)
    }

    /// Dispatches a comparison to the field's encoding. A field with no
    /// non-`Nothing` rows has no encoding, so the comparison is empty — exactly
    /// the SQL result (the `value_type` filter excludes every `Nothing` row).
    fn compare(
        &self,
        field: &str,
        op: CmpOp,
        value: &Value,
        roots: Option<&QueryRoots>,
    ) -> Result<RoaringBitmap, Unsupported> {
        if matches!(value, Value::Nothing) {
            return Err(unsupported("comparison with 'nothing'"));
        }
        // Exact-node path (spec-query "Exact-node equality"): on a tree_ref field
        // an Eq/Neq string operand containing '/' is a path-resolved node match,
        // not a value_name compare. The resolution lives in the tree cache, not
        // the index, so `Eq` is served only from a caller-supplied [`NodeRoots`]
        // entry; without one — and for `Neq`, whose multi-map negation the
        // rewrite does not cover — defer to the SQL engine rather than answer
        // with the (wrong, value_name-based) bitmap. A string field keeps literal
        // equality (the index handles it).
        if matches!(op, CmpOp::Eq | CmpOp::Neq) {
            if let Value::String(s) = value {
                if s.contains('/') && self.types.get(field) == Some(&"tree_ref") {
                    // A missing entry means nobody resolved this path: defer.
                    let resolved =
                        roots.and_then(|r| r.node.get(&(field.to_string(), s.clone())));
                    let Some(node) = resolved else {
                        return Err(unsupported("exact-node tree_ref path equality"));
                    };
                    // The node itself, restricted to the metarecords that do
                    // carry a value for this field — the SQL match is on a
                    // `field_name` row of type tree_ref, so a node whose rows
                    // are all `Nothing` matches nothing there either. An entry
                    // mapping to `None` resolved to no node: no match at all.
                    let eq = match node {
                        Some(node) => match self.registry.id(*node) {
                            Some(id) => RoaringBitmap::from_iter([id]) & self.present_of(field),
                            None => RoaringBitmap::new(),
                        },
                        None => RoaringBitmap::new(),
                    };
                    if matches!(op, CmpOp::Eq) {
                        return Ok(eq);
                    }
                    // `Neq` is *not* the complement: SQL asks for ≥1 non-Nothing
                    // row that is not the `Eq` match, so a metarecord with no
                    // value for the field is in neither. On a tree_ref field
                    // that is every path-bearing metarecord but the node.
                    let mut out = self.present_of(field);
                    out -= &eq;
                    return Ok(out);
                }
            }
        }
        match self.fields.get(field) {
            Some(fi) => fi.compare(op, value),
            None => Ok(RoaringBitmap::new()),
        }
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

    fn present_of(&self, field: &str) -> RoaringBitmap {
        self.present.get(field).cloned().unwrap_or_default()
    }

    fn absent_of(&self, field: &str) -> RoaringBitmap {
        self.absent.get(field).cloned().unwrap_or_default()
    }
}

// ── Pagination cursor ───────────────────────────────────────────────────────

/// A deterministic hash binding a cursor to its (query, sort) so a token from
/// one query cannot be replayed against another (matches the SQL engine).
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
fn cmp_entry(a: &SortEntry, b: &SortEntry, sort: &[SortBy]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    for (idx, key) in sort.iter().enumerate() {
        let ord = match (&a.0[idx], &b.0[idx]) {
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
    a.1.cmp(&b.1)
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
        2 => SortRep::Str(text(r)?),
        3 => SortRep::DateTime(r.u64()? as i64),
        4 => SortRep::Ref(r.take(16)?.try_into().ok()?),
        5 => SortRep::Tree(text(r)?),
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
