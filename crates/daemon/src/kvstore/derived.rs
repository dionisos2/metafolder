//! The derived key spaces (doc "Storage"
//! a): what the query source reads, written in each revision's own
//! transaction and rebuilt from the primary data by [`KvStore::reindex`].
//!
//! | table | key                                  | value                    |
//! |-------|--------------------------------------|--------------------------|
//! | ids   | uuid                                 | dense id (u32)           |
//! | uuids | dense id                             | uuid                     |
//! | sets  | kind · [field] · [key] · chunk       | a roaring bitmap         |
//! | parts | field · partition · key · dense id   | rows (count)             |
//! | kids  | parent uuid · field · dense id       | rows (count)             |
//! | grams | field · trigram · chunk              | a roaring bitmap         |
//!
//! A *set* is a bitmap of dense ids cut in chunks of 65 536 ids (the high 16
//! bits), so a write rewrites one chunk and a read unions a few: the universe,
//! and per field the ids holding a value (`present`), a `Nothing` (`absent`),
//! and — on a `tree_ref` field — a child (`parents`). A *partition* orders a
//! field's rows by a key whose bytes sort like the values: the value itself
//! (equality, ranges, the distinct-value scans), a `tree_ref`'s name, and the
//! uuid a value points at (its referent, or its parent). `kids` is the forest
//! keyed by parent first, so a metarecord arriving or leaving finds the fields
//! it is a parent in without a scan. A value held by [`POSTING_MIN`] records
//! or more also gets a *posting*: its holders as one set, so an equality on a
//! frequent value reads a key per 65 536 holders, not one per holder.
//!
//! Every row change goes through [`KvTxn::derive_row`], every metarecord
//! creation and removal through [`KvTxn::derive_created`] /
//! [`KvTxn::derive_removed`] — the log's navigation included, which writes by
//! the same primitives. [`KvStore::check_derived`] holds all of it to a
//! rebuild.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{Context, Result};
use heed::RoTxn;
use metafolder_core::metarecord::Value;
use roaring::RoaringBitmap;
use uuid::Uuid;

use super::{be, dec_row, from_be, name_key, uuid_of, KvStore, KvTxn, Tables};
use crate::index::keys::{dt_key, num_key};
use crate::store::Rows;

/// The format of the derived key spaces. A store stamped with another (or
/// none: a store from before they existed) is reindexed when it opens. 5:
/// descendant bitmaps for every forest (4 kept `mfr_path`'s only).
pub(super) const DERIVED_VERSION: i64 = 5;

/// Set kinds.
pub(super) const UNIVERSE: u8 = 0;
pub(super) const PRESENT: u8 = 1;
pub(super) const ABSENT: u8 = 2;
pub(super) const PARENTS: u8 = 3;

/// The texts too long to be split in trigrams (see [`TRIGRAM_MAX`]).
pub(super) const LONG_TEXTS: u8 = 4;
/// The ids pointing at a uuid in a field — a `ref`'s referrers, a folder's
/// children — keyed by field *and* target: a folder filter is one bitmap,
/// a read per 65 536 children (doc "Store tables", `children`).
pub(super) const REFERRERS: u8 = 5;

/// Every id below a node of a forest, keyed by field and node — what makes a
/// subtree one bitmap read (doc "The forest in the store").
/// Kept for every `tree_ref` field: a node holds one position per forest
/// (doc "One position per forest"), so the tree is a tree and a
/// move is set arithmetic.
pub(super) const DESCENDANTS: u8 = 6;

/// The holders of one value of a field (keyed by field and value key), for
/// the values held by [`POSTING_MIN`] records or more (doc "Store tables", `postings`). A rarer
/// value is answered from its run in the value
/// partition, where its ids are adjacent. A posting is never demoted: it
/// stays, exact, when its value grows rarer, and goes only with its last
/// holder — so "a value has a posting" always means "all its ids are in it".
pub(super) const POSTINGS: u8 = 7;

/// The ids holding several non-`Nothing` rows of a field — the ones whose
/// sort representative is not simply their value, which a walk of the
/// values must read one by one.
pub(super) const MULTI: u8 = 8;

/// How many holders promote a value to a posting.
pub(crate) const POSTING_MIN: u64 = 64;

/// The key of the posting of a value key in `field`, without its chunk. A
/// value key is self-delimiting (fixed-width, or a text ended by its
/// terminator or marker and hash), so no posting's prefix is another's.
pub(super) fn posting_prefix(field: &str, key: &[u8]) -> Vec<u8> {
    [&[POSTINGS][..], &name_key(field), key].concat()
}

/// The key of the descendants of `node` in `field`, without its chunk.
pub(super) fn descendants_prefix(field: &str, node: &[u8; 16]) -> Vec<u8> {
    [&[DESCENDANTS][..], &name_key(field), node].concat()
}

/// The key of the referrers of `target` in `field`, without its chunk.
pub(super) fn referrers_prefix(field: &str, target: &[u8; 16]) -> Vec<u8> {
    [&[REFERRERS][..], &name_key(field), target].concat()
}

/// The two tables of chunked bitmaps, as the transaction's chunk cache
/// tells them apart.
const SETS: u8 = 0;
const GRAMS: u8 = 1;

/// The longest text (lower-cased, in bytes) split in trigrams. A longer one —
/// a note, lyrics — would write a key per trigram on every edit; its id joins
/// the field's long-text set instead, and every search checks it.
pub(crate) const TRIGRAM_MAX: usize = 512;

/// Partitions.
pub(super) const VALUE: u8 = 0;
pub(super) const NAME: u8 = 1;
pub(super) const TARGET: u8 = 2;

const ZERO: [u8; 16] = [0; 16];

/// The distinct trigrams of a lower-cased text (3-byte windows of its UTF-8).
pub(crate) fn trigrams(lower: &str) -> std::collections::BTreeSet<[u8; 3]> {
    lower.as_bytes().windows(3).map(|w| [w[0], w[1], w[2]]).collect()
}

/// The text a value offers the text searches: a string, or a `tree_ref`'s
/// name — lower-cased, as the trigram index holds it.
pub(crate) fn search_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.to_lowercase()),
        Value::TreeRef { name, .. } => Some(name.display().to_lowercase()),
        _ => None,
    }
}

/// The key of a trigram's bitmap, without its chunk.
pub(super) fn gram_prefix(field: &str, gram: &[u8; 3]) -> Vec<u8> {
    [&name_key(field)[..], gram].concat()
}

// ── Keys ────────────────────────────────────────────────────────────────────

/// Bytes inside a composite key, escaped (`00` → `00 FF`) and ended by
/// `00 00`: no escaped string is a prefix of another, and byte order is
/// preserved.
pub(super) fn esc(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 2);
    for &b in bytes {
        out.push(b);
        if b == 0 {
            out.push(0xFF);
        }
    }
    out.extend_from_slice(&[0, 0]);
    out
}

/// The longest escaped text a key holds whole. LMDB refuses a key over 511
/// bytes, and a partition key holds the value: a longer text is keyed by its
/// first bytes (escaped, at most `TEXT_MAX`), the marker `00 01` — which
/// sorts after the `00 00` ending the one text equal to that prefix and
/// before any other — and a hash of the whole. Order is kept except among
/// the long texts sharing a prefix, which a reader resolves from the values
/// themselves ([`long_prefix`]).
pub(crate) const TEXT_MAX: usize = 256;

/// A text's partition key: [`esc`]aped whole, or cut and hashed.
pub(crate) fn text_key(bytes: &[u8]) -> Vec<u8> {
    let whole = esc(bytes);
    if whole.len() <= TEXT_MAX + 2 {
        return whole;
    }
    let mut out = Vec::with_capacity(TEXT_MAX + 10);
    for &b in bytes {
        let width = if b == 0 { 2 } else { 1 };
        if out.len() + width > TEXT_MAX {
            break;
        }
        out.push(b);
        if b == 0 {
            out.push(0xFF);
        }
    }
    out.extend_from_slice(&[0, 1]);
    out.extend_from_slice(&xxhash_rust::xxh3::xxh3_64(bytes).to_be_bytes());
    out
}

/// For a [`text_key`] cut and hashed: its prefix up to and with the marker —
/// what every long text sharing its first bytes starts with. `None` for a
/// text keyed whole.
pub(crate) fn long_prefix(key: &[u8]) -> Option<&[u8]> {
    let mut i = 0;
    while i + 1 < key.len() {
        match (key[i], key[i + 1]) {
            (0, 0) => return None,
            (0, 1) => return Some(&key[..i + 2]),
            (0, _) => i += 2,
            _ => i += 1,
        }
    }
    None
}

/// Reads an [`esc`]aped string at the front of `bytes`: the string and the
/// rest.
pub(super) fn unesc(bytes: &[u8]) -> Result<(Vec<u8>, &[u8])> {
    let mut out = Vec::new();
    let mut i = 0;
    loop {
        match (bytes.get(i), bytes.get(i + 1)) {
            (Some(0), Some(0)) => return Ok((out, &bytes[i + 2..])),
            (Some(0), Some(0xFF)) => {
                out.push(0);
                i += 2;
            }
            (Some(&b), _) => {
                out.push(b);
                i += 1;
            }
            (None, _) => anyhow::bail!("an unterminated key string"),
        }
    }
}

/// The key a value sorts by in the value partition: a type tag in the order
/// of the sort's type groups (bool, number, string, date, references, tree),
/// then bytes that sort like the value. `None` for `Nothing`, which no
/// partition holds.
pub(crate) fn value_key(value: &Value) -> Option<Vec<u8>> {
    let tagged = |tag: u8, bytes: &[u8]| [&[tag][..], bytes].concat();
    Some(match value {
        Value::Nothing => return None,
        Value::Bool(b) => vec![0, *b as u8],
        Value::Int(n) => tagged(1, &num_key(*n as f64).to_be_bytes()),
        Value::Float(f) => tagged(1, &num_key(*f).to_be_bytes()),
        Value::String(s) => tagged(2, &text_key(s.as_bytes())),
        Value::DateTime(ms) => tagged(3, &dt_key(*ms).to_be_bytes()),
        Value::Ref(u) => tagged(4, u.as_bytes()),
        Value::RefBase(u) => tagged(5, u.as_bytes()),
        Value::ExternalRef { repo, metarecord } => {
            tagged(6, &[&metarecord.as_bytes()[..], &repo.as_bytes()[..]].concat())
        }
        Value::TreeRef { parent, name } => {
            let parent = parent.map_or(ZERO, |p| *p.as_bytes());
            tagged(7, &[&parent[..], &text_key(name.as_bytes())].concat())
        }
    })
}

/// The key of a `tree_ref` value in the name partition.
pub(crate) fn name_part_key(value: &Value) -> Option<Vec<u8>> {
    match value {
        Value::TreeRef { name, .. } => Some(text_key(name.display().as_bytes())),
        _ => None,
    }
}

/// The uuid a value points at — a `ref`'s referent, a `tree_ref`'s parent
/// (the zero uuid for a root) — which is what `Follows` matches.
pub(crate) fn target_of(value: &Value) -> Option<[u8; 16]> {
    match value {
        Value::Ref(u) => Some(*u.as_bytes()),
        Value::TreeRef { parent, .. } => Some(parent.map_or(ZERO, |p| *p.as_bytes())),
        _ => None,
    }
}

pub(super) fn set_key(kind: u8, field: Option<&str>, chunk: u16) -> Vec<u8> {
    let mut k = vec![kind];
    if let Some(field) = field {
        k.extend_from_slice(&name_key(field));
    }
    k.extend_from_slice(&chunk.to_be_bytes());
    k
}

/// The prefix of every chunk of a set.
pub(super) fn set_prefix(kind: u8, field: Option<&str>) -> Vec<u8> {
    let mut k = set_key(kind, field, 0);
    k.truncate(k.len() - 2);
    k
}

pub(super) fn part_prefix(field: &str, part: u8) -> Vec<u8> {
    let mut k = name_key(field);
    k.push(part);
    k
}

fn part_key(field: &str, part: u8, key: &[u8], id: u32) -> Vec<u8> {
    [&part_prefix(field, part)[..], key, &id.to_be_bytes()].concat()
}

fn kids_key(parent: &[u8; 16], field: &str, id: u32) -> Vec<u8> {
    [&parent[..], &name_key(field), &id.to_be_bytes()].concat()
}

pub(super) fn dense(b: &[u8]) -> u32 {
    u32::from_be_bytes(b[..4].try_into().expect("a 4-byte id"))
}

pub(super) fn decode_set(bytes: &[u8]) -> Result<RoaringBitmap> {
    RoaringBitmap::deserialize_from(bytes).context("a derived set chunk")
}

// ── Reading ─────────────────────────────────────────────────────────────────

/// A whole set: the union of its chunks.
pub(super) fn read_set(
    t: &Tables,
    r: &RoTxn<'_>,
    kind: u8,
    field: Option<&str>,
) -> Result<RoaringBitmap> {
    let prefix = set_prefix(kind, field);
    let mut out = RoaringBitmap::new();
    for entry in t.sets.prefix_iter(r, &prefix)? {
        let (k, v) = entry?;
        // The field name's terminator keeps a longer name out of the prefix;
        // only the 2-byte chunk follows it.
        if k.len() == prefix.len() + 2 {
            out |= decode_set(v)?;
        }
    }
    Ok(out)
}

pub(super) fn id_of(t: &Tables, r: &RoTxn<'_>, uuid: &[u8]) -> Result<Option<u32>> {
    Ok(t.ids.get(r, uuid)?.map(dense))
}

// ── Maintenance ─────────────────────────────────────────────────────────────

impl KvTxn<'_> {
    fn id(&self, uuid: &[u8]) -> Result<Option<u32>> {
        id_of(&self.t, &self.txn.borrow(), uuid)
    }

    /// Adds `id` to (or removes it from) a set, in the transaction's chunk
    /// cache: a bulk write touches each chunk many times and writes it once,
    /// at [`Self::flush_sets`].
    fn set_member(&self, kind: u8, field: Option<&str>, id: u32, member: bool) -> Result<()> {
        self.chunk_member(SETS, &set_prefix(kind, field), id, member)
    }

    /// [`Self::set_member`] for a chunked bitmap of either table: `table`
    /// ([`SETS`] or [`GRAMS`]), the key without its chunk.
    fn chunk_member(&self, table: u8, prefix: &[u8], id: u32, member: bool) -> Result<()> {
        self.with_chunk(table, prefix, (id >> 16) as u16, &mut |bm| {
            if member {
                bm.insert(id);
            } else {
                bm.remove(id);
            }
        })
    }

    /// Changes one chunk of a bitmap, in the transaction's cache.
    fn with_chunk(
        &self,
        table: u8,
        prefix: &[u8],
        chunk: u16,
        change: &mut dyn FnMut(&mut RoaringBitmap),
    ) -> Result<()> {
        let db = if table == SETS { self.t.sets } else { self.t.grams };
        let k = [&[table][..], prefix, &chunk.to_be_bytes()].concat();
        let mut cache = self.sets.borrow_mut();
        if !cache.contains_key(&k) {
            let loaded = match db.get(&self.txn.borrow(), &k[1..])? {
                Some(bytes) => decode_set(bytes)?,
                None => RoaringBitmap::new(),
            };
            cache.insert(k.clone(), loaded);
            let whole = [&[table][..], prefix].concat();
            self.cached_chunks.borrow_mut().entry(whole).or_default().insert(chunk);
        }
        change(cache.get_mut(&k).expect("just loaded"));
        Ok(())
    }

    /// A whole bitmap as the transaction sees it: its stored chunks, the
    /// cached ones in their stead.
    fn read_chunked(&self, table: u8, prefix: &[u8]) -> Result<RoaringBitmap> {
        let db = if table == SETS { self.t.sets } else { self.t.grams };
        let whole = [&[table][..], prefix].concat();
        let cached = self.cached_chunks.borrow().get(&whole).cloned().unwrap_or_default();
        let cache = self.sets.borrow();
        let mut out = RoaringBitmap::new();
        for entry in db.prefix_iter(&self.txn.borrow(), prefix)? {
            let (k, v) = entry?;
            if k.len() == prefix.len() + 2 {
                let chunk = u16::from_be_bytes([k[k.len() - 2], k[k.len() - 1]]);
                if !cached.contains(&chunk) {
                    out |= decode_set(v)?;
                }
            }
        }
        for chunk in cached {
            let k = [&whole[..], &chunk.to_be_bytes()].concat();
            if let Some(bm) = cache.get(&k) {
                out |= bm;
            }
        }
        Ok(out)
    }

    /// Adds `ids` to a bitmap, or removes them — a chunk at a time.
    fn chunk_apply(&self, table: u8, prefix: &[u8], ids: &RoaringBitmap, add: bool) -> Result<()> {
        let (Some(lo), Some(hi)) = (ids.min(), ids.max()) else { return Ok(()) };
        for chunk in (lo >> 16)..=(hi >> 16) {
            let mut part = RoaringBitmap::new();
            part.insert_range((chunk << 16)..=((chunk << 16) | 0xFFFF));
            part &= ids;
            if part.is_empty() {
                continue;
            }
            self.with_chunk(table, prefix, chunk as u16, &mut |bm| {
                if add {
                    *bm |= &part;
                } else {
                    *bm -= &part;
                }
            })?;
        }
        Ok(())
    }

    /// Writes the chunks the transaction changed.
    pub(super) fn flush_sets(&self) -> Result<()> {
        let mut w = self.txn.borrow_mut();
        for (k, bm) in self.sets.borrow_mut().drain() {
            let db = if k[0] == SETS { self.t.sets } else { self.t.grams };
            if bm.is_empty() {
                db.delete(&mut w, &k[1..])?;
            } else {
                let mut bytes = Vec::with_capacity(bm.serialized_size());
                bm.serialize_into(&mut bytes)?;
                db.put(&mut w, &k[1..], &bytes)?;
            }
        }
        Ok(())
    }

    /// Whether a chunked bitmap has any member, as the transaction sees it —
    /// without decoding the stored chunks.
    fn chunked_exists(&self, table: u8, prefix: &[u8]) -> Result<bool> {
        let db = if table == SETS { self.t.sets } else { self.t.grams };
        let whole = [&[table][..], prefix].concat();
        let cached = self.cached_chunks.borrow().get(&whole).cloned().unwrap_or_default();
        {
            let cache = self.sets.borrow();
            let key = |chunk: u16| [&whole[..], &chunk.to_be_bytes()].concat();
            if cached.iter().any(|&c| cache.get(&key(c)).is_some_and(|bm| !bm.is_empty())) {
                return Ok(true);
            }
        }
        for entry in db.prefix_iter(&self.txn.borrow(), prefix)? {
            let (k, _) = entry?;
            if k.len() == prefix.len() + 2 {
                let chunk = u16::from_be_bytes([k[k.len() - 2], k[k.len() - 1]]);
                if !cached.contains(&chunk) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// The first `limit` holders of a value key in `field`'s value partition.
    fn value_holders(&self, field: &str, key: &[u8], limit: u64) -> Result<RoaringBitmap> {
        let prefix = [&part_prefix(field, VALUE)[..], key].concat();
        let mut out = RoaringBitmap::new();
        for entry in self.t.parts.prefix_iter(&self.txn.borrow(), &prefix)? {
            let (k, _) = entry?;
            out.insert(dense(&k[prefix.len()..]));
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    /// Keeps `id` in the posting of `key` as it is in the value partition,
    /// promoting the value once it has [`POSTING_MIN`] holders.
    fn derive_posting(&self, field: &str, key: &[u8], id: u32) -> Result<()> {
        let prefix = posting_prefix(field, key);
        let held =
            self.t.parts.get(&self.txn.borrow(), &part_key(field, VALUE, key, id))?.is_some();
        if self.chunked_exists(SETS, &prefix)? {
            return self.chunk_member(SETS, &prefix, id, held);
        }
        if held && self.value_holders(field, key, POSTING_MIN)?.len() >= POSTING_MIN {
            let all = self.value_holders(field, key, u64::MAX)?;
            self.chunk_apply(SETS, &prefix, &all, true)?;
        }
        Ok(())
    }

    /// Adds `delta` to a row count, deleting the key at zero.
    fn bump(&self, db: super::Db, k: &[u8], delta: i64) -> Result<()> {
        let mut w = self.txn.borrow_mut();
        let n = db.get(&w, k)?.map_or(0, from_be) + delta;
        if n <= 0 {
            db.delete(&mut w, k)?;
        } else {
            db.put(&mut w, k, &be(n))?;
        }
        Ok(())
    }

    /// Whether any row of `field` still has `parent` as its parent.
    fn has_kids(&self, parent: &[u8; 16], field: &str) -> Result<bool> {
        let prefix = [&parent[..], &name_key(field)].concat();
        Ok(self.t.kids.prefix_iter(&self.txn.borrow(), &prefix)?.next().is_some())
    }

    /// The fields in which `uuid` has children.
    fn parent_fields(&self, uuid: &[u8; 16]) -> Result<BTreeSet<String>> {
        let mut out = BTreeSet::new();
        for entry in self.t.kids.prefix_iter(&self.txn.borrow(), uuid)? {
            let (k, _) = entry?;
            let (field, _) = unesc(&k[16..])?;
            out.insert(String::from_utf8(field).context("a field name")?);
        }
        Ok(out)
    }

    /// A metarecord was created: it gets the next dense id, in the universe,
    /// and in `parents` wherever rows already hang below it (a restoration
    /// may put a child back before its parent).
    pub(super) fn derive_created(&self, uuid: Uuid) -> Result<()> {
        let id = u32::try_from(self.next("next_id")?).context("dense ids exhausted")?;
        {
            let mut w = self.txn.borrow_mut();
            self.t.ids.put(&mut w, uuid.as_bytes(), &id.to_be_bytes())?;
            self.t.uuids.put(&mut w, &id.to_be_bytes(), uuid.as_bytes())?;
        }
        self.set_member(UNIVERSE, None, id, true)?;
        for field in self.parent_fields(uuid.as_bytes())? {
            self.set_member(PARENTS, Some(&field), id, true)?;
        }
        Ok(())
    }

    /// A metarecord was removed (its rows first): its id leaves everything.
    pub(super) fn derive_removed(&self, uuid: Uuid) -> Result<()> {
        let Some(id) = self.id(uuid.as_bytes())? else { return Ok(()) };
        {
            let mut w = self.txn.borrow_mut();
            self.t.ids.delete(&mut w, uuid.as_bytes())?;
            self.t.uuids.delete(&mut w, &id.to_be_bytes())?;
        }
        self.set_member(UNIVERSE, None, id, false)?;
        for field in self.parent_fields(uuid.as_bytes())? {
            self.set_member(PARENTS, Some(&field), id, false)?;
        }
        Ok(())
    }

    /// A row of `uuid` was inserted (`delta` 1) or deleted (−1) — after the
    /// primary write, so the metarecord's remaining rows are current.
    pub(super) fn derive_row(
        &self,
        uuid: Uuid,
        field: &str,
        value: &Value,
        delta: i64,
    ) -> Result<()> {
        let id = self.id(uuid.as_bytes())?.with_context(|| format!("no dense id for {uuid}"))?;
        if let Some(k) = value_key(value) {
            self.bump(self.t.parts, &part_key(field, VALUE, &k, id), delta)?;
            self.derive_posting(field, &k, id)?;
        }
        if let Some(k) = name_part_key(value) {
            self.bump(self.t.parts, &part_key(field, NAME, &k, id), delta)?;
        }
        if let Some(target) = target_of(value) {
            self.bump(self.t.parts, &part_key(field, TARGET, &target, id), delta)?;
        }
        if let Value::TreeRef { .. } = value {
            let parent = target_of(value).expect("a tree_ref has a parent key");
            self.bump(self.t.kids, &kids_key(&parent, field, id), delta)?;
            if let Some(pid) = self.id(&parent)? {
                let member = delta > 0 || self.has_kids(&parent, field)?;
                self.set_member(PARENTS, Some(field), pid, member)?;
            }
        }
        let rows = Rows::rows_named(self, uuid, field)?;
        let present = rows.iter().any(|r| !matches!(r.value, Value::Nothing));
        let absent = rows.iter().any(|r| matches!(r.value, Value::Nothing));
        self.set_member(PRESENT, Some(field), id, present)?;
        self.set_member(ABSENT, Some(field), id, absent)?;
        let valued = rows.iter().filter(|r| !matches!(r.value, Value::Nothing)).count();
        self.set_member(MULTI, Some(field), id, valued > 1)?;
        // Trigrams: the changed row's, held by the id while one of its
        // remaining texts still has them.
        if let Some(text) = search_text(value).filter(|t| t.len() <= TRIGRAM_MAX) {
            let texts: Vec<String> = rows.iter().filter_map(|r| search_text(&r.value)).collect();
            let held: std::collections::BTreeSet<[u8; 3]> =
                texts.iter().filter(|t| t.len() <= TRIGRAM_MAX).flat_map(|t| trigrams(t)).collect();
            for gram in trigrams(&text) {
                self.chunk_member(GRAMS, &gram_prefix(field, &gram), id, held.contains(&gram))?;
            }
        }
        if search_text(value).is_some() {
            let long =
                rows.iter().filter_map(|r| search_text(&r.value)).any(|t| t.len() > TRIGRAM_MAX);
            self.set_member(LONG_TEXTS, Some(field), id, long)?;
        }
        if let Some(target) = target_of(value) {
            let held = rows.iter().any(|r| target_of(&r.value) == Some(target));
            self.chunk_member(SETS, &referrers_prefix(field, &target), id, held)?;
        }
        if let Value::TreeRef { parent: Some(parent), .. } = value {
            self.derive_descendants(uuid, id, field, *parent, delta > 0)?;
        }
        Ok(())
    }

    /// A position of `uuid` under `parent` in a forest came (`add`) or
    /// went: the node and everything below it join, or leave, the descendants
    /// of `parent` and of each of its ancestors — a node there holds one
    /// position, so the chain up is its one path.
    fn derive_descendants(
        &self,
        uuid: Uuid,
        id: u32,
        field: &str,
        parent: Uuid,
        add: bool,
    ) -> Result<()> {
        let mut moving = self.read_chunked(SETS, &descendants_prefix(field, uuid.as_bytes()))?;
        moving.insert(id);
        let mut up = Some(parent);
        for _ in 0..crate::log::MAX_TREE_DEPTH {
            let Some(ancestor) = up else { return Ok(()) };
            self.chunk_apply(SETS, &descendants_prefix(field, ancestor.as_bytes()), &moving, add)?;
            up = Rows::rows_named(self, ancestor, field)?.into_iter().find_map(|r| match r.value {
                Value::TreeRef { parent, .. } => parent,
                _ => None,
            });
        }
        anyhow::bail!("a tree deeper than {} in '{field}'", crate::log::MAX_TREE_DEPTH)
    }

    /// Empties every derived table (the primary data was cleared, or is about
    /// to be derived again).
    pub(super) fn clear_derived(&self) -> Result<()> {
        self.sets.borrow_mut().clear();
        self.cached_chunks.borrow_mut().clear();
        let mut w = self.txn.borrow_mut();
        let t = self.t;
        for db in [t.ids, t.uuids, t.sets, t.parts, t.kids, t.grams] {
            db.clear(&mut w)?;
        }
        Ok(())
    }
}

// ── Rebuilding and checking ─────────────────────────────────────────────────

impl KvStore {
    /// Derives every derived key space again from the primary data, in one
    /// transaction. Dense ids follow discovery order: the order of each
    /// metarecord's first row (then the metarecords without rows).
    pub fn reindex(&mut self) -> Result<()> {
        let txn = KvTxn::new(self)?;
        txn.clear_derived()?;
        txn.meta_put("next_id", 1)?;
        let order = {
            let r = txn.txn.borrow();
            let mut seen = BTreeSet::new();
            let mut order = Vec::new();
            for entry in txn.t.row_owner.iter(&r)? {
                let u = uuid_of(entry?.1);
                if seen.insert(u) {
                    order.push(u);
                }
            }
            for entry in txn.t.metarecords.iter(&r)? {
                let u = uuid_of(entry?.0);
                if seen.insert(u) {
                    order.push(u);
                }
            }
            order
        };
        for &uuid in &order {
            txn.derive_created(uuid)?;
        }
        for &uuid in &order {
            for row in Rows::rows(&txn, uuid)? {
                txn.derive_row(uuid, &row.name, &row.value, 1)?;
            }
        }
        txn.meta_put("derived", DERIVED_VERSION)?;
        txn.finish()
    }

    /// Where the derived key spaces differ from what the primary data
    /// derives (at most a hundred lines; empty when they agree). Ids are
    /// compared through the store's own id mapping, itself checked against
    /// the metarecords, so a store whose ids were allocated in another order
    /// still agrees.
    ///
    /// The comparison holds neither side: a store's derived data is larger
    /// than the memory of the machine that checks it (every trigram of every
    /// text, per holder). Both sides are *streamed* into fingerprints — an
    /// order-free sum of entry hashes, per key space and per bucket of
    /// [`BUCKETS`] — and only where two fingerprints differ is a bucket's
    /// content collected, in a second pass, to name its entries. What stays
    /// resident is what the forest's closure needs (a parent per node) and
    /// one record's field at a time.
    pub fn check_derived(&self) -> Result<Vec<String>> {
        let r = self.env.read_txn()?;
        let mut diff = Diff::default();

        let mut sums = Sums::new();
        self.walk_entries(&r, &mut diff, &mut |side, entry| {
            sums.add(side, &entry);
            Ok(())
        })?;
        let wanted = sums.differing();
        if wanted.iter().any(|buckets| !buckets.is_empty()) {
            let mut detail = Detail::new(wanted);
            // The first pass already said what it found outside the entries.
            let mut said = Diff::default();
            self.walk_entries(&r, &mut said, &mut |side, entry| {
                detail.add(&self.t, &r, side, &entry)
            })?;
            detail.report(&mut diff);
        }
        self.check_postings(&r, &mut diff)?;
        Ok(diff.lines)
    }

    /// Every entry the primary data derives ([`Side::Expected`]) and every
    /// entry the derived tables hold ([`Side::Got`]), postings aside, handed
    /// to `sink` one at a time. What cannot be an entry — a metarecord with
    /// no dense id, an id the two mappings disagree on — goes to `diff`.
    fn walk_entries(
        &self,
        r: &RoTxn<'_>,
        diff: &mut Diff,
        sink: &mut dyn FnMut(Side, Entry<'_>) -> Result<()>,
    ) -> Result<()> {
        let t = &self.t;

        // The universe and the id mappings.
        let mut metarecords = 0u64;
        for entry in t.metarecords.iter(r)? {
            let (k, _) = entry?;
            metarecords += 1;
            match id_of(t, r, &k[..16])? {
                Some(id) => sink(Side::Expected, Entry::set(UNIVERSE, "", id))?,
                None => diff.push(format!("ids: {} has no dense id", uuid_of(k))),
            }
        }
        let mut mapped = 0u64;
        for entry in t.ids.iter(r)? {
            let (k, v) = entry?;
            mapped += 1;
            let back = t.uuids.get(r, &v[..4])?;
            if back.map(|u| &u[..16]) != Some(&k[..16]) {
                diff.push(format!(
                    "ids: {} -> {}, but uuids says {:?}",
                    uuid_of(k),
                    dense(v),
                    back.map(uuid_of)
                ));
            }
        }
        if mapped != metarecords {
            diff.push(format!("ids: map {mapped} metarecords, the store holds {metarecords}"));
        }

        // Expected, from the rows — which the primary table keeps grouped by
        // metarecord and field, so everything a (metarecord, field) derives
        // is known when its last row is read.
        let mut group = Group::default();
        // Per field: the ids with a child, and each node's parent (with the
        // node's own id), for the closure below.
        let mut parents: BTreeMap<String, RoaringBitmap> = BTreeMap::new();
        let mut fields: HashMap<String, u32> = HashMap::new();
        let mut parent_of: HashMap<(u32, Uuid), (Uuid, Option<u32>)> = HashMap::new();
        let mut owner: Option<([u8; 16], Option<u32>)> = None;
        for entry in t.cells.iter(r)? {
            let (k, v) = entry?;
            let uuid: [u8; 16] = k[..16].try_into().expect("a 16-byte uuid");
            let row = dec_row(v)?;
            let id = match owner {
                Some((u, id)) if u == uuid => id,
                _ => {
                    let id = id_of(t, r, &uuid)?;
                    if id.is_none() {
                        diff.push(format!("rows of {}: no dense id", Uuid::from_bytes(uuid)));
                    }
                    owner = Some((uuid, id));
                    id
                }
            };
            if group.uuid != uuid || group.field != row.name {
                group.flush(sink)?;
                group = Group { uuid, id, field: row.name.clone(), ..Group::default() };
            }
            if let Some(key) = value_key(&row.value) {
                *group.parts.entry((VALUE, key)).or_default() += 1;
            }
            if let Some(key) = name_part_key(&row.value) {
                *group.parts.entry((NAME, key)).or_default() += 1;
            }
            if let Some(target) = target_of(&row.value) {
                *group.parts.entry((TARGET, target.to_vec())).or_default() += 1;
                group.targets.insert(target);
            }
            if let Value::TreeRef { parent, .. } = &row.value {
                let target = target_of(&row.value).expect("a tree_ref has a parent key");
                *group.kids.entry(target).or_default() += 1;
                if let Some(parent_id) = id_of(t, r, &target)? {
                    match parents.get_mut(&row.name) {
                        Some(set) => {
                            set.insert(parent_id);
                        }
                        None => {
                            parents.insert(row.name.clone(), RoaringBitmap::from_iter([parent_id]));
                        }
                    }
                }
                if let Some(p) = parent {
                    let next = fields.len() as u32;
                    let field = *fields.entry(row.name.clone()).or_insert(next);
                    parent_of.insert((field, Uuid::from_bytes(uuid)), (*p, id));
                }
            }
            if let Some(text) = search_text(&row.value) {
                if text.len() <= TRIGRAM_MAX {
                    group.grams.extend(trigrams(&text));
                } else {
                    group.long = true;
                }
            }
            if matches!(row.value, Value::Nothing) {
                group.absent = true;
            } else {
                group.valued += 1;
            }
        }
        group.flush(sink)?;
        for (field, set) in &parents {
            for id in set {
                sink(Side::Expected, Entry::set(PARENTS, field, id))?;
            }
        }
        let names: HashMap<u32, &str> =
            fields.iter().map(|(name, i)| (*i, name.as_str())).collect();
        for ((field, _), (first, id)) in &parent_of {
            let Some(id) = *id else { continue };
            let mut up = Some(*first);
            for _ in 0..crate::log::MAX_TREE_DEPTH {
                let Some(ancestor) = up else { break };
                let entry = Entry {
                    space: DESCENDANT,
                    tag: 0,
                    field: names[field],
                    key: ancestor.as_bytes(),
                    id,
                    count: 1,
                };
                sink(Side::Expected, entry)?;
                up = parent_of.get(&(*field, ancestor)).map(|(parent, _)| *parent);
            }
        }
        drop(parent_of);

        // Got: the derived tables, as they are.
        for entry in t.sets.iter(r)? {
            let (k, v) = entry?;
            let kind = k[0];
            if kind == POSTINGS {
                continue; // not an equality: see `check_postings`
            }
            let (name, rest) =
                if kind == UNIVERSE { (Vec::new(), &k[1..]) } else { unesc(&k[1..])? };
            let field = std::str::from_utf8(&name).context("a field name")?;
            let (space, tag, key) = match kind {
                REFERRERS => (REFERRER, 0, &rest[..16]),
                DESCENDANTS => (DESCENDANT, 0, &rest[..16]),
                _ => (SET, kind, &rest[..0]),
            };
            for id in decode_set(v)? {
                sink(Side::Got, Entry { space, tag, field, key, id, count: 1 })?;
            }
        }
        for entry in t.parts.iter(r)? {
            let (k, v) = entry?;
            let (name, rest) = unesc(k)?;
            let entry = Entry {
                space: PART,
                tag: rest[0],
                field: std::str::from_utf8(&name).context("a field name")?,
                key: &rest[1..rest.len() - 4],
                id: dense(&rest[rest.len() - 4..]),
                count: from_be(v),
            };
            sink(Side::Got, entry)?;
        }
        for entry in t.kids.iter(r)? {
            let (k, v) = entry?;
            let (name, rest) = unesc(&k[16..])?;
            let entry = Entry {
                space: KID,
                tag: 0,
                field: std::str::from_utf8(&name).context("a field name")?,
                key: &k[..16],
                id: dense(rest),
                count: from_be(v),
            };
            sink(Side::Got, entry)?;
        }
        for entry in t.grams.iter(r)? {
            let (k, v) = entry?;
            let (name, rest) = unesc(k)?;
            let field = std::str::from_utf8(&name).context("a field name")?;
            for id in decode_set(v)? {
                sink(
                    Side::Got,
                    Entry { space: GRAM, tag: 0, field, key: &rest[..3], id, count: 1 },
                )?;
            }
        }
        Ok(())
    }

    /// Postings: required from the threshold on, and exact wherever kept (a
    /// value grown rarer keeps its posting). Held against the value
    /// partition, where a value's holders are adjacent — and which
    /// [`Self::walk_entries`] holds against the rows.
    fn check_postings(&self, r: &RoTxn<'_>, diff: &mut Diff) -> Result<()> {
        let t = &self.t;
        // `(field, value key)`, as the lines name a value.
        let value = |name_and_key: &[u8]| -> Result<(String, Vec<u8>)> {
            let (name, key) = unesc(name_and_key)?;
            Ok((String::from_utf8(name).context("a field name")?, key.to_vec()))
        };

        // Each posting kept, against its value's holders.
        let exact = |prefix: &[u8], got: &RoaringBitmap, diff: &mut Diff| -> Result<()> {
            let (field, key) = value(&prefix[1..])?;
            let run = [&name_key(&field)[..], &[VALUE], &key].concat();
            let mut holders = RoaringBitmap::new();
            for entry in t.parts.prefix_iter(r, &run)? {
                let (k, _) = entry?;
                if k.len() == run.len() + 4 {
                    holders.insert(dense(&k[run.len()..]));
                }
            }
            if holders.is_empty() {
                diff.push(format!("posting unexpected: {:?} (no holder)", (field, key)));
            } else if holders != *got {
                diff.push(format!(
                    "posting of {:?}: {} ids, the value has {} holders",
                    (field, key),
                    got.len(),
                    holders.len()
                ));
            }
            Ok(())
        };
        let mut posting: Option<(Vec<u8>, RoaringBitmap)> = None;
        for entry in t.sets.prefix_iter(r, &[POSTINGS])? {
            let (k, v) = entry?;
            let prefix = &k[..k.len() - 2];
            if posting.as_ref().is_some_and(|(p, _)| p != prefix) {
                let (p, got) = posting.take().expect("a posting");
                exact(&p, &got, diff)?;
            }
            posting.get_or_insert_with(|| (prefix.to_vec(), RoaringBitmap::new())).1 |=
                decode_set(v)?;
        }
        if let Some((p, got)) = posting {
            exact(&p, &got, diff)?;
        }

        // Each value held often enough, against the postings.
        let required = |run: &[u8], holders: u64, diff: &mut Diff| -> Result<()> {
            if holders < POSTING_MIN {
                return Ok(());
            }
            let (name, rest) = unesc(run)?;
            if rest[0] != VALUE {
                return Ok(());
            }
            let name_len = run.len() - rest.len();
            let prefix = [&[POSTINGS][..], &run[..name_len], &rest[1..]].concat();
            let mut chunks = t.sets.prefix_iter(r, &prefix)?;
            if chunks.next().transpose()?.is_none() {
                let field = String::from_utf8(name).context("a field name")?;
                diff.push(format!(
                    "posting missing: {:?} ({holders} holders)",
                    (field, rest[1..].to_vec())
                ));
            }
            Ok(())
        };
        let mut run: (Vec<u8>, u64) = (Vec::new(), 0);
        for entry in t.parts.iter(r)? {
            let (k, _) = entry?;
            let prefix = &k[..k.len() - 4];
            if run.0 != prefix {
                if run.1 > 0 {
                    required(&run.0, run.1, diff)?;
                }
                run = (prefix.to_vec(), 0);
            }
            run.1 += 1;
        }
        if run.1 > 0 {
            required(&run.0, run.1, diff)?;
        }
        Ok(())
    }
}

/// The lines of a check, capped: a store that diverges everywhere must not
/// cost its size in messages.
#[derive(Default)]
struct Diff {
    lines: Vec<String>,
}

impl Diff {
    const MAX: usize = 100;

    fn push(&mut self, line: String) {
        if self.lines.len() < Self::MAX {
            self.lines.push(line);
        }
    }
}

/// The key spaces [`KvStore::check_derived`] compares entry by entry, by the
/// name its lines give them.
const SPACES: [&str; 6] = ["set", "referrer", "descendant", "part", "kid", "gram"];
const SET: usize = 0;
const REFERRER: usize = 1;
const DESCENDANT: usize = 2;
const PART: usize = 3;
const KID: usize = 4;
const GRAM: usize = 5;

/// How many buckets each key space's entries are spread over. A divergence
/// costs the content of the buckets it falls in — a 4096th of the key space
/// each — and not the key space.
const BUCKETS: usize = 4096;

/// How many differing buckets of a key space are detailed: enough to fill the
/// report, without collecting a key space that diverges everywhere.
const DETAILED: usize = 4;

#[derive(Clone, Copy, PartialEq)]
enum Side {
    Expected,
    Got,
}

/// One entry of a derived key space, in the one form both sides produce:
/// what derives it on one, the key that holds it on the other.
#[derive(Clone, Copy)]
struct Entry<'a> {
    space: usize,
    /// The set kind, or the partition.
    tag: u8,
    field: &'a str,
    /// A partition key, a trigram, or the uuid a referrer, a descendant or a
    /// child is filed under.
    key: &'a [u8],
    id: u32,
    /// Rows, where the key space counts them; 1 elsewhere.
    count: i64,
}

impl<'a> Entry<'a> {
    fn set(kind: u8, field: &'a str, id: u32) -> Entry<'a> {
        Entry { space: SET, tag: kind, field, key: &[], id, count: 1 }
    }

    /// A hash of the whole entry; `buf` is scratch space.
    fn hash(&self, buf: &mut Vec<u8>) -> u128 {
        buf.clear();
        buf.push(self.space as u8);
        buf.push(self.tag);
        buf.extend_from_slice(&(self.field.len() as u32).to_be_bytes());
        buf.extend_from_slice(self.field.as_bytes());
        buf.extend_from_slice(self.key);
        buf.extend_from_slice(&self.id.to_be_bytes());
        buf.extend_from_slice(&self.count.to_be_bytes());
        xxhash_rust::xxh3::xxh3_128(buf)
    }

    /// The entry as a line names it, its id resolved to the metarecord.
    fn show(&self, t: &Tables, r: &RoTxn<'_>) -> Result<String> {
        let who = match t.uuids.get(r, &self.id.to_be_bytes())? {
            Some(uuid) => uuid_of(uuid).to_string(),
            None => format!("dense id {} (no metarecord)", self.id),
        };
        let field = self.field;
        Ok(match self.space {
            SET => format!("({}, {field:?}, {who})", self.tag),
            REFERRER | DESCENDANT => format!("({field:?}, {}, {who})", uuid_of(self.key)),
            PART => format!("({field:?}, {}, {:?}, {who}): {}", self.tag, self.key, self.count),
            KID => format!("({}, {field:?}, {who}): {}", uuid_of(self.key), self.count),
            _ => format!("({field:?}, {:?}, {who})", String::from_utf8_lossy(self.key)),
        })
    }
}

/// Everything one field of one metarecord derives.
#[derive(Default)]
struct Group {
    uuid: [u8; 16],
    id: Option<u32>,
    field: String,
    parts: BTreeMap<(u8, Vec<u8>), i64>,
    kids: BTreeMap<[u8; 16], i64>,
    grams: BTreeSet<[u8; 3]>,
    targets: BTreeSet<[u8; 16]>,
    /// Non-`Nothing` rows.
    valued: usize,
    absent: bool,
    long: bool,
}

impl Group {
    fn flush(&self, sink: &mut dyn FnMut(Side, Entry<'_>) -> Result<()>) -> Result<()> {
        // No id: nothing in the derived tables can name this metarecord, and
        // the walk has said so.
        let Some(id) = self.id else { return Ok(()) };
        let field = self.field.as_str();
        let mut emit = |space, tag, key: &[u8], count| {
            sink(Side::Expected, Entry { space, tag, field, key, id, count })
        };
        for ((part, key), rows) in &self.parts {
            emit(PART, *part, key, *rows)?;
        }
        for (parent, rows) in &self.kids {
            emit(KID, 0, parent, *rows)?;
        }
        for gram in &self.grams {
            emit(GRAM, 0, gram, 1)?;
        }
        for target in &self.targets {
            emit(REFERRER, 0, target, 1)?;
        }
        let kinds = [
            (PRESENT, self.valued > 0),
            (ABSENT, self.absent),
            (MULTI, self.valued > 1),
            (LONG_TEXTS, self.long),
        ];
        for (kind, holds) in kinds {
            if holds {
                emit(SET, kind, &[], 1)?;
            }
        }
        Ok(())
    }
}

/// Both sides' fingerprints: per key space and bucket, how many entries and
/// the sum of their hashes — equal whatever the order they came in.
struct Sums {
    sides: [Vec<(u64, u128)>; 2],
    buf: Vec<u8>,
}

impl Sums {
    fn new() -> Sums {
        let empty = || vec![(0, 0); SPACES.len() * BUCKETS];
        Sums { sides: [empty(), empty()], buf: Vec::new() }
    }

    fn add(&mut self, side: Side, entry: &Entry<'_>) {
        let hash = entry.hash(&mut self.buf);
        let slot = &mut self.sides[side as usize][entry.space * BUCKETS + bucket(hash)];
        slot.0 += 1;
        slot.1 = slot.1.wrapping_add(hash);
    }

    /// Per key space, the first buckets whose two sides differ.
    fn differing(&self) -> Vec<Vec<usize>> {
        (0..SPACES.len())
            .map(|space| {
                (0..BUCKETS)
                    .filter(|b| {
                        self.sides[0][space * BUCKETS + b] != self.sides[1][space * BUCKETS + b]
                    })
                    .take(DETAILED)
                    .collect()
            })
            .collect()
    }
}

fn bucket(hash: u128) -> usize {
    (hash % BUCKETS as u128) as usize
}

/// The second pass: the entries of the buckets that differ, as text.
struct Detail {
    wanted: Vec<Vec<usize>>,
    sides: [Vec<BTreeSet<String>>; 2],
    buf: Vec<u8>,
}

impl Detail {
    fn new(wanted: Vec<Vec<usize>>) -> Detail {
        let empty = || vec![BTreeSet::new(); SPACES.len()];
        Detail { wanted, sides: [empty(), empty()], buf: Vec::new() }
    }

    fn add(&mut self, t: &Tables, r: &RoTxn<'_>, side: Side, entry: &Entry<'_>) -> Result<()> {
        if self.wanted[entry.space].contains(&bucket(entry.hash(&mut self.buf))) {
            self.sides[side as usize][entry.space].insert(entry.show(t, r)?);
        }
        Ok(())
    }

    /// The entries one side holds and the other does not (or holds otherwise).
    fn report(&self, diff: &mut Diff) {
        // Referrers and descendants first, then the sets, as the lines have
        // always come.
        for space in [REFERRER, DESCENDANT, SET, PART, KID, GRAM] {
            let (expected, got) = (&self.sides[0][space], &self.sides[1][space]);
            let what = SPACES[space];
            for e in expected.difference(got) {
                diff.push(format!("{what} missing: {e}"));
            }
            for e in got.difference(expected) {
                diff.push(format!("{what} unexpected: {e}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use metafolder_core::metarecord::{Field, TreeName, Value};

    use super::*;
    use crate::kvstore::test_store;
    use crate::log::Writer;

    /// A root, a folder and files under it, with texts and a value frequent
    /// enough to have a posting.
    fn populated() -> (KvStore, crate::kvstore::TestDir) {
        let (mut store, dir) = test_store();
        let tree = |parent, name: &str| Value::TreeRef { parent, name: TreeName::from(name) };
        let mut w = Writer::begin(&mut store, None).unwrap();
        let root = w.create_metarecord(vec![Field::new("loc", tree(None, ""))]).unwrap().uuid;
        let folder =
            w.create_metarecord(vec![Field::new("loc", tree(Some(root), "dir"))]).unwrap().uuid;
        for i in 0..(POSTING_MIN as usize + 6) {
            w.create_metarecord(vec![
                Field::new("loc", tree(Some(folder), &format!("file{i}.txt"))),
                Field::new("kind", Value::String("text".into())),
                Field::new("title", Value::String(format!("title number {i}"))),
                Field::new("link", Value::Ref(folder)),
            ])
            .unwrap();
        }
        w.commit().unwrap();
        (store, dir)
    }

    /// Applies `tamper` to the tables, behind the store's back.
    fn tamper(store: &KvStore, tamper: impl FnOnce(&Tables, &mut heed::RwTxn<'_>)) {
        let mut w = store.env.write_txn().unwrap();
        tamper(&store.t, &mut w);
        w.commit().unwrap();
    }

    fn first_key(
        store: &KvStore,
        db: impl Fn(&Tables) -> &super::super::Db,
        prefix: &[u8],
    ) -> Vec<u8> {
        let r = store.env.read_txn().unwrap();
        let (k, _) = db(&store.t).prefix_iter(&r, prefix).unwrap().next().unwrap().unwrap();
        k.to_vec()
    }

    /// The first lines of a diff, for a failure message.
    fn some(diff: &[String]) -> String {
        diff.iter().take(5).cloned().collect::<Vec<_>>().join("\n")
    }

    fn has(diff: &[String], start: &str) -> bool {
        diff.iter().any(|line| line.starts_with(start))
    }

    #[test]
    fn test_a_healthy_store_checks_clean() {
        let (store, _dir) = populated();
        assert_eq!(store.check_derived().unwrap(), Vec::<String>::new());
    }

    #[test]
    fn test_a_lost_trigram_chunk_is_named() {
        let (store, _dir) = populated();
        let key = first_key(&store, |t| &t.grams, &gram_prefix("title", b"num"));
        tamper(&store, |t, w| {
            t.grams.delete(w, &key).unwrap();
        });
        let diff = store.check_derived().unwrap();
        assert!(has(&diff, "gram missing: "), "{}", some(&diff));
        assert!(!has(&diff, "gram unexpected: "), "{}", some(&diff));
        assert!(diff.iter().all(|l| l.starts_with("gram ")), "only the trigrams differ: {diff:?}");
    }

    #[test]
    fn test_a_partition_entry_nothing_derives_is_named() {
        let (store, _dir) = populated();
        let existing = first_key(&store, |t| &t.parts, &name_key("title"));
        let id = &existing[existing.len() - 4..];
        let bogus = [&name_key("title")[..], &[VALUE], &value_key(&Value::Bool(true)).unwrap(), id]
            .concat();
        tamper(&store, |t, w| {
            t.parts.put(w, &bogus, &be(1)).unwrap();
        });
        let diff = store.check_derived().unwrap();
        assert!(has(&diff, "part unexpected: "), "{}", some(&diff));
        assert!(!has(&diff, "part missing: "), "{}", some(&diff));
    }

    #[test]
    fn test_a_wrong_row_count_is_named_on_both_sides() {
        let (store, _dir) = populated();
        let key = {
            let r = store.env.read_txn().unwrap();
            let (k, _) = store.t.kids.iter(&r).unwrap().next().unwrap().unwrap();
            k.to_vec()
        };
        tamper(&store, |t, w| {
            t.kids.put(w, &key, &be(7)).unwrap();
        });
        let diff = store.check_derived().unwrap();
        assert!(has(&diff, "kid missing: ") && has(&diff, "kid unexpected: "), "{}", some(&diff));
    }

    #[test]
    fn test_a_lost_set_chunk_is_named_by_its_key_space() {
        let cases: [(&str, Vec<u8>); 3] = [
            ("set missing: ", set_prefix(PRESENT, Some("kind"))),
            ("descendant missing: ", vec![DESCENDANTS]),
            ("referrer missing: ", vec![REFERRERS]),
        ];
        for (expected, prefix) in cases {
            let (store, _dir) = populated();
            let key = first_key(&store, |t| &t.sets, &prefix);
            tamper(&store, |t, w| {
                t.sets.delete(w, &key).unwrap();
            });
            let diff = store.check_derived().unwrap();
            assert!(
                !diff.is_empty() && diff.iter().all(|l| l.starts_with(expected)),
                "{}",
                some(&diff)
            );
        }
    }

    #[test]
    fn test_postings_are_required_and_exact() {
        let (store, _dir) = populated();
        let posting = posting_prefix("kind", &value_key(&Value::String("text".into())).unwrap());
        let key = first_key(&store, |t| &t.sets, &posting);

        // Inexact: one holder short.
        let short = {
            let r = store.env.read_txn().unwrap();
            let mut set = decode_set(store.t.sets.get(&r, &key).unwrap().unwrap()).unwrap();
            set.remove(set.max().unwrap());
            let mut bytes = Vec::new();
            set.serialize_into(&mut bytes).unwrap();
            bytes
        };
        tamper(&store, |t, w| {
            t.sets.put(w, &key, &short).unwrap();
        });
        let diff = store.check_derived().unwrap();
        assert!(has(&diff, "posting of "), "{}", some(&diff));

        // Gone, for a value over the threshold.
        tamper(&store, |t, w| {
            t.sets.delete(w, &key).unwrap();
        });
        let diff = store.check_derived().unwrap();
        assert!(has(&diff, "posting missing: "), "{}", some(&diff));

        // Kept for a value nothing holds.
        let orphan =
            [&posting_prefix("kind", &value_key(&Value::Bool(true)).unwrap())[..], &[0, 0]]
                .concat();
        tamper(&store, |t, w| {
            t.sets.put(w, &key, &short).unwrap();
            t.sets.put(w, &orphan, &short).unwrap();
        });
        let diff = store.check_derived().unwrap();
        assert!(has(&diff, "posting unexpected: "), "{}", some(&diff));
    }

    #[test]
    fn test_an_id_naming_no_metarecord_is_reported() {
        let (store, _dir) = populated();
        let mut stray = RoaringBitmap::new();
        stray.insert(4_000_000);
        let mut bytes = Vec::new();
        stray.serialize_into(&mut bytes).unwrap();
        tamper(&store, |t, w| {
            t.sets.put(w, &set_key(PRESENT, Some("kind"), 61), &bytes).unwrap();
        });
        let diff = store.check_derived().unwrap();
        assert!(has(&diff, "set unexpected: "), "{}", some(&diff));
        assert!(diff.iter().any(|l| l.contains("4000000")), "{}", some(&diff));
    }
}
