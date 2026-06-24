//! In-memory bitmap/BSI query index (spec-indexing.org), increment 1.
//!
//! A *derived, read-only* accelerator built once from the `field` table. It
//! answers a [`Query`] as a `RoaringBitmap` of dense metarecord ids and is
//! validated against the SQL engine ([`crate::query_exec`]) by an equivalence
//! oracle (`tests/index_oracle.rs`). It is not yet wired into `RepoState` or
//! the live query route, is never mutated incrementally, and is not persisted —
//! those are later increments.

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
    /// Per field name: min/max sort representatives, for `ORDER BY`.
    sort: HashMap<String, SortReps>,
    /// The log HEAD (`log_head.op_id`) this index reflects. The caller only
    /// uses the index while this matches the current HEAD, then rebuilds.
    built_at_head: Option<i64>,
}

impl RepoIndex {
    /// Builds the index from a single pass over the universe's field rows.
    /// Link metarecords (shared ownership) are excluded by construction: only
    /// the exclusively-owned set (`db::list_entries`) is interned and scanned.
    pub fn build(conn: &Connection, db_id: Uuid) -> anyhow::Result<RepoIndex> {
        let built_at_head = db::current_head(conn)?;
        let mut registry = IdRegistry::new();
        let mut universe = RoaringBitmap::new();
        for uuid in db::list_entries(conn, db_id)? {
            universe.insert(registry.intern(uuid));
        }

        let mut present: HashMap<String, RoaringBitmap> = HashMap::new();
        let mut absent: HashMap<String, RoaringBitmap> = HashMap::new();
        let mut fields: HashMap<String, FieldIndex> = HashMap::new();
        let mut sort: HashMap<String, SortReps> = HashMap::new();
        for id in 0..registry.len() as u32 {
            let uuid = registry.uuid(id).expect("dense id in range");
            for row in db::get_field_rows(conn, uuid)? {
                match row.value {
                    Value::Nothing => {
                        absent.entry(row.name).or_default().insert(id);
                    }
                    value => {
                        present.entry(row.name.clone()).or_default().insert(id);
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
            }
        }
        for fi in fields.values_mut() {
            fi.finalize();
        }

        Ok(RepoIndex { registry, universe, present, absent, fields, sort, built_at_head })
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
    pub fn refresh(&mut self, conn: &Connection, db_id: Uuid) -> anyhow::Result<()> {
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
            _ => *self = Self::build(conn, db_id)?,
        }
        Ok(())
    }

    /// The operations strictly between `built_at_head` and `current_head` along
    /// the HEAD parent chain, oldest first — or `None` if `built_at_head` is not
    /// an ancestor of `current_head` (history was rewritten), an op type is not
    /// one we replay, or the delta is large enough that a rebuild is cheaper.
    fn forward_delta(
        &self,
        conn: &Connection,
        current_head: i64,
    ) -> anyhow::Result<Option<Vec<crate::log::OpRow>>> {
        const KNOWN: &[&str] = &[
            "create_metarecord",
            "delete_metarecord",
            "set_field",
            "append_field",
            "delete_field",
            "file_deleted",
            "file_moved",
            "file_modified",
        ];
        const REBUILD_OVER: usize = 20_000;

        let mut delta = Vec::new();
        let mut reached = false;
        for op in crate::log::ancestry_ops(conn, current_head)? {
            if Some(op.id) == self.built_at_head {
                reached = true;
                break;
            }
            if !KNOWN.contains(&op.op_type.as_str()) || delta.len() >= REBUILD_OVER {
                return Ok(None);
            }
            delta.push(op);
        }
        if !reached {
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
        Ok(self.evaluate(q)?.len())
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

        let matched = self.evaluate(q)?;
        let mut entries: Vec<SortEntry> = matched.iter().map(|id| self.entry_of(id, sort)).collect();
        // Keyset: keep only what sorts strictly after the cursor position.
        if let Some(after) = &after {
            entries.retain(|e| cmp_entry(e, after, sort) == Ordering::Greater);
        }
        entries.sort_by(|a, b| cmp_entry(a, b, sort));

        let total = entries.len();
        let end = match limit {
            Some(l) => l.min(total),
            None => total,
        };
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

    /// Evaluates a query to the bitmap of matching dense ids.
    pub fn evaluate(&self, q: &Query) -> Result<RoaringBitmap, Unsupported> {
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

            Query::Eq { field, value } => self.compare(field, CmpOp::Eq, value),
            Query::Neq { field, value } => self.compare(field, CmpOp::Neq, value),
            Query::Lt { field, value } => self.compare(field, CmpOp::Lt, value),
            Query::Lte { field, value } => self.compare(field, CmpOp::Lte, value),
            Query::Gt { field, value } => self.compare(field, CmpOp::Gt, value),
            Query::Gte { field, value } => self.compare(field, CmpOp::Gte, value),

            Query::And { operands } => self.combine(operands, true),
            Query::Or { operands } => self.combine(operands, false),
            Query::Not { operand } => {
                let mut r = self.universe.clone();
                r -= &self.evaluate(operand)?;
                Ok(r)
            }

            Query::Follows { field, target } => self.follows(field, target),
            Query::FollowsTransitive { field, target } => self.follows_transitive(field, target),

            other => Err(unsupported(format!("{other:?}"))),
        }
    }

    /// Direct `Follows`: referrers of every metarecord matching the sub-query.
    /// Path targets need the tree cache and are deferred (`Unsupported`).
    fn follows(&self, field: &str, target: &FollowTarget) -> Result<RoaringBitmap, Unsupported> {
        let FollowTarget::Condition(cond) = target else {
            return Err(unsupported("path-target follows"));
        };
        let Some(fi) = self.fields.get(field) else { return Ok(RoaringBitmap::new()) };
        if !fi.supports_follows() {
            return Ok(RoaringBitmap::new());
        }
        let targets = self.evaluate(cond)?;
        let mut out = RoaringBitmap::new();
        for tid in &targets {
            if let Some(uuid) = self.registry.uuid(tid) {
                if let Some(referrers) = fi.referrers_of(uuid) {
                    out |= referrers;
                }
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
    ) -> Result<RoaringBitmap, Unsupported> {
        let FollowTarget::Condition(cond) = target else {
            return Err(unsupported("path-target follows_transitive"));
        };
        let Some(fi) = self.fields.get(field) else { return Ok(RoaringBitmap::new()) };
        if !fi.supports_transitive() {
            return Ok(RoaringBitmap::new());
        }
        let mut result = RoaringBitmap::new();
        let mut frontier = self.evaluate(cond)?;
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
        Ok(result)
    }

    fn combine(&self, operands: &[Query], is_and: bool) -> Result<RoaringBitmap, Unsupported> {
        let mut it = operands.iter();
        let first = it.next().ok_or_else(|| unsupported("'and'/'or' need an operand"))?;
        let mut acc = self.evaluate(first)?;
        for operand in it {
            let bm = self.evaluate(operand)?;
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
    fn compare(&self, field: &str, op: CmpOp, value: &Value) -> Result<RoaringBitmap, Unsupported> {
        if matches!(value, Value::Nothing) {
            return Err(unsupported("comparison with 'nothing'"));
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
        Some(String::from_utf8(r.take(len)?.to_vec()).ok()?)
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
