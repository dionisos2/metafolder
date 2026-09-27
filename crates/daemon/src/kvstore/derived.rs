//! The derived key spaces (docs/spec-storage.org, "Increment 4, concretely"
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
use crate::index::field_index::{dt_key, num_key};
use crate::store::Rows;

/// The format of the derived key spaces. A store stamped with another (or
/// none: a store from before they existed) is reindexed when it opens.
pub(super) const DERIVED_VERSION: i64 = 4;

/// Set kinds.
pub(super) const UNIVERSE: u8 = 0;
pub(super) const PRESENT: u8 = 1;
pub(super) const ABSENT: u8 = 2;
pub(super) const PARENTS: u8 = 3;

/// The texts too long to be split in trigrams (see [`TRIGRAM_MAX`]).
pub(super) const LONG_TEXTS: u8 = 4;
/// The ids pointing at a uuid in a field — a `ref`'s referrers, a folder's
/// children — keyed by field *and* target: a folder filter is one bitmap,
/// a read per 65 536 children (spec-storage "Key layout", `children`).
pub(super) const REFERRERS: u8 = 5;

/// Every id below a node of a file tree, keyed by field and node — what
/// makes a subtree one bitmap read (spec-storage "The forest: descendant
/// bitmaps"). Kept for the fields in [`DESCENDANT_FIELDS`] only: a node there
/// holds one position, so the tree is a tree and a move is set arithmetic;
/// where a node may hang at several places the closure would need its paths
/// counted, and a subtree is expanded level by level instead.
pub(super) const DESCENDANTS: u8 = 6;

/// The holders of one value of a field (keyed by field and value key), for
/// the values held by [`POSTING_MIN`] records or more (spec-storage "Key
/// layout", `postings`). A rarer value is answered from its run in the value
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

/// The forests whose descendant bitmaps are kept: one position per node.
pub(crate) const DESCENDANT_FIELDS: [&str; 1] = ["mfr_path"];

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
            if DESCENDANT_FIELDS.contains(&field) {
                self.derive_descendants(uuid, id, field, *parent, delta > 0)?;
            }
        }
        Ok(())
    }

    /// A position of `uuid` under `parent` in a file tree came (`add`) or
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
    /// compared through the uuids they name, so a store whose ids were
    /// allocated in another order still agrees.
    pub fn check_derived(&self) -> Result<Vec<String>> {
        let r = self.env.read_txn()?;
        let t = &self.t;
        let mut diff = Vec::new();

        // Expected, from the primary data.
        let mut universe = BTreeSet::new();
        for entry in t.metarecords.iter(&r)? {
            universe.insert(uuid_of(entry?.0));
        }
        type Part = (String, u8, Vec<u8>, Uuid);
        let mut parts: BTreeMap<Part, i64> = BTreeMap::new();
        let mut kids: BTreeMap<([u8; 16], String, Uuid), i64> = BTreeMap::new();
        let mut sets: BTreeSet<(u8, String, Uuid)> = BTreeSet::new();
        let mut grams: BTreeSet<(String, [u8; 3], Uuid)> = BTreeSet::new();
        let mut referrers: BTreeSet<(String, [u8; 16], Uuid)> = BTreeSet::new();
        let mut valued: BTreeMap<(String, Uuid), usize> = BTreeMap::new();
        // Per descendant field: each node's parent, for the closure below.
        let mut parent_of: BTreeMap<(String, Uuid), Uuid> = BTreeMap::new();
        for entry in t.cells.iter(&r)? {
            let (k, v) = entry?;
            let uuid = uuid_of(k);
            let row = dec_row(v)?;
            let f = row.name.clone();
            let mut add = |part: u8, key: Vec<u8>| {
                *parts.entry((f.clone(), part, key, uuid)).or_default() += 1;
            };
            if let Some(key) = value_key(&row.value) {
                add(VALUE, key);
            }
            if let Some(key) = name_part_key(&row.value) {
                add(NAME, key);
            }
            if let Some(target) = target_of(&row.value) {
                add(TARGET, target.to_vec());
            }
            if let Value::TreeRef { .. } = row.value {
                let parent = target_of(&row.value).expect("a tree_ref has a parent key");
                *kids.entry((parent, f.clone(), uuid)).or_default() += 1;
                if universe.contains(&Uuid::from_bytes(parent)) {
                    sets.insert((PARENTS, f.clone(), Uuid::from_bytes(parent)));
                }
            }
            if let Some(text) = search_text(&row.value) {
                if text.len() <= TRIGRAM_MAX {
                    for gram in trigrams(&text) {
                        grams.insert((f.clone(), gram, uuid));
                    }
                } else {
                    sets.insert((LONG_TEXTS, f.clone(), uuid));
                }
            }
            if let Some(target) = target_of(&row.value) {
                referrers.insert((f.clone(), target, uuid));
            }
            if let Value::TreeRef { parent: Some(p), .. } = &row.value {
                if DESCENDANT_FIELDS.contains(&f.as_str()) {
                    parent_of.insert((f.clone(), uuid), *p);
                }
            }
            let kind = if matches!(row.value, Value::Nothing) { ABSENT } else { PRESENT };
            if kind == PRESENT {
                *valued.entry((f.clone(), uuid)).or_default() += 1;
            }
            sets.insert((kind, f, uuid));
        }

        for ((f, uuid), n) in valued {
            if n > 1 {
                sets.insert((MULTI, f, uuid));
            }
        }

        // Actual: the id mappings first, then everything through them.
        let mut to_uuid: HashMap<u32, Uuid> = HashMap::new();
        for entry in t.uuids.iter(&r)? {
            let (k, v) = entry?;
            to_uuid.insert(dense(k), uuid_of(v));
        }
        let mut mapped = BTreeSet::new();
        for entry in t.ids.iter(&r)? {
            let (k, v) = entry?;
            let (uuid, id) = (uuid_of(k), dense(v));
            if to_uuid.get(&id) != Some(&uuid) {
                diff.push(format!("ids: {uuid} -> {id}, but uuids says {:?}", to_uuid.get(&id)));
            }
            mapped.insert(uuid);
        }
        if mapped != universe {
            diff.push(format!(
                "ids: map {} metarecords, the store holds {}",
                mapped.len(),
                universe.len()
            ));
        }
        let uuid = |id: u32, diff: &mut Vec<String>| {
            let u = to_uuid.get(&id).copied();
            if u.is_none() {
                diff.push(format!("dense id {id} names no metarecord"));
            }
            u
        };

        let mut got_universe = BTreeSet::new();
        let mut got_sets = BTreeSet::new();
        let mut got_referrers = BTreeSet::new();
        let mut got_descendants = BTreeSet::new();
        let mut got_postings: BTreeMap<(String, Vec<u8>), BTreeSet<Uuid>> = BTreeMap::new();
        for entry in t.sets.iter(&r)? {
            let (k, v) = entry?;
            let kind = k[0];
            let (field, rest) = if kind == UNIVERSE {
                (String::new(), &k[1..])
            } else {
                let (f, rest) = unesc(&k[1..])?;
                (String::from_utf8(f).context("a field name")?, rest)
            };
            for id in decode_set(v)? {
                let Some(u) = uuid(id, &mut diff) else { continue };
                if kind == UNIVERSE {
                    got_universe.insert(u);
                } else if kind == REFERRERS {
                    let target: [u8; 16] = rest[..16].try_into().context("a target uuid")?;
                    got_referrers.insert((field.clone(), target, u));
                } else if kind == POSTINGS {
                    let key = rest[..rest.len() - 2].to_vec();
                    got_postings.entry((field.clone(), key)).or_default().insert(u);
                } else if kind == DESCENDANTS {
                    let node: [u8; 16] = rest[..16].try_into().context("a node uuid")?;
                    got_descendants.insert((field.clone(), node, u));
                } else {
                    got_sets.insert((kind, field.clone(), u));
                }
            }
        }
        report(&mut diff, "referrer", lines(&referrers), lines(&got_referrers));
        let mut descendants: BTreeSet<(String, [u8; 16], Uuid)> = BTreeSet::new();
        for ((field, node), first) in &parent_of {
            let mut up = Some(*first);
            for _ in 0..crate::log::MAX_TREE_DEPTH {
                let Some(ancestor) = up else { break };
                descendants.insert((field.clone(), *ancestor.as_bytes(), *node));
                up = parent_of.get(&(field.clone(), ancestor)).copied();
            }
        }
        report(&mut diff, "descendant", lines(&descendants), lines(&got_descendants));
        if got_universe != universe {
            diff.push(format!(
                "universe: {} ids, the store holds {} metarecords",
                got_universe.len(),
                universe.len()
            ));
        }
        report(&mut diff, "set", lines(&sets), lines(&got_sets));

        let mut got_parts = BTreeMap::new();
        for entry in t.parts.iter(&r)? {
            let (k, v) = entry?;
            let (field, rest) = unesc(k)?;
            let (part, key, id) =
                (rest[0], &rest[1..rest.len() - 4], dense(&rest[rest.len() - 4..]));
            let Some(u) = uuid(id, &mut diff) else { continue };
            let field = String::from_utf8(field).context("a field name")?;
            got_parts.insert((field, part, key.to_vec(), u), from_be(v));
        }
        report(&mut diff, "part", lines(&parts), lines(&got_parts));

        // Postings: required from the threshold on, and exact wherever kept
        // (a value grown rarer keeps its posting).
        let mut holders: BTreeMap<(String, Vec<u8>), BTreeSet<Uuid>> = BTreeMap::new();
        for (field, part, key, u) in parts.keys() {
            if *part == VALUE {
                holders.entry((field.clone(), key.clone())).or_default().insert(*u);
            }
        }
        for (value, ids) in &holders {
            match got_postings.remove(value) {
                Some(got) if got != *ids => diff.push(format!(
                    "posting of {value:?}: {} ids, the value has {} holders",
                    got.len(),
                    ids.len()
                )),
                None if ids.len() as u64 >= POSTING_MIN => {
                    diff.push(format!("posting missing: {value:?} ({} holders)", ids.len()))
                }
                _ => {}
            }
        }
        for value in got_postings.keys() {
            diff.push(format!("posting unexpected: {value:?} (no holder)"));
        }

        let mut got_kids = BTreeMap::new();
        for entry in t.kids.iter(&r)? {
            let (k, v) = entry?;
            let parent: [u8; 16] = k[..16].try_into().expect("a 16-byte uuid");
            let (field, rest) = unesc(&k[16..])?;
            let Some(u) = uuid(dense(rest), &mut diff) else { continue };
            let field = String::from_utf8(field).context("a field name")?;
            got_kids.insert((parent, field, u), from_be(v));
        }
        report(&mut diff, "kid", lines(&kids), lines(&got_kids));

        let mut got_grams = BTreeSet::new();
        for entry in t.grams.iter(&r)? {
            let (k, v) = entry?;
            let (field, rest) = unesc(k)?;
            let field = String::from_utf8(field).context("a field name")?;
            let gram: [u8; 3] = rest[..3].try_into().context("a trigram")?;
            for id in decode_set(v)? {
                let Some(u) = uuid(id, &mut diff) else { continue };
                got_grams.insert((field.clone(), gram, u));
            }
        }
        report(&mut diff, "gram", lines(&grams), lines(&got_grams));

        diff.truncate(100);
        Ok(diff)
    }
}

/// Every entry of a collection, as text — what [`report`] compares.
fn lines<T: std::fmt::Debug>(items: impl IntoIterator<Item = T>) -> BTreeSet<String> {
    items.into_iter().map(|e| format!("{e:?}")).collect()
}

/// The entries one side holds and the other does not (or holds otherwise).
fn report(diff: &mut Vec<String>, what: &str, expected: BTreeSet<String>, got: BTreeSet<String>) {
    for e in expected.difference(&got) {
        diff.push(format!("{what} missing: {e}"));
    }
    for e in got.difference(&expected) {
        diff.push(format!("{what} unexpected: {e}"));
    }
}
