//! The key-value store as a query [`Source`] (doc "Storage"):
//! every answer read from the derived key
//! spaces inside one read transaction, so a query sees one snapshot of the
//! repository, whatever is committed meanwhile.
//!
//! A field's values are, at bottom, a partition "key → ids", and each
//! comparison a union of the buckets whose key satisfies it — one key, all but
//! one, a range. The partitions are ordered key spaces ([`super::derived`]) and
//! a comparison is a range read. An operand of another type than the field's
//! is refused before evaluation (doc "Comparisons"); the type tag leading
//! every value key keeps one that reached here from matching any key.
//!
//! A read error cannot surface through the trait, whose answers are bitmaps:
//! the first one is kept, answers go empty from there, and
//! [`KvSource::take_error`] hands it to the caller, which fails the query.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

use anyhow::Result;
use heed::{RoTxn, WithoutTls};
use metafolder_core::metarecord::Value;
use roaring::{MultiOps, RoaringBitmap};
use uuid::Uuid;

use super::derived::index_writes::{self, IndexWrite};
use super::derived::{
    self, dense, hist_bucket, hist_key, id_of, long_prefix, read_set, text_key, type_tag,
    value_key, IndexSet, NAME, VALUE,
};
use super::{mass, name_key, uuid_of, KvStore, Tables};
use crate::index::keys::{sort_rep, CmpOp, SortRep};
use crate::index::plan;
use crate::index::{unsupported, Follow, NamedNode, RepReader, Source, Unsupported, Walked};

/// A read snapshot of a KV store, answering the evaluator's questions.
pub struct KvSource<'s> {
    t: Tables,
    r: RoTxn<'s, WithoutTls>,
    error: RefCell<Option<anyhow::Error>>,
    /// Keys read so far — what the cost assertions count
    /// (`tests/kv_cost.rs`): a bounded query must read a bounded number.
    reads: Cell<u64>,
    /// The store's own counter, which the snapshot's reads add to.
    store_reads: &'s std::sync::atomic::AtomicU64,
    /// The universe, read once: every answer is sifted through it
    /// ([`KvSource::sift`]).
    live: std::cell::OnceCell<RoaringBitmap>,
    /// The universe's chunks read one at a time, before anything needed it
    /// whole ([`KvSource::is_live`]).
    live_chunks: RefCell<BTreeMap<u16, RoaringBitmap>>,
    /// Each field's pending index writes, read once (doc "Mass writes in the
    /// store"): what is stored of the field's values is behind by them.
    index_writes: RefCell<BTreeMap<String, std::rc::Rc<Vec<IndexWrite>>>>,
    /// Whether any part has entries pending, which a record is read with
    /// applied ([`mass`]).
    writes_pending: bool,
}

impl KvStore {
    /// A query source over the store as committed now.
    pub fn source(&self) -> Result<KvSource<'_>> {
        Ok(KvSource {
            t: self.t,
            r: self.env.read_txn()?,
            error: RefCell::new(None),
            reads: Cell::new(0),
            store_reads: &self.reads,
            live: std::cell::OnceCell::new(),
            live_chunks: RefCell::new(BTreeMap::new()),
            index_writes: RefCell::new(BTreeMap::new()),
            writes_pending: self.has_pending_writes(),
        })
    }
}

/// How many rows of a field a comparison holds, as its histogram bounds it
/// ([`KvSource::range_rows`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RangeRows {
    /// The rows of the buckets wholly inside the comparison.
    pub at_least: u64,
    /// Those and every row of the bucket the operand falls in.
    pub at_most: u64,
}

/// How a field's values compare, by value type.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// `bool` / `string`.
    Categorical,
    /// `int` / `float`, compared together as numbers.
    Numeric,
    Datetime,
    /// A reference; `tree` for a `tree_ref` field.
    Reference {
        tree: bool,
    },
}

/// The largest id suffix, for an inclusive bound on a key.
const ID_MAX: [u8; 5] = [0xFF; 5];

impl KvSource<'_> {
    /// The ids of the metarecords that exist. A deleted metarecord leaves
    /// the universe and nothing else: its id stays in every other bitmap and
    /// partition until a reindex (doc "Deleting a metarecord in the store"),
    /// so an answer is what the indexes say *of the universe*.
    fn live(&self) -> &RoaringBitmap {
        self.live.get_or_init(|| self.live_set())
    }

    /// `ids` without those of deleted metarecords.
    fn sift(&self, mut ids: RoaringBitmap) -> RoaringBitmap {
        ids &= self.live();
        ids
    }

    /// Whether the metarecord of `id` exists: one chunk of the universe read
    /// (and kept), unless the whole is at hand.
    fn is_live(&self, id: u32) -> bool {
        if let Some(live) = self.live.get() {
            return live.contains(id);
        }
        let chunk = (id >> 16) as u16;
        let mut seen = self.live_chunks.borrow_mut();
        if let std::collections::btree_map::Entry::Vacant(slot) = seen.entry(chunk) {
            let read = || -> Result<RoaringBitmap> {
                self.read_keys(1)?;
                Ok(match self.t.live_metarecords.get(&self.r, &chunk.to_be_bytes())? {
                    Some(bytes) => derived::decode_set(bytes)?,
                    None => RoaringBitmap::new(),
                })
            };
            slot.insert(self.ok(read(), RoaringBitmap::new()));
        }
        seen[&chunk].contains(id)
    }

    /// The distinct `(field, value type)` pairs holding a value, by name,
    /// optionally of one type — `GET /repos/:repo/fields`.
    pub fn field_catalog(&self, type_filter: Option<&str>) -> Vec<(String, String)> {
        let read = || -> Result<Vec<(String, String)>> {
            let mut out = Vec::new();
            for entry in self.t.field_types.iter(&self.r)? {
                let (k, _) = entry?;
                let (name, ty) = derived::unesc(k)?;
                let ty = super::codec::field_type(ty)?.to_string();
                if type_filter.is_none_or(|want| want == ty) {
                    out.push((String::from_utf8(name)?, ty));
                }
            }
            out.sort();
            Ok(out)
        };
        self.ok(read(), Vec::new())
    }

    /// The representative of an id that holds `key` in a value partition and
    /// sorts on it, read from the id's rows: for a key that does not hold its
    /// whole text.
    fn rep_at(&self, id: u32, field: &str, key: &[u8]) -> Option<SortRep> {
        self.values(id, field)
            .into_iter()
            .find(|v| value_key(v).as_deref() == Some(key))
            .and_then(|v| sort_rep(&v))
    }

    /// [`Source::walk_values`] over the value partition: each entry is an id
    /// holding a value, handed over as it comes — the representative is the
    /// key's own value, no row is read. A run of long texts sharing their
    /// first bytes — whose keys sort by hash, and hold only those bytes — is
    /// read whole and put in order of the values. A value with a posting is
    /// not read holder by holder: its holders are handed over as one run, and
    /// the walk resumes past it.
    ///
    /// With index writes pending on the field (doc "Mass writes in the store"),
    /// what is stored is behind by them. The values the index writes name are
    /// stepped over where they are stored and handed over at their place in
    /// the order, their holders replayed — one that is not stored yet
    /// included; any other value has only lost the records a replacement was
    /// written on.
    fn walk(
        &self,
        field: &str,
        want_max: bool,
        start: Option<&SortRep>,
        start_key: Option<&[u8]>,
        visit: &mut dyn FnMut(&SortRep, Walked<'_>) -> bool,
    ) -> Result<()> {
        // A value is its posting: a key per chunk of its holders, the keys of
        // one value side by side.
        let prefix = name_key(field);
        // A hashed start may sit anywhere in its run: start at the run.
        let start_key: Option<Vec<u8>> = start_key.map(|k| match hashed_run(k) {
            Some(run) => run.to_vec(),
            None => k.to_vec(),
        });
        // The walk's far bound.
        let bound: Vec<u8> = if want_max {
            match &start_key {
                Some(k) => [&prefix[..], k, &[0xFF; 16][..]].concat(),
                None => [&prefix[..], &[0xFF; 2][..]].concat(),
            }
        } else {
            match &start_key {
                Some(k) => [&prefix[..], k].concat(),
                None => prefix.clone(),
            }
        };
        // Before the start in the walk's direction: already paged.
        let before_start =
            |rep: &SortRep| start.is_some_and(|s| if want_max { rep > s } else { rep < s });
        let pending = self.index_writes(field);
        let replaced = index_writes::replaced(&pending);
        // The values the index writes name, in the walk's order, each handed over
        // once: when the walk reaches its place.
        let named = index_writes::named(&pending);
        let mut ahead: Vec<&Vec<u8>> = named.iter().collect();
        if !want_max {
            ahead.reverse();
        }
        let mut named_upto =
            |upto: Option<&[u8]>, visit: &mut dyn FnMut(&SortRep, Walked<'_>) -> bool| -> bool {
                while let Some(key) = ahead.last().copied() {
                    let reached = match upto {
                        None => true,
                        Some(at) if want_max => key.as_slice() >= at,
                        Some(at) => key.as_slice() <= at,
                    };
                    if !reached {
                        break;
                    }
                    ahead.pop();
                    let Some(rep) = key_rep(key).filter(|rep| !before_start(rep)) else { continue };
                    let holders = self.bucket(field, VALUE, key);
                    if !holders.is_empty() && !visit(&rep, Walked::Run(&holders)) {
                        return false;
                    }
                }
                true
            };
        let mut run: Vec<(SortRep, u32)> = Vec::new();
        let mut run_key: Option<Vec<u8>> = None;
        let emit_run =
            |run: &mut Vec<(SortRep, u32)>, visit: &mut dyn FnMut(&SortRep, Walked<'_>) -> bool| {
                run.sort_by(|a, b| if want_max { b.0.cmp(&a.0) } else { a.0.cmp(&b.0) });
                run.drain(..).all(|(rep, id)| visit(&rep, Walked::One(id)))
            };
        let entries: Entries<'_> = if want_max {
            let range = (Bound::Included(prefix.as_slice()), Bound::Excluded(bound.as_slice()));
            Box::new(derived::Bitmaps::ValueHolders.db(&self.t).rev_range(&self.r, &range)?)
        } else {
            let range = (Bound::Included(bound.as_slice()), Bound::Unbounded);
            Box::new(derived::Bitmaps::ValueHolders.db(&self.t).range(&self.r, &range)?)
        };
        // One value at a time: its chunks come one after the other.
        let mut value: Option<(Vec<u8>, RoaringBitmap)> = None;
        let mut hand = |key: &[u8],
                        holders: RoaringBitmap,
                        run: &mut Vec<(SortRep, u32)>,
                        run_key: &mut Option<Vec<u8>>,
                        visit: &mut dyn FnMut(&SortRep, Walked<'_>) -> bool|
         -> bool {
            if let Some(r) = hashed_run(key) {
                if run_key.as_deref() != Some(r) {
                    if !emit_run(run, visit) || !named_upto(Some(key), visit) {
                        return false;
                    }
                    *run_key = Some(r.to_vec());
                }
                // Its rows say what a replaced record still holds.
                for id in holders {
                    if let Some(rep) = self.rep_at(id, field, key).filter(|r| !before_start(r)) {
                        run.push((rep, id));
                    }
                }
                return true;
            }
            if !emit_run(run, visit) || !named_upto(Some(key), visit) {
                return false;
            }
            *run_key = None;
            // A value the index writes name was handed over just now, replayed.
            if named.contains(key) {
                return true;
            }
            let Some(rep) = key_rep(key) else { return true };
            let holders = holders - &replaced;
            before_start(&rep) || holders.is_empty() || visit(&rep, Walked::Run(&holders))
        };
        for entry in entries {
            let (k, v) = entry?;
            self.read_keys(1)?;
            if !k.starts_with(&prefix) || k.len() < prefix.len() + 2 {
                break;
            }
            let key = &k[prefix.len()..k.len() - 2];
            let chunk = derived::decode_set(v)?;
            match &mut value {
                Some((held, holders)) if held == key => *holders |= chunk,
                _ => {
                    if let Some((held, holders)) = value.take() {
                        if !hand(&held, holders, &mut run, &mut run_key, visit) {
                            return Ok(());
                        }
                    }
                    value = Some((key.to_vec(), chunk));
                }
            }
        }
        if let Some((held, holders)) = value.take() {
            if !hand(&held, holders, &mut run, &mut run_key, visit) {
                return Ok(());
            }
        }
        if emit_run(&mut run, visit) {
            named_upto(None, visit);
        }
        Ok(())
    }

    /// How many keys this snapshot has read (entries iterated, lookups made).
    pub fn reads(&self) -> u64 {
        self.reads.get()
    }

    /// `field`'s pending index writes, oldest first; a read each, once.
    fn index_writes(&self, field: &str) -> std::rc::Rc<Vec<IndexWrite>> {
        if let Some(known) = self.index_writes.borrow().get(field) {
            return known.clone();
        }
        let read = || -> Result<Vec<IndexWrite>> {
            let found = index_writes::read_index_writes(&self.t, &self.r, field)?;
            self.read_keys(1 + found.len() as u64)?;
            Ok(found)
        };
        let found = std::rc::Rc::new(self.ok(read(), Vec::new()));
        self.index_writes.borrow_mut().insert(field.to_string(), found.clone());
        found
    }

    /// What `grams` say of `field`'s texts (doc "Text search in the store"),
    /// a key or a few per trigram: the texts listed under every trigram kept
    /// by value — a list holds at most [`derived::GRAM_TEXTS_MAX`] texts, so
    /// this side is short — and the ids under every trigram kept by
    /// metarecord, within `restrict`. One no text holds empties the ids side.
    fn gram_lookup(
        &self,
        field: &str,
        grams: &BTreeSet<[u8; 3]>,
        restrict: Option<&RoaringBitmap>,
    ) -> GramLookup {
        let read = || -> Result<GramLookup> {
            let mut texts: Option<Vec<derived::GramText>> = None;
            let mut ids: Option<RoaringBitmap> = None;
            for gram in grams {
                let prefix = derived::gram_prefix(field, gram);
                self.read_keys(1)?;
                let list = match self.t.index_trigram_texts.prefix_iter(&self.r, &prefix)?.next() {
                    Some(entry) => {
                        let (k, v) = entry?;
                        Some((k[prefix.len()..].first().copied().unwrap_or(0xFF), v))
                    }
                    None => None,
                };
                match list {
                    Some((derived::GRAM_BY_VALUE, v)) => {
                        let list = derived::decode_gram_texts(v)?;
                        texts = Some(match texts {
                            None => list,
                            Some(prev) => {
                                prev.into_iter().filter(|t| list.binary_search(t).is_ok()).collect()
                            }
                        });
                    }
                    Some((mode, _)) => anyhow::bail!("a trigram list of mode {mode}"),
                    None => {
                        if ids.as_ref().is_some_and(RoaringBitmap::is_empty) {
                            continue;
                        }
                        let held = self.gram_ids_within(&prefix, restrict)?;
                        ids = Some(match ids {
                            None => held,
                            Some(prev) => prev & held,
                        });
                    }
                }
            }
            Ok(GramLookup { texts, ids })
        };
        self.ok(read(), GramLookup { texts: None, ids: None })
    }

    /// A trigram's bitmap of ids (its key `prefix`, without the chunk): its
    /// chunks, only those `restrict` has ids in when given; a read per chunk.
    fn gram_ids_within(
        &self,
        prefix: &[u8],
        restrict: Option<&RoaringBitmap>,
    ) -> Result<RoaringBitmap> {
        let mut out = RoaringBitmap::new();
        match restrict {
            Some(r) => {
                let chunks: BTreeSet<u16> = r.iter().map(|id| (id >> 16) as u16).collect();
                for chunk in chunks {
                    self.read_keys(1)?;
                    let k = [prefix, &chunk.to_be_bytes()].concat();
                    if let Some(v) = self.t.index_trigram_holders.get(&self.r, &k)? {
                        derived::add_encoded(&mut out, v)?;
                    }
                }
                out &= r;
            }
            None => {
                for entry in self.t.index_trigram_holders.prefix_iter(&self.r, prefix)? {
                    let (k, v) = entry?;
                    self.read_keys(1)?;
                    if k.len() == prefix.len() + 2 {
                        derived::add_encoded(&mut out, v)?;
                    }
                }
            }
        }
        Ok(out)
    }

    /// The ids pointing at `target` in `field`: a folder's children, its
    /// parent holders — one bitmap, a read per chunk; a `ref`'s referrers,
    /// the holders of its value (doc "Store tables").
    fn referrers_of(&self, field: &str, target: &[u8; 16]) -> RoaringBitmap {
        if self.value_type(field) == Some("ref") {
            let value = Value::Ref(Uuid::from_bytes(*target));
            let key = value_key(&value).expect("a reference has a value key");
            return self.bucket(field, VALUE, &key);
        }
        let read = || -> Result<RoaringBitmap> {
            // Filed under the target's dense id; nothing points at a uuid
            // that has none.
            let target = if *target == [0; 16] {
                derived::NO_TARGET
            } else {
                self.read_keys(1)?;
                match self.t.ids.get(&self.r, target)? {
                    Some(id) => dense(id),
                    None => return Ok(RoaringBitmap::new()),
                }
            };
            let prefix = derived::parent_holders_prefix(field, target);
            let mut out = RoaringBitmap::new();
            for entry in self.t.index_parent_holders.prefix_iter(&self.r, &prefix)? {
                let (k, v) = entry?;
                self.read_keys(1)?;
                if k.len() == prefix.len() + 2 {
                    derived::add_encoded(&mut out, v)?;
                }
            }
            Ok(out)
        };
        self.ok(read(), RoaringBitmap::new())
    }

    /// How many rows of `field` the comparison `op value` holds, from the
    /// field's histogram (doc "Query planner"): exact but for the
    /// operand's bucket, whose rows may fall either side. Reads the buckets
    /// on the comparison's side of the operand, not the rows. `None` for an
    /// operand of a type that has no histogram (a text, a bool, a
    /// reference). The operand is of the field's type: another is refused
    /// before evaluation (doc "Comparisons").
    pub fn range_rows(&self, field: &str, op: CmpOp, value: &Value) -> Option<RangeRows> {
        self.range_counted(field, op, value, self.t.index_histogram_rows)
    }

    /// [`Self::range_rows`] for the keys a walk of the range reads: the
    /// histogram counts the posting keys of each bucket, a key per chunk of
    /// each value.
    fn range_chunks(&self, field: &str, op: CmpOp, value: &Value) -> Option<RangeRows> {
        self.range_counted(field, op, value, self.t.index_histogram_keys)
    }

    /// What the buckets on the comparison's side of the operand count in the
    /// histogram table `db`.
    fn range_counted(
        &self,
        field: &str,
        op: CmpOp,
        value: &Value,
        db: super::table::Db,
    ) -> Option<RangeRows> {
        let key_of = hist_key;
        let operand = hist_bucket(&value_key(value)?)?;
        let tag = operand[0];
        let region = key_of(field, &[tag]);
        let end = key_of(field, &[tag + 1]);
        let at = key_of(field, &operand);
        let range: (Bound<&[u8]>, Bound<&[u8]>) = match op {
            CmpOp::Eq => (Bound::Included(&at), Bound::Included(&at)),
            CmpOp::Neq => (Bound::Included(&region), Bound::Excluded(&end)),
            CmpOp::Lt | CmpOp::Lte => (Bound::Included(&region), Bound::Included(&at)),
            CmpOp::Gt | CmpOp::Gte => (Bound::Included(&at), Bound::Excluded(&end)),
        };
        let read = || -> Result<RangeRows> {
            let (mut sure, mut edge, mut keys) = (0, 0, 0);
            for entry in db.range(&self.r, &range)? {
                let (k, v) = entry?;
                self.read_keys(1)?;
                keys += 1;
                let rows = super::from_be(v) as u64;
                let bucket = &k[region.len() - 1..];
                match bucket.cmp(&operand[..]) {
                    std::cmp::Ordering::Equal => edge += rows,
                    ord if op.matches_ordering(ord) => sure += rows,
                    _ => {}
                }
            }
            if keys == 0 {
                // The seek that found nothing.
                self.read_keys(1)?;
            }
            Ok(RangeRows { at_least: sure, at_most: sure + edge })
        };
        let estimate = read();
        self.ok(estimate.map(Some), None)
    }

    /// Whether a comparison checks the `candidates` on their own rows rather
    /// than walk its range (`plan::probes`; doc "Query planner"). The
    /// walk reads the field's holders for a `!=` or a text range; an ordered
    /// comparison of a number or a date reads the rows its histogram gives
    /// the range, at most, but a value with a posting as its posting. Each
    /// count is read only while the choice is still open, and none for no
    /// candidate, which costs nothing to probe.
    fn probes(&self, candidates: &RoaringBitmap, field: &str, op: CmpOp, value: &Value) -> bool {
        let n = candidates.len();
        if n == 0 || !plan::probes(n, self.present(field).len()) {
            return n == 0;
        }
        if op == CmpOp::Neq {
            return true;
        }
        match self.range_rows(field, op, value) {
            Some(e) if plan::probes(n, e.at_most) => {
                // The keys walking the range reads, which the rows bound.
                let keys = self.range_chunks(field, op, value).map_or(e.at_most, |k| k.at_most);
                plan::probes(n, keys.min(e.at_most))
            }
            Some(_) => false,
            None => true,
        }
    }

    /// A whole set; a read per chunk of 65 536 ids it spans.
    /// Every live metarecord: `live_metarecords` whole.
    fn live_set(&self) -> RoaringBitmap {
        let bm = self.ok(derived::read_live(&self.t, &self.r), RoaringBitmap::new());
        if !self.counted(1 + bm.max().map_or(0, |m| u64::from(m >> 16))) {
            return RoaringBitmap::new();
        }
        bm
    }

    fn set(&self, set: IndexSet, field: Option<&str>) -> RoaringBitmap {
        let bm = self.ok(read_set(&self.t, &self.r, set, field), RoaringBitmap::new());
        if !self.counted(1 + bm.max().map_or(0, |m| u64::from(m >> 16))) {
            return RoaringBitmap::new();
        }
        bm
    }

    /// Counts `n` keys read; fails once the running query was asked to stop
    /// (`crate::interrupt`), which is what stops each loop of the source.
    fn read_keys(&self, n: u64) -> Result<()> {
        // Before counting: a read refused is not a read.
        crate::interrupt::check(n)?;
        self.reads.set(self.reads.get() + n);
        metafolder_core::slowlog::count_reads(n);
        self.store_reads.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// [`Self::read_keys`] where no `Result` can carry its error: keeps it,
    /// and says whether the read may go on.
    fn counted(&self, n: u64) -> bool {
        self.ok(self.read_keys(n).map(|()| true), false)
    }

    /// The first read error met, if any (and forgets it).
    pub fn take_error(&self) -> Option<anyhow::Error> {
        self.error.borrow_mut().take()
    }

    /// `r`'s value, or `default` after keeping its error.
    fn ok<T>(&self, r: Result<T>, default: T) -> T {
        r.unwrap_or_else(|e| {
            self.error.borrow_mut().get_or_insert(e);
            default
        })
    }

    fn kind(&self, field: &str) -> Option<Kind> {
        Some(match self.value_type(field)? {
            "bool" | "string" => Kind::Categorical,
            "int" | "float" => Kind::Numeric,
            "datetime" => Kind::Datetime,
            "tree_ref" | "hierarchy_ref" => Kind::Reference { tree: true },
            _ => Kind::Reference { tree: false },
        })
    }

    /// The ids of the entries of a partition between two bounds on the key
    /// that follows the partition's prefix, keeping those whose key `keep`
    /// accepts.
    ///
    /// In the value partition, a value with a posting is not read holder by
    /// holder: its posting is taken whole (when `keep` accepts it) and the
    /// range resumes past its run — so a range over a few frequent values
    /// costs a few postings, not the rows they hold.
    fn part_ids(
        &self,
        field: &str,
        part: u8,
        from: Bound<&[u8]>,
        to: Bound<&[u8]>,
        keep: &dyn Fn(&[u8]) -> bool,
    ) -> RoaringBitmap {
        self.scan_part(field, part, from, to, keep)
    }

    /// [`Self::part_ids`], with the field's pending index writes replayed, in the value partition (doc "Mass
    /// writes in the store"). The values an index write names are left out of the
    /// scan of what is stored and read one by one, replayed; any other value
    /// has only lost the records a replacement was written on.
    fn scan_part(
        &self,
        field: &str,
        part: u8,
        from: Bound<&[u8]>,
        to: Bound<&[u8]>,
        keep: &dyn Fn(&[u8]) -> bool,
    ) -> RoaringBitmap {
        let pending = if part == VALUE { self.index_writes(field) } else { Default::default() };
        if pending.is_empty() {
            return self.scan_stored(field, part, from, to, keep);
        }
        let named = index_writes::named(&pending);
        let unnamed = |k: &[u8]| !named.contains(k) && keep(k);
        let mut out = self.scan_stored(field, part, from, to, &unnamed);
        out -= index_writes::replaced(&pending);
        // A key is in the range when its entries are: a key, then an id.
        let within = |k: &[u8]| {
            let (first, last) = ([k, &[0; 4][..]].concat(), [k, &ID_MAX[..]].concat());
            let above = match from {
                Bound::Included(a) => last.as_slice() >= a,
                Bound::Excluded(a) => last.as_slice() > a,
                Bound::Unbounded => true,
            };
            let below = match to {
                Bound::Included(b) => first.as_slice() <= b,
                Bound::Excluded(b) => first.as_slice() < b,
                Bound::Unbounded => true,
            };
            above && below
        };
        for key in named.iter().filter(|k| within(k) && keep(k)) {
            out |= index_writes::replay(&pending, key, self.bucket_stored(field, part, key));
        }
        out
    }

    /// [`Self::scan_part`] over what is stored: the postings of the keys
    /// `keep` accepts, in range, each taken whole — a key per chunk.
    fn scan_stored(
        &self,
        field: &str,
        part: u8,
        from: Bound<&[u8]>,
        to: Bound<&[u8]>,
        keep: &dyn Fn(&[u8]) -> bool,
    ) -> RoaringBitmap {
        let prefix = derived::part_posting_prefix(field, part, &[]);
        let with = |k: &[u8]| [&prefix[..], k].concat();
        let lo = match from {
            Bound::Included(k) => Bound::Included(with(k)),
            Bound::Excluded(k) => Bound::Excluded(with(k)),
            Bound::Unbounded => Bound::Included(prefix.clone()),
        };
        let hi = match to {
            Bound::Included(k) => Bound::Included(with(k)),
            Bound::Excluded(k) => Bound::Excluded(with(k)),
            Bound::Unbounded => Bound::Unbounded,
        };
        let read = || -> Result<RoaringBitmap> {
            let mut out = RoaringBitmap::new();
            let range = (lo.as_ref().map(|k| k.as_slice()), hi.as_ref().map(|k| k.as_slice()));
            for entry in self.t.index_value_holders.range(&self.r, &range)? {
                let (k, v) = entry?;
                self.read_keys(1)?;
                if !k.starts_with(&prefix) || k.len() < prefix.len() + 2 {
                    break;
                }
                let key = &k[prefix.len()..k.len() - 2];
                // `nothing` is no value a comparison or a scan reads.
                if key != [derived::NOTHING_TAG] && keep(key) {
                    derived::add_encoded(&mut out, v)?;
                }
            }
            Ok(out)
        };
        self.ok(read(), RoaringBitmap::new())
    }

    /// The posting of a value key in `field`; `None` for a value without one.
    fn posting(&self, field: &str, part: u8, key: &[u8]) -> Option<RoaringBitmap> {
        let read = || -> Result<Option<RoaringBitmap>> {
            let prefix = derived::part_posting_prefix(field, part, key);
            let mut out = None;
            for entry in self.t.index_value_holders.prefix_iter(&self.r, &prefix)? {
                let (k, v) = entry?;
                self.read_keys(1)?;
                if k.len() == prefix.len() + 2 {
                    derived::add_set(
                        out.get_or_insert_with(RoaringBitmap::new),
                        derived::decode_set(v)?,
                    );
                }
            }
            Ok(out)
        };
        self.ok(read(), None)
    }

    /// The ids holding exactly `key` in a partition: what is stored, with the
    /// field's pending index writes replayed.
    fn bucket(&self, field: &str, part: u8, key: &[u8]) -> RoaringBitmap {
        let stored = self.bucket_stored(field, part, key);
        let pending = if part == VALUE { self.index_writes(field) } else { Default::default() };
        if pending.is_empty() {
            return stored;
        }
        index_writes::replay(&pending, key, stored)
    }

    /// The ids stored under exactly `key` in a partition — its posting.
    fn bucket_stored(&self, field: &str, part: u8, key: &[u8]) -> RoaringBitmap {
        self.posting(field, part, key).unwrap_or_default()
    }

    /// The ids holding a key other than `key` in a partition.
    fn all_but(&self, field: &str, part: u8, key: Option<&[u8]>) -> RoaringBitmap {
        self.part_ids(field, part, Bound::Unbounded, Bound::Unbounded, &|k| Some(k) != key)
    }

    /// The ids holding a key `op` `key`, among the keys starting with `region`
    /// (a type tag: the operand's own type).
    fn ordered(
        &self,
        field: &str,
        part: u8,
        region: &[u8],
        op: CmpOp,
        key: &[u8],
        exclude: Option<&[u8]>,
    ) -> RoaringBitmap {
        let after = [key, &ID_MAX[..]].concat();
        let end: Vec<u8> = match region.split_last() {
            Some((last, head)) => [head, &[last + 1]].concat(),
            None => Vec::new(),
        };
        let (from, to) = match op {
            CmpOp::Lt => (Bound::Included(region), Bound::Excluded(key)),
            CmpOp::Lte => (Bound::Included(region), Bound::Excluded(after.as_slice())),
            CmpOp::Gt => (Bound::Excluded(after.as_slice()), Bound::Excluded(end.as_slice())),
            CmpOp::Gte => (Bound::Included(key), Bound::Excluded(end.as_slice())),
            CmpOp::Eq | CmpOp::Neq => unreachable!("not an ordered comparison"),
        };
        let to = if region.is_empty() && matches!(op, CmpOp::Gt | CmpOp::Gte) {
            Bound::Unbounded
        } else {
            to
        };
        self.part_ids(field, part, from, to, &|k| {
            k.starts_with(region) && exclude.is_none_or(|e| !k.starts_with(e))
        })
    }

    /// [`Self::ordered`] for an operand whose key may be cut and hashed: the
    /// long texts sharing its prefix are the one region the key order cannot
    /// decide, so they are read out of the range and decided by `holds` on
    /// their values.
    fn ordered_exact(
        &self,
        field: &str,
        part: u8,
        region: &[u8],
        op: CmpOp,
        key: &[u8],
        holds: &dyn Fn(&Value) -> bool,
    ) -> RoaringBitmap {
        // Only a text key can be cut: a number's or a date's raw bytes may
        // well contain the marker.
        let text = part == NAME || key.first() == Some(&2);
        let Some(tail) = long_prefix(&key[region.len()..]).filter(|_| text) else {
            return self.ordered(field, part, region, op, key, None);
        };
        let ambiguous = [region, tail].concat();
        let mut out = self.ordered(field, part, region, op, key, Some(&ambiguous));
        let mut end = ambiguous.clone();
        *end.last_mut().expect("a marker") += 1;
        let undecided =
            self.part_ids(field, part, Bound::Included(&ambiguous), Bound::Excluded(&end), &|_| {
                true
            });
        out |= self.having(undecided, field, holds);
        out
    }

    /// The ids of `ids` with a row of `field` for which `holds` is true.
    fn having(
        &self,
        ids: RoaringBitmap,
        field: &str,
        holds: &dyn Fn(&Value) -> bool,
    ) -> RoaringBitmap {
        // The candidates checked one by one: what a query's progress counts
        // (doc "Task status and progress").
        let mut done = crate::progress::counting(ids.len());
        ids.iter()
            .filter(|&id| {
                done.tick();
                self.values(id, field).iter().any(holds)
            })
            .collect()
    }

    // ── The schema check's candidates (doc "Schema") ────────────────────────
    //
    // Answered from the sets and the postings: a healthy repository answers
    // each from a few bitmaps, where reading the constrained fields' rows cost
    // a key per row (`tests/kv_cost.rs`). `tests/store_contract.rs` holds
    // each to a derivation from the rows, in id order rather than row order.

    /// The value types `field` holds rows of (`field_types`).
    fn types_of(&self, field: &str) -> Result<Vec<String>> {
        let prefix = name_key(field);
        let mut out = Vec::new();
        for e in self.t.field_types.prefix_iter(&self.r, &prefix)? {
            self.read_keys(1)?;
            out.push(super::codec::field_type(&e?.0[prefix.len()..])?.to_string());
        }
        Ok(out)
    }

    /// How many values of `field` each id holds: a set per bit of the count,
    /// the lowest first (doc "Store tables"). A read per chunk of each.
    fn counts(&self, field: &str) -> Vec<RoaringBitmap> {
        let counts = self.ok(derived::read_counts(&self.t, &self.r, field), Vec::new());
        let keys: u64 =
            counts.iter().map(|bm| 1 + bm.max().map_or(0, |m| u64::from(m >> 16))).sum();
        if !self.counted(keys.max(1)) {
            return Vec::new();
        }
        counts
    }

    /// The ids holding a value of `field`: those of any bit of the counts.
    fn valued(&self, field: &str) -> RoaringBitmap {
        self.counts(field).into_iter().fold(RoaringBitmap::new(), |a, b| a | b)
    }

    /// The ids holding a row of `field`, `Nothing` or not.
    fn holders(&self, field: &str) -> RoaringBitmap {
        self.valued(field) | self.set(derived::IndexSet::Nothing, Some(field))
    }

    /// The ids holding more than `n` rows of `field` — its values, and the
    /// `Nothing` row it may hold beside them — read off the counts, no row
    /// being.
    fn rows_over(&self, field: &str, n: i64) -> RoaringBitmap {
        let counts = self.counts(field);
        let unknown = self.set(derived::IndexSet::Nothing, Some(field));
        let Ok(n) = u64::try_from(n) else {
            return counts.into_iter().fold(unknown, |a, b| a | b);
        };
        // With a `Nothing` row, one value less makes as many rows.
        let mut over = derived::counted_over(&counts, n);
        over |= match n.checked_sub(1) {
            Some(less) => derived::counted_over(&counts, less) & &unknown,
            None => unknown,
        };
        over
    }

    /// The uuids of up to `limit` of `ids`, and the first read error met.
    fn uuids_of(&self, ids: impl Iterator<Item = u32>, limit: i64) -> Result<Vec<Uuid>> {
        let limit = usize::try_from(limit).unwrap_or(0);
        let mut out = Vec::new();
        for id in ids {
            if out.len() >= limit {
                break;
            }
            if !self.live().contains(id) {
                continue;
            }
            if let Some(u) = self.uuid(id) {
                out.push(u);
            }
        }
        self.take_error().map_or(Ok(out), Err)
    }

    /// Up to `limit` metarecords holding a non-`Nothing` row of `field` of
    /// another type than `allowed`. A field holds one type (the `Writer` keeps
    /// it so): when it is not `allowed`, every holder of a value. Mixed types
    /// are read off the value partition's run of each other type, a type tag
    /// apart — but `int` and `float` share theirs, so those rows are read back
    /// for their type.
    pub(crate) fn wrong_type(&self, field: &str, allowed: &str, limit: i64) -> Result<Vec<Uuid>> {
        let types = self.types_of(field)?;
        if let [only] = types.as_slice() {
            let ids = if only == allowed { RoaringBitmap::new() } else { self.valued(field) };
            return self.uuids_of(ids.iter(), limit);
        }
        let mut out = RoaringBitmap::new();
        for ty in types {
            if ty == allowed {
                continue;
            }
            let Some(tag) = type_tag(&ty) else { continue };
            let (from, to) = ([tag], [tag + 1]);
            // A position is no posting: every id with one is under some parent.
            let run = if matches!(ty.as_str(), "tree_ref" | "hierarchy_ref") {
                self.any_referrers(field)
            } else {
                self.part_ids(field, VALUE, Bound::Included(&from), Bound::Excluded(&to), &|_| true)
            };
            out |= if type_tag(allowed) == Some(tag) {
                self.having(run, field, &|v| crate::rows::encode_value(v).value_type == ty)
            } else {
                run
            };
        }
        self.uuids_of(out.iter(), limit)
    }

    /// Up to `limit` metarecords with more than `max` rows of `field`.
    pub(crate) fn count_over(&self, field: &str, max: i64, limit: i64) -> Result<Vec<Uuid>> {
        self.uuids_of(self.rows_over(field, max).iter(), limit)
    }

    /// Up to `limit` metarecords holding `field`, with fewer than `min` rows.
    pub(crate) fn count_under(&self, field: &str, min: i64, limit: i64) -> Result<Vec<Uuid>> {
        if min <= 1 {
            return self.uuids_of(std::iter::empty(), limit);
        }
        let under = self.holders(field) - self.rows_over(field, min - 1);
        self.uuids_of(under.iter(), limit)
    }

    /// Up to `limit` metarecords with no `field` row at all.
    pub(crate) fn missing(&self, field: &str, limit: i64) -> Result<Vec<Uuid>> {
        let ids = self.live_set() - self.holders(field);
        self.uuids_of(ids.iter(), limit)
    }

    /// Up to `limit` metarecords declared one of `types` (`mf_schema`) with no
    /// `field` row.
    pub(crate) fn typed_missing(
        &self,
        types: &[String],
        field: &str,
        limit: i64,
    ) -> Result<Vec<Uuid>> {
        let mut declared = RoaringBitmap::new();
        for ty in types {
            let value = Value::String(ty.clone());
            let key = value_key(&value).expect("a string has a value key");
            declared |= self.equal("mf_schema", &key, &value);
        }
        let ids = declared - self.holders(field);
        self.uuids_of(ids.iter(), limit)
    }

    /// The ids holding exactly `value` in the value partition: its bucket,
    /// read back when its key is cut and hashed.
    fn equal(&self, field: &str, key: &[u8], value: &Value) -> RoaringBitmap {
        if value.position().is_some() {
            return self.position_holders(field, value);
        }
        let ids = self.bucket(field, VALUE, key);
        if is_hashed(key) {
            self.having(ids, field, &|v| v == value)
        } else {
            ids
        }
    }

    /// The metarecord holding the position `value` in `field`: the forest has
    /// one for a parent and a name, and a position is no posting of its own.
    fn position_holders(&self, field: &str, value: &Value) -> RoaringBitmap {
        let read = || -> Result<RoaringBitmap> {
            self.read_keys(1)?;
            let Some(k) = derived::raw_key_of(&self.t, &self.r, field, value)? else {
                return Ok(RoaringBitmap::new());
            };
            self.read_keys(1)?;
            Ok(match self.t.index_raw_holder.get(&self.r, &k)? {
                Some(v) => RoaringBitmap::from_iter([dense(v)]),
                None => RoaringBitmap::new(),
            })
        };
        self.ok(read(), RoaringBitmap::new())
    }

    /// The ids pointing at anything in `field`: a key per chunk of each
    /// target's referrers.
    fn any_referrers(&self, field: &str) -> RoaringBitmap {
        let read = || -> Result<RoaringBitmap> {
            let prefix = name_key(field);
            let mut out = RoaringBitmap::new();
            for entry in self.t.index_parent_holders.prefix_iter(&self.r, &prefix)? {
                let (_, v) = entry?;
                self.read_keys(1)?;
                derived::add_encoded(&mut out, v)?;
            }
            Ok(out)
        };
        self.ok(read(), RoaringBitmap::new())
    }

    /// A metarecord's rows of `field`.
    fn values(&self, id: u32, field: &str) -> Vec<Value> {
        let read = || -> Result<Vec<Value>> {
            // The record: one key, and the entries of its part.
            let rows = mass::read_record(&self.t, &self.r, id, self.writes_pending, &mut |n| {
                self.read_keys(n)
            })?;
            Ok(rows.into_iter().filter(|r| r.name == field).map(|r| r.value).collect())
        };
        self.ok(read(), Vec::new())
    }

    /// What `literals`' trigrams answer of `field` within `restrict` (doc
    /// "Text search in the store"). With a trigram kept by value, the texts
    /// listed under all of them are few: each is tested on its key, and the
    /// holders of those that match are the answer — but for the texts whose
    /// key is cut, which are left to check. Otherwise the ids under the
    /// trigrams kept by metarecord are candidates to check.
    /// Either way with the texts too long to be split.
    fn gram_answer(
        &self,
        field: &str,
        keep: &dyn Fn(&str) -> bool,
        literals: &[String],
        restrict: Option<&RoaringBitmap>,
    ) -> GramAnswer {
        let grams: BTreeSet<[u8; 3]> = literals.iter().flat_map(|l| derived::trigrams(l)).collect();
        if grams.is_empty() {
            return GramAnswer::Nothing;
        }
        let within = |mut ids: RoaringBitmap| {
            if let Some(r) = restrict {
                ids &= r;
            }
            ids
        };
        let look = self.gram_lookup(field, &grams, restrict);
        let long = within(self.set(derived::IndexSet::LongTexts, Some(field)));
        if let Some(texts) = look.texts {
            let mut out = RoaringBitmap::new();
            let mut undecided = long;
            for (part, key) in texts {
                let region: &[u8] = if part == VALUE { &[2] } else { &[] };
                let Some(tail) = key.strip_prefix(region) else { continue };
                if long_prefix(tail).is_some() {
                    undecided |= within(self.bucket(field, part, &key));
                } else if derived::unesc(tail)
                    .is_ok_and(|(bytes, _)| keep(&String::from_utf8_lossy(&bytes)))
                {
                    out |= within(self.bucket(field, part, &key));
                }
            }
            undecided -= &out;
            return GramAnswer::Exact(self.sift(out), self.sift(undecided));
        }
        match look.ids {
            Some(ids) => GramAnswer::Candidates(self.sift(within(ids) | long)),
            None => GramAnswer::Nothing,
        }
    }

    /// The ids whose text in `field` may satisfy `keep`, within `restrict`
    /// — a superset of the answer, from the trigrams ([`Self::gram_answer`]).
    /// `None` when they narrow nothing.
    fn text_candidates(
        &self,
        field: &str,
        keep: &dyn Fn(&str) -> bool,
        literals: &[String],
        restrict: Option<&RoaringBitmap>,
    ) -> Option<RoaringBitmap> {
        match self.gram_answer(field, keep, literals, restrict) {
            GramAnswer::Exact(out, undecided) => Some(out | undecided),
            GramAnswer::Candidates(candidates) => Some(candidates),
            GramAnswer::Nothing => None,
        }
    }

    /// The ids of `field`'s partition `part` whose text satisfies `keep`,
    /// within `restrict`. Trigrams kept by value name the texts to test,
    /// whose holders are then read without a metarecord being; kept by
    /// metarecord they name candidates, each read. A small candidate set
    /// reads its own rows; a large one — or a field most of whose ids await
    /// a fold — scans the distinct keys, testing each once.
    fn scan(
        &self,
        field: &str,
        part: u8,
        text_of: &dyn Fn(&Value) -> Option<String>,
        keep: &dyn Fn(&str) -> bool,
        literals: &[String],
        restrict: Option<&RoaringBitmap>,
    ) -> RoaringBitmap {
        let _stage = crate::progress::stage(format!("scanning {field}"));
        let text = |v: &Value| text_of(v).is_some_and(|t| keep(&t));
        let region: &[u8] = if part == VALUE { &[2] } else { &[] };
        match self.gram_answer(field, keep, literals, restrict) {
            GramAnswer::Exact(out, undecided) => return out | self.having(undecided, field, &text),
            GramAnswer::Candidates(candidates) => return self.having(candidates, field, &text),
            GramAnswer::Nothing => {}
        }
        if let Some(r) = restrict {
            if plan::probes(r.len(), self.present(field).len()) {
                return self.having(r.clone(), field, &text);
            }
        }
        let last: RefCell<Option<(Vec<u8>, bool)>> = RefCell::new(None);
        // A key cut and hashed does not hold its whole text: its ids are
        // decided on their values, after the pass.
        let hashed = |k: &[u8]| long_prefix(&k[region.len()..]).is_some();
        let saw_hashed = Cell::new(false);
        let test = |k: &[u8]| {
            if hashed(k) {
                saw_hashed.set(true);
                return false;
            }
            let mut last = last.borrow_mut();
            if let Some((key, verdict)) = last.as_ref() {
                if key == k {
                    return *verdict;
                }
            }
            let text = derived::unesc(&k[region.len()..])
                .map(|(bytes, _)| String::from_utf8_lossy(&bytes).into_owned());
            let verdict = text.is_ok_and(|t| keep(&t));
            *last = Some((k.to_vec(), verdict));
            verdict
        };
        let end = [region.first().map_or(0xFF, |t| t + 1)];
        let to = if region.is_empty() { Bound::Unbounded } else { Bound::Excluded(&end[..]) };
        let mut out = self.part_ids(field, part, Bound::Included(region), to, &test);
        if saw_hashed.get() {
            let mut long = self.part_ids(field, part, Bound::Included(region), to, &hashed);
            if let Some(r) = restrict {
                long &= r;
            }
            out |= self.having(long, field, &|v| text_of(v).is_some_and(|t| keep(&t)));
        }
        if let Some(r) = restrict {
            out &= r;
        }
        out
    }
}

/// Whether a value key holds a text cut and hashed (a string, or a
/// `tree_ref`'s name after its parent).
fn is_hashed(key: &[u8]) -> bool {
    match key.first() {
        Some(2) => long_prefix(&key[1..]).is_some(),
        Some(7) => long_prefix(&key[17..]).is_some(),
        _ => false,
    }
}

/// A range of a table's entries, either way.
type Entries<'t> = Box<dyn Iterator<Item = heed::Result<(&'t [u8], &'t [u8])>> + 't>;

/// For a value key holding a text cut and hashed: the run of keys sharing
/// its first bytes (tag, prefix and marker), which sort by hash, not value.
fn hashed_run(key: &[u8]) -> Option<&[u8]> {
    match key.first() {
        Some(2) => long_prefix(&key[1..]).map(|p| &key[..1 + p.len()]),
        _ => None,
    }
}

/// The sort representative of a value key holding its whole value — a bool,
/// a number, a date, a text keyed whole. `None` for a reference, which the
/// partition does not sort, and for a text cut and hashed.
fn key_rep(key: &[u8]) -> Option<SortRep> {
    let int = |b: &[u8]| Some(u64::from_be_bytes(b.try_into().ok()?));
    Some(match key.split_first()? {
        (0, [b]) => SortRep::Bool(*b != 0),
        (1, rest) => SortRep::Num(crate::index::keys::num_of_key(int(rest)?)),
        (2, rest) if long_prefix(rest).is_none() => {
            let (bytes, _) = derived::unesc(rest).ok()?;
            SortRep::Str(String::from_utf8(bytes).ok()?.into())
        }
        (3, rest) => SortRep::DateTime(crate::index::keys::dt_of_key(int(rest)?)),
        _ => return None,
    })
}

/// The value a sort representative stands for, as far as its key goes: an
/// int and a float of equal value share a key.
fn rep_value(rep: &SortRep) -> Option<Value> {
    Some(match rep {
        SortRep::Bool(b) => Value::Bool(*b),
        SortRep::Num(x) => Value::Float(*x),
        SortRep::Str(s) => Value::String(s.to_string()),
        SortRep::DateTime(ms) => Value::DateTime(*ms),
        SortRep::Ref(_) | SortRep::Tree(_) => return None,
    })
}

/// A position's name (`tree_ref`, `hierarchy_ref`), as the text partitions
/// hold it.
fn name_of(v: &Value) -> Option<String> {
    v.position().map(|(_, name)| name.display().into_owned())
}

/// What a search's trigrams name in a field ([`KvSource::gram_lookup`]).
struct GramLookup {
    /// The texts listed under every trigram kept by value, when one is.
    texts: Option<Vec<derived::GramText>>,
    /// The ids under every trigram kept by metarecord, when one is.
    ids: Option<RoaringBitmap>,
}

/// What a search's trigrams answer ([`KvSource::gram_answer`]).
enum GramAnswer {
    /// The holders of the texts that match, and the ids left to check (a
    /// text whose key is cut, a text too long to be split).
    Exact(RoaringBitmap, RoaringBitmap),
    /// Ids to check, each on its values.
    Candidates(RoaringBitmap),
    /// The trigrams narrow nothing.
    Nothing,
}

impl Source for KvSource<'_> {
    fn universe(&self) -> Cow<'_, RoaringBitmap> {
        Cow::Borrowed(self.live())
    }

    fn present(&self, field: &str) -> Cow<'_, RoaringBitmap> {
        Cow::Owned(self.sift(self.valued(field)))
    }

    fn absent(&self, field: &str) -> Cow<'_, RoaringBitmap> {
        Cow::Owned(self.sift(self.set(derived::IndexSet::Nothing, Some(field))))
    }

    fn value_type(&self, field: &str) -> Option<&str> {
        const TYPES: [&str; 10] = [
            "string",
            "int",
            "float",
            "bool",
            "datetime",
            "ref",
            "tree_ref",
            "hierarchy_ref",
            "refbase",
            "externalref",
        ];
        let read = || -> Result<Option<&'static str>> {
            let prefix = name_key(field);
            for entry in self.t.field_types.prefix_iter(&self.r, &prefix)? {
                let (k, _) = entry?;
                self.read_keys(1)?;
                let ty = super::codec::field_type(&k[prefix.len()..])?;
                if let Some(t) = TYPES.iter().find(|t| **t == ty) {
                    return Ok(Some(t));
                }
            }
            Ok(None)
        };
        self.ok(read(), None)
    }

    fn id(&self, uuid: Uuid) -> Option<u32> {
        if !self.counted(1) {
            return None;
        }
        let r = id_of(&self.t, &self.r, uuid.as_bytes());
        // A deleted metarecord keeps its id, which names nothing any more.
        self.ok(r, None).filter(|&id| self.is_live(id))
    }

    fn uuid(&self, id: u32) -> Option<Uuid> {
        if !self.counted(1) {
            return None;
        }
        let r = self.t.uuids.get(&self.r, &id.to_be_bytes()).map(|u| u.map(uuid_of));
        self.ok(r.map_err(Into::into), None)
    }

    fn id_count(&self) -> u64 {
        let r = self.t.ids.len(&self.r);
        self.ok(r.map_err(Into::into), 0)
    }

    fn numbering(&self) -> u64 {
        let read = || -> Result<u64> {
            self.read_keys(1)?;
            Ok(super::State::NextReindex.get(&self.t, &self.r)?.unwrap_or(0) as u64)
        };
        self.ok(read(), 0)
    }

    fn in_uuid_order(&self, after: Option<Uuid>) -> Box<dyn Iterator<Item = (Uuid, u32)> + '_> {
        let after = after.map(|u| *u.as_bytes());
        let lo = match &after {
            Some(u) => Bound::Excluded(&u[..]),
            None => Bound::Unbounded,
        };
        let range = (lo, Bound::Unbounded);
        let live = self.live();
        match self.t.ids.range(&self.r, &range) {
            Ok(iter) => Box::new(
                iter.map_while(move |entry| match entry {
                    Ok((k, v)) => self.counted(1).then(|| (uuid_of(k), dense(v))),
                    Err(e) => {
                        self.error.borrow_mut().get_or_insert(e.into());
                        None
                    }
                })
                .filter(move |(_, id)| live.contains(*id)),
            ),
            Err(e) => {
                self.error.borrow_mut().get_or_insert(e.into());
                Box::new(std::iter::empty())
            }
        }
    }

    fn range_estimate(&self, field: &str, op: CmpOp, value: &Value) -> Option<u64> {
        self.range_rows(field, op, value).map(|e| e.at_most)
    }

    fn compare(
        &self,
        field: &str,
        op: CmpOp,
        value: &Value,
        restrict: Option<&RoaringBitmap>,
    ) -> Result<RoaringBitmap, Unsupported> {
        let answer = || -> Result<RoaringBitmap, Unsupported> {
            let _stage = crate::progress::stage(format!("comparing {field}"));
            let Some(kind) = self.kind(field) else { return Ok(RoaringBitmap::new()) };
            // A string operand on a forest compares the names.
            if let (Kind::Reference { tree: true }, Value::String(s)) = (kind, value) {
                let key = text_key(s.as_bytes());
                let hashed = long_prefix(&key).is_some();
                let named =
                    |want: bool| move |v: &Value| name_of(v).is_some_and(|n| (n == *s) == want);
                return Ok(match op {
                    CmpOp::Eq if hashed => {
                        self.having(self.bucket(field, NAME, &key), field, &named(true))
                    }
                    CmpOp::Eq => self.bucket(field, NAME, &key),
                    CmpOp::Neq => {
                        let mut out = self.all_but(field, NAME, Some(&key));
                        if hashed {
                            out |=
                                self.having(self.bucket(field, NAME, &key), field, &named(false));
                        }
                        out
                    }
                    _ => {
                        let holds = |v: &Value| {
                            name_of(v)
                                .is_some_and(|n| op.matches_ordering(n.as_str().cmp(s.as_str())))
                        };
                        self.ordered_exact(field, NAME, &[], op, &key, &holds)
                    }
                });
            }
            let key = value_key(value);
            // A few candidates are checked on their own rows, where a walk would
            // read a key per row of the range (an equality reads one value's
            // posting or run: it is never probed). Each row is decided by its key,
            // as the walk decides it.
            let probe = restrict.filter(|r| op != CmpOp::Eq && self.probes(r, field, op, value));
            match op {
                CmpOp::Eq => Ok(key.map(|k| self.equal(field, &k, value)).unwrap_or_default()),
                CmpOp::Neq if probe.is_some() => {
                    let differs = |v: &Value| match value_key(v) {
                        None => false,
                        // A key cut and hashed is shared: the values decide.
                        Some(k) if Some(&k) == key.as_ref() => is_hashed(&k) && v != value,
                        Some(_) => true,
                    };
                    Ok(self.having(probe.expect("a probe").clone(), field, &differs))
                }
                CmpOp::Neq if value.position().is_some() => {
                    // Whoever holds a position holds another unless this is
                    // its only one.
                    let mut out = self.valued(field);
                    for id in self.position_holders(field, value) {
                        if self.values(id, field).iter().all(|v| v == value) {
                            out.remove(id);
                        }
                    }
                    Ok(out)
                }
                CmpOp::Neq => {
                    let mut out = self.all_but(field, VALUE, key.as_deref());
                    if let Some(k) = key.as_deref().filter(|k| is_hashed(k)) {
                        let differs = |v: &Value| !matches!(v, Value::Nothing) && v != value;
                        out |= self.having(self.bucket(field, VALUE, k), field, &differs);
                    }
                    Ok(out)
                }
                _ => {
                    // The operands an ordered comparison can use on each kind; a
                    // bool or a reference is refused (a 400 upstream), any other
                    // operand finds no row.
                    let usable = match (kind, value) {
                        (Kind::Categorical, Value::Bool(_)) => {
                            return Err(unsupported("ordered comparison on bool"))
                        }
                        (
                            Kind::Reference { .. },
                            Value::Ref(_)
                            | Value::RefBase(_)
                            | Value::TreeRef { .. }
                            | Value::HierarchyRef { .. }
                            | Value::ExternalRef { .. },
                        ) => return Err(unsupported("ordered comparison on a reference")),
                        (Kind::Categorical, Value::String(_)) => true,
                        (Kind::Numeric, Value::Int(_) | Value::Float(_)) => true,
                        (Kind::Datetime, Value::DateTime(_)) => true,
                        _ => false,
                    };
                    Ok(match key {
                        Some(k) if usable => {
                            let holds = |v: &Value| match (v, value) {
                                (Value::String(x), Value::String(o)) => {
                                    op.matches_ordering(x.as_str().cmp(o.as_str()))
                                }
                                _ => false,
                            };
                            let region = &k[..1];
                            match probe {
                                Some(r) => {
                                    // `ordered_exact`'s decision, row by row: the
                                    // texts cut where the operand is are decided
                                    // on their values, the rest on their keys.
                                    let ambiguous = long_prefix(&k[1..])
                                        .filter(|_| region == [2])
                                        .map(|tail| [region, tail].concat());
                                    let row = |v: &Value| match value_key(v) {
                                        None => false,
                                        Some(rk)
                                            if ambiguous
                                                .as_ref()
                                                .is_some_and(|a| rk.starts_with(a)) =>
                                        {
                                            holds(v)
                                        }
                                        Some(rk) => {
                                            rk.starts_with(region)
                                                && op.matches_ordering(
                                                    rk.as_slice().cmp(k.as_slice()),
                                                )
                                        }
                                    };
                                    self.having(r.clone(), field, &row)
                                }
                                None => self.ordered_exact(field, VALUE, region, op, &k, &holds),
                            }
                        }
                        _ => RoaringBitmap::new(),
                    })
                }
            }
        };
        answer().map(|ids| self.sift(ids))
    }

    fn same_as(&self, field: &str, seed: &RoaringBitmap) -> RoaringBitmap {
        // Each value the seed holds, under its key: a hashed key is resolved
        // against the values themselves.
        let mut wanted: BTreeMap<Vec<u8>, Vec<Value>> = BTreeMap::new();
        for v in seed.iter().flat_map(|id| self.values(id, field)) {
            if let Some(k) = value_key(&v) {
                wanted.entry(k).or_default().push(v);
            }
        }
        let sharing = wanted
            .iter()
            .map(|(k, values)| {
                if values.iter().any(|v| v.position().is_some()) {
                    return values.iter().map(|v| self.position_holders(field, v)).union();
                }
                let ids = self.bucket(field, VALUE, k);
                if is_hashed(k) {
                    self.having(ids, field, &|v| values.contains(v))
                } else {
                    ids
                }
            })
            .union();
        self.sift(sharing)
    }

    fn scan_text(
        &self,
        field: &str,
        keep: &dyn Fn(&str) -> bool,
        literals: &[String],
        restrict: Option<&RoaringBitmap>,
    ) -> RoaringBitmap {
        match self.kind(field) {
            Some(Kind::Categorical) => {
                let text = |v: &Value| match v {
                    Value::String(s) => Some(s.clone()),
                    _ => None,
                };
                self.sift(self.scan(field, VALUE, &text, keep, literals, restrict))
            }
            Some(Kind::Reference { tree: true }) => {
                self.scan_names(field, keep, literals, restrict)
            }
            _ => RoaringBitmap::new(),
        }
    }

    fn scan_names(
        &self,
        field: &str,
        keep: &dyn Fn(&str) -> bool,
        literals: &[String],
        restrict: Option<&RoaringBitmap>,
    ) -> RoaringBitmap {
        if self.kind(field) != Some(Kind::Reference { tree: true }) {
            return RoaringBitmap::new();
        }
        self.sift(self.scan(field, NAME, &name_of, keep, literals, restrict))
    }

    fn text_superset(
        &self,
        field: &str,
        keep: &dyn Fn(&str) -> bool,
        literals: &[String],
        restrict: Option<&RoaringBitmap>,
    ) -> Option<RoaringBitmap> {
        self.text_candidates(field, keep, literals, restrict)
    }

    fn named_positions(
        &self,
        field: &str,
        keep: &dyn Fn(&str) -> bool,
        literals: &[String],
        within: Option<&RoaringBitmap>,
    ) -> Option<Vec<NamedNode>> {
        if self.kind(field) != Some(Kind::Reference { tree: true }) {
            return Some(Vec::new());
        }
        let candidates = self.text_candidates(field, keep, literals, within)?;
        let read = || -> Result<Vec<NamedNode>> {
            let mut out = Vec::new();
            for id in &candidates {
                self.read_keys(1)?;
                let Some(uuid) = self.t.uuids.get(&self.r, &id.to_be_bytes())? else { continue };
                // The record's rows: one key.
                let mut positions = Vec::new();
                let rows =
                    mass::read_record(&self.t, &self.r, id, self.writes_pending, &mut |n| {
                        self.read_keys(n)
                    })?;
                for row in rows {
                    if row.name != field {
                        continue;
                    }
                    if let Some((parent, name)) = row.value.position() {
                        positions.push((parent, name.display().into_owned()));
                    }
                }
                if positions.iter().any(|(_, name)| keep(name)) {
                    out.push((uuid_of(uuid), positions));
                }
            }
            Ok(out)
        };
        Some(self.ok(read(), Vec::new()))
    }

    fn follow(&self, field: &str) -> Option<Follow> {
        Some(match self.value_type(field)? {
            "tree_ref" | "hierarchy_ref" => Follow::Tree,
            "ref" => Follow::Direct,
            _ => Follow::None,
        })
    }

    fn referrers(&self, field: &str, target: Uuid) -> Option<Cow<'_, RoaringBitmap>> {
        let ids = self.sift(self.referrers_of(field, target.as_bytes()));
        (!ids.is_empty()).then_some(Cow::Owned(ids))
    }

    fn tree_roots(&self, field: &str) -> RoaringBitmap {
        self.sift(self.referrers_of(field, &[0; 16]))
    }

    fn tree_parents_except(&self, field: &str, except: Option<Uuid>) -> RoaringBitmap {
        // Every parent's referrers but that one's: a key per parent and
        // 65 536 children, where the positions are a key each.
        let read = || -> Result<RoaringBitmap> {
            // The zero uuid is "no parent", which the roots are filed under.
            let except = match except {
                None => None,
                Some(u) if u.is_nil() => Some(derived::NO_TARGET.to_be_bytes().to_vec()),
                Some(u) => {
                    self.read_keys(1)?;
                    self.t.ids.get(&self.r, u.as_bytes())?.map(|id| id.to_vec())
                }
            };
            let prefix = name_key(field);
            let mut out = RoaringBitmap::new();
            for entry in self.t.index_parent_holders.prefix_iter(&self.r, &prefix)? {
                let (k, v) = entry?;
                self.read_keys(1)?;
                if k.len() == prefix.len() + 6 && except.as_deref() != Some(&k[prefix.len()..][..4])
                {
                    derived::add_encoded(&mut out, v)?;
                }
            }
            Ok(out)
        };
        self.sift(self.ok(read(), RoaringBitmap::new()))
    }

    // Every forest keeps its descendant bitmaps; another field has none, and
    // no node below anything, as its expansion would find.
    fn descendants(&self, field: &str, of: &RoaringBitmap) -> Option<RoaringBitmap> {
        // Only a node with children has descendants: many nodes (the anchors
        // of an `osm` path, mostly files) are sifted by the parents set, a
        // read per 65 536 ids, rather than looked up one by one.
        let sifted;
        let of = if of.len() > 1 {
            sifted = of & &*self.parents(field);
            &sifted
        } else {
            of
        };
        let read = || -> Result<RoaringBitmap> {
            let mut out = RoaringBitmap::new();
            for id in of {
                let prefix = derived::descendants_prefix(field, id);
                for entry in self.t.index_descendants.prefix_iter(&self.r, &prefix)? {
                    let (k, v) = entry?;
                    self.read_keys(1)?;
                    if k.len() == prefix.len() + 2 {
                        derived::add_encoded(&mut out, v)?;
                    }
                }
            }
            Ok(out)
        };
        Some(self.sift(self.ok(read(), RoaringBitmap::new())))
    }

    fn parents(&self, field: &str) -> Cow<'_, RoaringBitmap> {
        Cow::Owned(self.sift(self.set(derived::IndexSet::Parents, Some(field))))
    }

    fn sort_reps(&self, field: &str, want_max: bool) -> RepReader<'_> {
        let field = field.to_string();
        Box::new(move |id| {
            let reps = self.values(id, &field).into_iter().filter_map(|v| sort_rep(&v));
            if want_max {
                reps.max()
            } else {
                reps.min()
            }
        })
    }

    fn several(&self, field: &str) -> RoaringBitmap {
        // Several values: a count of two or more, any bit but the first.
        let several =
            self.counts(field).into_iter().skip(1).fold(RoaringBitmap::new(), |a, b| a | b);
        self.sift(several)
    }

    fn walk_values(
        &self,
        field: &str,
        want_max: bool,
        start: Option<&SortRep>,
        visit: &mut dyn FnMut(&SortRep, Walked<'_>) -> bool,
    ) -> bool {
        // The value partition sorts like the values (its keys are built for
        // it) on a scalar field; a reference or a forest sorts otherwise.
        if !matches!(self.kind(field), Some(Kind::Categorical | Kind::Numeric | Kind::Datetime)) {
            return false;
        }
        // A value the pending index writes name is walked at its key: one whose
        // key does not hold it whole has no place there, and the page is
        // fetched, from the rows.
        if index_writes::named(&self.index_writes(field)).iter().any(|k| key_rep(k).is_none()) {
            return false;
        }
        let start_key = match start {
            None => None,
            Some(rep) => match rep_value(rep).as_ref().and_then(value_key) {
                Some(k) => Some(k),
                None => return false,
            },
        };
        // The partition still lists the deleted metarecords: they are
        // stepped over, a run without them.
        let live = self.live();
        let mut sifted = |rep: &SortRep, walked: Walked<'_>| match walked {
            Walked::One(id) => !live.contains(id) || visit(rep, Walked::One(id)),
            Walked::Run(run) => {
                let run = run & live;
                run.is_empty() || visit(rep, Walked::Run(&run))
            }
        };
        let r = self.walk(field, want_max, start, start_key.as_deref(), &mut sifted);
        self.ok(r, ());
        true
    }
}
