//! The derived key spaces (doc "Store tables"): what the query source
//! reads, written in each write's own transaction and rebuilt from the
//! primary data by [`KvStore::reindex`].
//!
//! | table | key                                  | value                    |
//! |-------|--------------------------------------|--------------------------|
//! | ids   | uuid                                 | dense id (u32)           |
//! | uuids | dense id                             | uuid                     |
//! | sets  | kind · [field] · [key] · chunk       | a roaring bitmap         |
//! | kids  | parent id · field · dense id         | rows (count)             |
//! | grams | 0 · field · trigram · text key       | (empty)                  |
//! | grams | 1 · field · trigram · chunk          | a roaring bitmap         |
//! | grams | 2 · field                            | the field's trigram state|
//! | hist  | field · bucket                       | rows (count)             |
//!
//! A *set* is a bitmap of dense ids cut in chunks of 65 536 ids (the high 16
//! bits), so a write rewrites one chunk and a read unions a few: the universe,
//! and per field the ids holding a value (`present`), a `Nothing` (`absent`),
//! and — on a `tree_ref` field — a child (`parents`). A *partition* orders a
//! field's `tree_ref` names by a key whose bytes sort like them. `kids` is the forest
//! keyed by parent first, so a metarecord arriving or leaving finds the fields
//! it is a parent in without a scan. A value is a *posting*: its holders as
//! one set (`POSTINGS · field · value key · chunk`), so an equality reads a
//! key per 65 536 holders and the values are in order of their keys.
//! A number's or a date's row also counts in its field's *histogram*: rows
//! per bucket, a bucket being a prefix of the value key ([`hist_bucket`]), so
//! the planner estimates a range by summing a few counts (doc "Planner
//! statistics").
//!
//! Every row change goes through [`KvTxn::derive_row`], every metarecord
//! creation and removal through [`KvTxn::derive_created`] /
//! [`KvTxn::derive_removed`] — a revert included, which writes by
//! the same primitives. [`KvStore::check_derived`] holds all of it to a
//! rebuild.

use std::collections::{BTreeMap, BTreeSet, HashMap};

#[path = "events.rs"]
pub(super) mod events;

use anyhow::{bail, Context, Result};
use heed::RoTxn;
use metafolder_core::metarecord::Value;
use roaring::RoaringBitmap;
use uuid::Uuid;

use super::{be, dec_rows, from_be, name_key, uuid_of, KvStore, KvTxn, Tables};
use crate::index::keys::{dt_key, num_key};
use crate::store::Rows;

/// The format of the derived key spaces. A store stamped with another (or
/// none: a store from before they existed) is reindexed when it opens. 5:
/// descendant bitmaps for every forest (4 kept `mfr_path`'s only); 6: the
/// histograms; 7: the trigrams in two modes (doc "Text search in the store");
/// 8: what points at a metarecord filed under its dense id, and no target
/// partition; 9: the count of values in place of the `present` and `multi`
/// sets; 10: the fields' pending events; 11: every value a posting, and no value partition; 12: a position's name a posting too, and no `parts`; 13: a position's value no posting (the forest holds it); 14: the histograms count the posting keys of each bucket; 15: the record of a deleted metarecord stays until a reindex.
pub(super) const DERIVED_VERSION: i64 = 15;

/// Set kinds.
pub(super) const UNIVERSE: u8 = 0;
pub(super) const ABSENT: u8 = 2;
pub(super) const PARENTS: u8 = 3;

/// The texts too long to be split in trigrams (see [`TRIGRAM_MAX`]).
pub(super) const LONG_TEXTS: u8 = 4;
/// The ids pointing at a metarecord in a field — a `ref`'s referrers, a
/// folder's children — keyed by field *and* the target's dense id
/// ([`KvTxn::target_id`]): a folder filter is one bitmap, a read per 65 536
/// children (doc "Store tables", `children`).
pub(super) const REFERRERS: u8 = 5;

/// Every id below a node of a forest, keyed by field and the node's dense id — what makes a
/// subtree one bitmap read (doc "The forest in the store").
/// Kept for every `tree_ref` field: a node holds one position per forest
/// (doc "One position per forest"), so the tree is a tree and a
/// move is set arithmetic.
pub(super) const DESCENDANTS: u8 = 6;

/// The holders of one value of a field (keyed by field and value key, then
/// chunk): every value that has a holder (doc "Store tables", `postings`).
/// The value partition is gone: a posting is a value's run, in the order of
/// the keys, and goes with its last holder.
pub(super) const POSTINGS: u8 = 7;

/// The holders of one name of a position (keyed by field and name key, then
/// chunk): the files called so, whatever their folder — what sorts and
/// searches by name read.
pub(super) const NAME_POSTINGS: u8 = 12;

/// The kind of set a partition's postings are.
pub(super) fn postings_kind(part: u8) -> u8 {
    if part == VALUE {
        POSTINGS
    } else {
        NAME_POSTINGS
    }
}

/// How many non-`Nothing` rows of a field each id holds, as one set per bit
/// of the count (keyed by field and bit number): the ids holding a value are
/// those in any of them, the ids holding several those in any but the
/// first. A count is what a write to many records can keep without reading
/// one: adding a value to the ids that lack it is an increment, which on
/// bitmaps is a few set operations whatever the number of ids (doc "Store
/// tables").
pub(super) const COUNT: u8 = 11;

/// The bits a count is kept on: no record holds 2³² values of a field.
pub(super) const COUNT_BITS: u8 = 32;

/// The key of one bit of `field`'s counts, without its chunk.
pub(super) fn count_prefix(field: &str, bit: u8) -> Vec<u8> {
    [&[COUNT][..], &name_key(field), &[bit]].concat()
}

/// `field`'s counts, a set per bit, the lowest first — as many as the
/// largest count needs.
pub(super) fn read_counts(t: &Tables, r: &RoTxn<'_>, field: &str) -> Result<Vec<RoaringBitmap>> {
    let prefix = set_prefix(COUNT, Some(field));
    let mut out: Vec<RoaringBitmap> = Vec::new();
    for entry in t.sets.prefix_iter(r, &prefix)? {
        let (k, v) = entry?;
        // The bit, then the chunk.
        if k.len() == prefix.len() + 3 {
            let bit = k[prefix.len()] as usize;
            if out.len() <= bit {
                out.resize(bit + 1, RoaringBitmap::new());
            }
            out[bit] |= decode_set(v)?;
        }
    }
    Ok(out)
}

/// The ids whose count is over `n`, among `counts` ([`read_counts`]): the
/// counts compared with `n` bit by bit from the highest — an id is over as
/// soon as it has a bit `n` lacks, having had the same bits until then.
pub(super) fn counted_over(counts: &[RoaringBitmap], n: u64) -> RoaringBitmap {
    let mut over = RoaringBitmap::new();
    // The ids equal to `n` on the bits seen so far; `None`: every id.
    let mut same: Option<RoaringBitmap> = None;
    for bit in (0..counts.len().max(64 - n.leading_zeros() as usize)).rev() {
        let empty = RoaringBitmap::new();
        let set = counts.get(bit).unwrap_or(&empty);
        if n >> bit & 1 == 0 {
            over |= match &same {
                None => set.clone(),
                Some(same) => same & set,
            };
            same = Some(match same {
                // "Every id" less a set is not a set: the ids of any count
                // are those of some bit, which is all that can be over.
                None => counts.iter().fold(RoaringBitmap::new(), |a, b| a | b) - set,
                Some(same) => same - set,
            });
        } else {
            same = Some(match same {
                None => set.clone(),
                Some(same) => same & set,
            });
        }
    }
    over
}

/// In a field whose trigrams are kept by metarecord: the ids written since
/// the last fold, whose trigrams are not built yet — every text search checks
/// them (doc "Text search in the store").
pub(super) const GRAM_PENDING: u8 = 9;

/// The dense ids given to a uuid something points at and no metarecord has
/// had: a reference to a metarecord not created yet, a child written before
/// its parent. The metarecord takes that id when it comes.
pub(super) const RESERVED: u8 = 10;

/// The `meta` counter of the numberings of the dense ids: a reindex, which
/// numbers the metarecords again, moves it, and a cursor of an earlier
/// numbering is refused (doc "Pagination and sorting").
pub(super) const NUMBERING: &str = "numbering";

/// The dense id standing for "no parent": what a forest's roots are filed
/// under. No metarecord gets it — ids start at 1.
pub(super) const NO_TARGET: u32 = 0;

/// The key of the posting of a value key in `field`, without its chunk. A
/// value key is self-delimiting (fixed-width, or a text ended by its
/// terminator or marker and hash), so no posting's prefix is another's.
pub(super) fn posting_prefix(field: &str, key: &[u8]) -> Vec<u8> {
    part_posting_prefix(field, VALUE, key)
}

/// The key of the posting of `key` in a partition of `field`, without its chunk.
pub(super) fn part_posting_prefix(field: &str, part: u8, key: &[u8]) -> Vec<u8> {
    [&[postings_kind(part)][..], &name_key(field), key].concat()
}

/// The key of the descendants of `node` in `field`, without its chunk.
pub(super) fn descendants_prefix(field: &str, node: u32) -> Vec<u8> {
    [&[DESCENDANTS][..], &name_key(field), &node.to_be_bytes()].concat()
}

/// The key of the referrers of `target` in `field`, without its chunk.
pub(super) fn referrers_prefix(field: &str, target: u32) -> Vec<u8> {
    [&[REFERRERS][..], &name_key(field), &target.to_be_bytes()].concat()
}

/// The histogram bucket of a value key: the prefix of it that rows are
/// counted under — for a number (an order-preserving `f64`) its type tag and
/// first 16 bits, the sign, the exponent and 4 bits of mantissa: 1/16 of an
/// octave; for a date (milliseconds) its tag and first 34 bits, about twelve
/// days, the rest of the last byte cleared. Buckets are contiguous ranges of
/// the value partition, of one width per type, so comparing a bucket with an
/// operand's says whether all of its rows are below, above, or undecided.
/// `None` for the types without a histogram: a text, a bool, a reference
/// (doc "Query planner", Open).
pub(crate) fn hist_bucket(key: &[u8]) -> Option<Vec<u8>> {
    match key.first() {
        Some(1) => Some(key[..3].to_vec()),
        Some(3) => {
            let mut b = key[..6].to_vec();
            b[5] &= 0xC0;
            Some(b)
        }
        _ => None,
    }
}

/// The key of a bucket of `field`'s histogram.
pub(super) fn hist_key(field: &str, bucket: &[u8]) -> Vec<u8> {
    [&name_key(field)[..], bucket].concat()
}

/// The key of the count of posting keys in a bucket of `field`'s histogram:
/// how many keys a walk of the bucket reads, a key per chunk of each value.
/// Past every bucket's tag, so a range of buckets never meets it.
pub(super) fn hist_chunks_key(field: &str, bucket: &[u8]) -> Vec<u8> {
    [&name_key(field)[..], &[0xFF], bucket].concat()
}

/// The two tables of chunked bitmaps, as the transaction's chunk cache
/// tells them apart.
pub(super) const SETS: u8 = 0;
const GRAMS: u8 = 1;

/// The longest text (lower-cased, in bytes) split in trigrams. A longer one —
/// a note, lyrics — would write a key per trigram on every edit; its id joins
/// the field's long-text set instead, and every search checks it.
pub(crate) const TRIGRAM_MAX: usize = 512;

/// Partitions.
pub(super) const VALUE: u8 = 0;
pub(super) const NAME: u8 = 1;

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
        Value::TreeRef { name, .. } | Value::HierarchyRef { name, .. } => {
            Some(name.display().to_lowercase())
        }
        _ => None,
    }
}

/// The three key spaces of `grams` (doc "Text search in the store"): the
/// distinct texts holding a trigram, the ids holding it, and each field's
/// state.
const GRAM_VALUES: u8 = 0;
const GRAM_IDS: u8 = 1;
const GRAM_STATE: u8 = 2;

/// How many distinct texts a field's trigram names on average before the
/// field turns by metarecord: under it a new text writes its trigrams (some
/// twenty keys, once per distinct text) and a search reads no metarecord;
/// over it the field's texts are mostly new ones — file names — and a write
/// only marks its id pending. A starting point, to be measured.
pub(crate) const GRAM_VALUES_MAX: u64 = 16;

/// The key of a trigram's bitmap of ids, without its chunk.
pub(super) fn gram_prefix(field: &str, gram: &[u8; 3]) -> Vec<u8> {
    [&[GRAM_IDS][..], &name_key(field), gram].concat()
}

/// The prefix of the texts holding a trigram: each key ends with the text's
/// key in the partition the field's texts are read from ([`text_part_key`]).
pub(super) fn gram_values_prefix(field: &str, gram: &[u8; 3]) -> Vec<u8> {
    [&[GRAM_VALUES][..], &name_key(field), gram].concat()
}

/// The key of a field's trigram state.
pub(super) fn gram_state_key(field: &str) -> Vec<u8> {
    [&[GRAM_STATE][..], &name_key(field)].concat()
}

/// How a field's trigrams are kept.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub(super) struct GramState {
    /// By metarecord (trigram → ids, built by a fold) rather than by value
    /// (trigram → distinct texts, written with each new text).
    pub(super) by_ids: bool,
    /// By value: the (trigram, text) entries, and the distinct trigrams.
    entries: u64,
    grams: u64,
    /// The field turned by metarecord and its by-value entries are still
    /// there, for the next fold to drop.
    leftovers: bool,
}

impl GramState {
    pub(super) fn decode(bytes: Option<&[u8]>) -> Result<GramState> {
        let Some(b) = bytes else { return Ok(GramState::default()) };
        anyhow::ensure!(b.len() == 18, "a trigram state of {} bytes", b.len());
        Ok(GramState {
            by_ids: b[0] != 0,
            leftovers: b[1] != 0,
            entries: u64::from_be_bytes(b[2..10].try_into().expect("8 bytes")),
            grams: u64::from_be_bytes(b[10..18].try_into().expect("8 bytes")),
        })
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = vec![self.by_ids as u8, self.leftovers as u8];
        out.extend_from_slice(&self.entries.to_be_bytes());
        out.extend_from_slice(&self.grams.to_be_bytes());
        out
    }
}

/// The partition a value's text is read from, and its key there: a string's
/// value key, a position's name. What a by-value trigram entry names.
pub(crate) fn text_part_key(value: &Value) -> Option<(u8, Vec<u8>)> {
    match value {
        Value::String(_) => value_key(value).map(|k| (VALUE, k)),
        _ => name_part_key(value).map(|k| (NAME, k)),
    }
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
        Value::HierarchyRef { parent, name } => {
            let parent = parent.map_or(ZERO, |p| *p.as_bytes());
            tagged(8, &[&parent[..], &text_key(name.as_bytes())].concat())
        }
    })
}

/// The type tag leading the value keys of a value type ([`value_key`]): `int`
/// and `float` share one, compared together as numbers. `None` for a type no
/// value key carries.
pub(crate) fn type_tag(value_type: &str) -> Option<u8> {
    Some(match value_type {
        "bool" => 0,
        "int" | "float" => 1,
        "string" => 2,
        "datetime" => 3,
        "ref" => 4,
        "refbase" => 5,
        "externalref" => 6,
        "tree_ref" => 7,
        "hierarchy_ref" => 8,
        _ => return None,
    })
}

/// The key of a position (`tree_ref`, `hierarchy_ref`) in the name partition.
pub(crate) fn name_part_key(value: &Value) -> Option<Vec<u8>> {
    value.position().map(|(_, name)| text_key(name.display().as_bytes()))
}

/// The uuid a value points at — a `ref`'s referent, a position's parent (the
/// zero uuid for a root) — which is what `Follows` matches.
pub(crate) fn target_of(value: &Value) -> Option<[u8; 16]> {
    match value {
        Value::Ref(u) => Some(*u.as_bytes()),
        _ => value.position().map(|(parent, _)| parent.map_or(ZERO, |p| *p.as_bytes())),
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

fn kids_key(parent: u32, field: &str, id: u32) -> Vec<u8> {
    [&parent.to_be_bytes()[..], &name_key(field), &id.to_be_bytes()].concat()
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

/// The bitmap filed under `prefix` (a set's key without its chunk), as
/// committed: its chunks unioned.
pub(super) fn read_prefix_set(t: &Tables, r: &RoTxn<'_>, prefix: &[u8]) -> Result<RoaringBitmap> {
    let mut out = RoaringBitmap::new();
    for entry in t.sets.prefix_iter(r, prefix)? {
        let (k, v) = entry?;
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

    /// The dense id of a metarecord, as the transaction sees it.
    pub(super) fn dense_id(&self, uuid: &[u8]) -> Result<Option<u32>> {
        self.id(uuid)
    }

    /// Runs `f` on one chunk of the universe as the transaction sees it: the
    /// chunk it changed, or the stored one — read once, and not put among the
    /// changed chunks, which the commit writes.
    fn with_universe<T>(
        &self,
        r: &RoTxn<'_>,
        chunk: u16,
        f: impl FnOnce(&RoaringBitmap) -> T,
    ) -> Result<T> {
        let k = set_key(UNIVERSE, None, chunk);
        let cached = [&[SETS][..], &k].concat();
        if let Some(bm) = self.sets.borrow().get(&cached) {
            return Ok(f(bm));
        }
        let mut seen = self.universe.borrow_mut();
        if let std::collections::hash_map::Entry::Vacant(slot) = seen.entry(chunk) {
            slot.insert(match self.t.sets.get(r, &k)? {
                Some(bytes) => decode_set(bytes)?,
                None => RoaringBitmap::new(),
            });
        }
        Ok(f(&seen[&chunk]))
    }

    /// Whether the metarecord of `id` exists, as the transaction sees it.
    pub(super) fn live_in(&self, r: &RoTxn<'_>, id: u32) -> Result<bool> {
        self.with_universe(r, (id >> 16) as u16, |bm| bm.contains(id))
    }

    /// Whether any of `ids` is a metarecord that exists — a chunk of the
    /// universe at a time, not an id at a time: a folder emptied by one
    /// write asks it once per file.
    fn any_live(&self, ids: &RoaringBitmap) -> Result<bool> {
        let (Some(lo), Some(hi)) = (ids.min(), ids.max()) else { return Ok(false) };
        let r = self.txn.borrow();
        for chunk in (lo >> 16)..=(hi >> 16) {
            if ids.range_cardinality((chunk << 16)..=((chunk << 16) | 0xFFFF)) == 0 {
                continue;
            }
            if self.with_universe(&r, chunk as u16, |bm| !bm.is_disjoint(ids))? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Whether a metarecord that exists, other than `except`, is placed
    /// under `parent` in `field` — read off the parent's referrers, among
    /// which the children deleted since the last reindex still are.
    fn has_live_kids(&self, parent: u32, field: &str, except: Option<u32>) -> Result<bool> {
        let mut kids = self.read_chunked(SETS, &referrers_prefix(field, parent))?;
        if let Some(id) = except {
            kids.remove(id);
        }
        self.any_live(&kids)
    }

    /// Adds `id` to (or removes it from) a set, in the transaction's chunk
    /// cache: a bulk write touches each chunk many times and writes it once,
    /// at [`Self::flush_sets`].
    pub(super) fn set_member(
        &self,
        kind: u8,
        field: Option<&str>,
        id: u32,
        member: bool,
    ) -> Result<()> {
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
                Some(bytes) => {
                    if table == SETS && k[1] == POSTINGS {
                        self.chunks_existing.borrow_mut().insert(k.clone());
                    }
                    decode_set(bytes)?
                }
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
    pub(super) fn read_chunked(&self, table: u8, prefix: &[u8]) -> Result<RoaringBitmap> {
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
    pub(super) fn chunk_apply(
        &self,
        table: u8,
        prefix: &[u8],
        ids: &RoaringBitmap,
        add: bool,
    ) -> Result<()> {
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
        for field in self.grams_changed.borrow_mut().drain() {
            let state = self.grams.borrow()[&field];
            self.t.grams.put(&mut w, &gram_state_key(&field), &state.encode())?;
        }
        // The posting chunks that came or went, by bucket of their value.
        let mut chunks: BTreeMap<Vec<u8>, i64> = BTreeMap::new();
        for (k, mut bm) in self.sets.borrow_mut().drain() {
            if k[0] == SETS && k[1] == POSTINGS {
                let existed = self.chunks_existing.borrow_mut().remove(&k);
                let delta = i64::from(!bm.is_empty()) - i64::from(existed);
                if let (true, Ok((name, rest))) = (delta != 0, unesc(&k[2..])) {
                    let bucket = hist_bucket(&rest[..rest.len() - 2]);
                    if let (Some(bucket), Ok(field)) = (bucket, String::from_utf8(name)) {
                        *chunks.entry(hist_chunks_key(&field, &bucket)).or_default() += delta;
                    }
                }
            }
            let db = if k[0] == SETS { self.t.sets } else { self.t.grams };
            if bm.is_empty() {
                db.delete(&mut w, &k[1..])?;
            } else {
                // Stored with run containers where they are smaller, so
                // consecutive ids — a folder after a reindex — take a few
                // bytes per run (doc "Store tables").
                bm.optimize();
                let mut bytes = Vec::with_capacity(bm.serialized_size());
                bm.serialize_into(&mut bytes)?;
                db.put(&mut w, &k[1..], &bytes)?;
            }
        }
        drop(w);
        for (key, delta) in chunks {
            self.bump(self.t.hist, &key, delta)?;
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

    /// The fields in which the metarecord of dense id `parent` has children.
    fn parent_fields(&self, parent: u32) -> Result<BTreeSet<String>> {
        let mut out = BTreeSet::new();
        for entry in self.t.kids.prefix_iter(&self.txn.borrow(), &parent.to_be_bytes())? {
            let (k, _) = entry?;
            let (field, _) = unesc(&k[4..])?;
            out.insert(String::from_utf8(field).context("a field name")?);
        }
        Ok(out)
    }

    /// The dense id a target — a `ref`'s referent, a position's parent, as
    /// [`target_of`] gives it — is filed under: [`NO_TARGET`] for no parent,
    /// the metarecord's id, or one reserved for a uuid no metarecord has had.
    fn target_id(&self, target: &[u8; 16]) -> Result<u32> {
        if *target == ZERO {
            return Ok(NO_TARGET);
        }
        if let Some(id) = self.id(target)? {
            return Ok(id);
        }
        let id = self.new_id(target)?;
        self.set_member(RESERVED, None, id, true)?;
        Ok(id)
    }

    /// The next dense id, given to `uuid`.
    fn new_id(&self, uuid: &[u8; 16]) -> Result<u32> {
        let id = u32::try_from(self.next("next_id")?).context("dense ids exhausted")?;
        let mut w = self.txn.borrow_mut();
        self.t.ids.put(&mut w, uuid, &id.to_be_bytes())?;
        self.t.uuids.put(&mut w, &id.to_be_bytes(), uuid)?;
        Ok(id)
    }

    /// Whether `id` was reserved for a uuid no metarecord has had.
    pub(super) fn reserved(&self, id: u32) -> Result<bool> {
        let mut found = false;
        self.with_chunk(SETS, &set_prefix(RESERVED, None), (id >> 16) as u16, &mut |bm| {
            found = bm.contains(id);
        })?;
        Ok(found)
    }

    /// A metarecord was created: it gets a dense id, in the universe, and
    /// is in `parents` wherever rows already hang below it (a revert may put
    /// a child back before its parent). The id is the one reserved for its
    /// uuid when something pointed at it first; otherwise the next one — a
    /// new one too for a uuid that had a metarecord before, whose former id
    /// still names what that one left in the indexes (doc "Deleting a
    /// metarecord in the store"): what points at the uuid is then filed
    /// under the new id.
    pub(super) fn derive_created(&self, uuid: Uuid) -> Result<()> {
        let id = match self.id(uuid.as_bytes())? {
            Some(reserved) if self.reserved(reserved)? => {
                self.set_member(RESERVED, None, reserved, false)?;
                reserved
            }
            Some(_) => bail!("metarecord {uuid} was deleted: it is given back, not created"),
            None => self.new_id(uuid.as_bytes())?,
        };
        self.set_member(UNIVERSE, None, id, true)?;
        for field in self.parent_fields(id)? {
            if self.has_live_kids(id, &field, None)? {
                self.set_member(PARENTS, Some(&field), id, true)?;
            }
        }
        Ok(())
    }

    /// A metarecord was removed (its rows first): its id leaves the universe,
    /// and stays wherever else it is — its mapping to its uuid included. An
    /// id is never given again, so what still names it names nothing: every
    /// reader sifts its answers through the universe, and a reindex drops the
    /// lot (doc "Deleting a metarecord in the store"). A metarecord created
    /// again under the same uuid (a revert) gets a new id.
    pub(super) fn derive_removed(&self, uuid: Uuid) -> Result<()> {
        let Some(id) = self.id(uuid.as_bytes())? else { return Ok(()) };
        self.set_member(UNIVERSE, None, id, false)
    }

    /// A deleted metarecord comes back under the id it had (doc "Deleting a
    /// metarecord in the store"): its record, which stays until a reindex, says
    /// what the entries that stayed in the indexes describe, so only what the
    /// deletion took off is given back: the record's number, the universe, the
    /// counts, the forest keys and the parents. The rows it holds again are
    /// returned; `None` — nothing written — when the uuid never had an id or its
    /// metarecord exists.
    pub(super) fn restore_deleted(&self, uuid: Uuid) -> Result<Option<Vec<crate::rows::FieldRow>>> {
        let Some(id) = self.id(uuid.as_bytes())? else { return Ok(None) };
        if self.live_in(&self.txn.borrow(), id)? || self.reserved(id)? {
            return Ok(None);
        }
        let rows = self.load_record(id)?.plain_rows();
        let rows = rows.as_slice();
        // A position a metarecord that exists took since is not given back.
        for row in rows {
            let Some(k) = Self::forest_key(&row.name, &row.value) else { continue };
            let txn = self.txn.borrow();
            if let Some(v) = self.t.forest.get(&txn, &k)? {
                let holder = dense(&v[super::FOREST_ID..]);
                if holder != id && self.live_in(&txn, holder)? {
                    return Err(crate::error::DomainError::BadRequest(format!(
                        "tree position already occupied for field '{}'",
                        row.name
                    ))
                    .into());
                }
            }
        }
        // A record of a deletion of many records still pending is given back
        // as that deletion's own undo does it, for this one operation: the
        // mark names the id, and the operation is its rank.
        if self.deletes_pending() {
            let marks = {
                let txn = self.txn.borrow();
                super::mass::read_part(&self.t, &txn, super::mass::DELETES_PART)?
            };
            if let Some(mark) = marks.iter().find(|m| m.ids.contains(id)) {
                let op = mark.op as i64 + (mark.ids.rank(id) - 1) as i64;
                if self.mass_undelete_log(op, op)?.is_some() {
                    return Ok(Some(self.load_record(id)?.plain_rows()));
                }
                // The mark is gone before the record is given back.
                while self.fold_one_delete()? {}
            }
        }
        self.put_given_back(id, rows)?;
        self.t.metarecords.put(&mut self.txn.borrow_mut(), uuid.as_bytes(), &[])?;
        for row in rows {
            self.count_type(&row.name, &row.value, 1)?;
            self.add_hist(&row.name, &row.value)?;
            if let Some(k) = Self::forest_key(&row.name, &row.value) {
                let v = super::key(&[uuid.as_bytes(), &id.to_be_bytes()]);
                self.t.forest.put(&mut self.txn.borrow_mut(), &k, &v)?;
            }
            if row.value.position().is_some() {
                let pid =
                    self.target_id(&target_of(&row.value).expect("a position has a parent"))?;
                if pid != NO_TARGET {
                    self.set_member(PARENTS, Some(&row.name), pid, true)?;
                }
            }
        }
        self.set_member(UNIVERSE, None, id, true)?;
        for field in self.parent_fields(id)? {
            if self.has_live_kids(id, &field, None)? {
                self.set_member(PARENTS, Some(&field), id, true)?;
            }
        }
        Ok(Some(rows.to_vec()))
    }

    /// Writes the rows of a record given back under its id. The entries of a
    /// write to many records still naming the id are older than this write:
    /// they change none of its rows, and add none to a field it lacks — the
    /// record says so, or they would apply to the rows again on reading.
    pub(super) fn put_given_back(&self, id: u32, rows: &[crate::rows::FieldRow]) -> Result<()> {
        let entries = if self.mass_pending() { self.load_record(id)?.entries } else { Vec::new() };
        let mut w = self.txn.borrow_mut();
        if entries.is_empty() {
            if rows.is_empty() {
                self.t.records.delete(&mut w, &id.to_be_bytes())?;
            } else {
                let record = super::mass::Record::plain(rows.to_vec());
                self.t.records.put(&mut w, &id.to_be_bytes(), &record.encode())?;
            }
            return Ok(());
        }
        drop(w);
        let n = self.next("mass_op")? as u64;
        let fields: BTreeSet<&str> = entries.iter().map(|e| e.field.as_str()).collect();
        let record = super::mass::Record {
            ops: vec![n; rows.len()],
            rows: rows.to_vec(),
            tombs: Vec::new(),
            cleared: fields.into_iter().map(|f| (f.to_string(), n)).collect(),
        };
        self.t.records.put(&mut self.txn.borrow_mut(), &id.to_be_bytes(), &record.encode())?;
        Ok(())
    }

    /// The histogram forgets a row of a metarecord that was deleted: what the
    /// fold of a deletion of many records gives back, the histograms counting
    /// rows that no universe sifts.
    pub(super) fn drop_hist(&self, field: &str, value: &Value) -> Result<()> {
        if let Some(bucket) = value_key(value).as_deref().and_then(hist_bucket) {
            self.bump(self.t.hist, &hist_key(field, &bucket), -1)?;
        }
        Ok(())
    }

    /// The histogram counts a row again: what a deletion's fold took off, put
    /// back with the record.
    pub(super) fn add_hist(&self, field: &str, value: &Value) -> Result<()> {
        if let Some(bucket) = value_key(value).as_deref().and_then(hist_bucket) {
            self.bump(self.t.hist, &hist_key(field, &bucket), 1)?;
        }
        Ok(())
    }

    /// A row of `uuid` goes with its metarecord, which is being removed whole
    /// (not [`Self::derive_row`], which unpicks every entry of the row): only
    /// what no universe can sift is kept exact — the histograms, which count
    /// rows, and whether the row's parent still has a child.
    pub(super) fn derive_dropped(&self, uuid: Uuid, field: &str, value: &Value) -> Result<()> {
        if let Some(bucket) = value_key(value).as_deref().and_then(hist_bucket) {
            self.bump(self.t.hist, &hist_key(field, &bucket), -1)?;
        }
        if value.position().is_some() {
            let parent = target_of(value).expect("a position has a parent key");
            let pid = self.target_id(&parent)?;
            let Some(id) = self.id(uuid.as_bytes())? else { return Ok(()) };
            if pid != NO_TARGET && !self.has_live_kids(pid, field, Some(id))? {
                self.set_member(PARENTS, Some(field), pid, false)?;
            }
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
        count_before: u64,
    ) -> Result<()> {
        let id = self.id(uuid.as_bytes())?.with_context(|| format!("no dense id for {uuid}"))?;
        // A field with events pending takes this row as one more, of one
        // id: what is stored is behind, and is not written past them.
        let eventful = self.eventful(field)? > 0;
        // Before the partitions learn of the row: whether its text is one
        // the field does not hold yet.
        let text = match search_text(value) {
            Some(text) if delta > 0 && text.len() <= TRIGRAM_MAX => Some(text),
            _ => None,
        };
        let new_text = match &text {
            Some(_) => self.new_text(field, value)?,
            None => None,
        };
        // The metarecord's rows of the field, as the primary write left them.
        let rows = Rows::rows_named(self, uuid, field)?;
        // A position's value is not a posting: the forest holds it exactly
        // (one metarecord for a parent and a name), and its parent and name
        // are what the referrers and the name postings hold.
        if let Some(k) = value_key(value).filter(|_| value.position().is_none()) {
            if eventful {
                self.single_event(field, value, id, delta > 0)?;
            } else {
                // A record is in a value's posting while one of its rows has
                // its key: a row going leaves it only if no other does.
                let held = delta > 0
                    || rows.iter().any(|r| value_key(&r.value).as_deref() == Some(&k[..]));
                self.chunk_member(SETS, &posting_prefix(field, &k), id, held)?;
            }
            if let Some(bucket) = hist_bucket(&k) {
                self.bump(self.t.hist, &hist_key(field, &bucket), delta)?;
            }
        }
        if let Some(k) = name_part_key(value) {
            let held = delta > 0
                || rows.iter().any(|r| name_part_key(&r.value).as_deref() == Some(&k[..]));
            self.chunk_member(SETS, &part_posting_prefix(field, NAME, &k), id, held)?;
        }
        // What the row points at, by the dense id it is filed under.
        let target = match target_of(value) {
            Some(target) => Some((target, self.target_id(&target)?)),
            None => None,
        };
        if let (Some((parent, pid)), true) = (target, value.position().is_some()) {
            self.bump(self.t.kids, &kids_key(pid, field, id), delta)?;
            if pid != NO_TARGET {
                // The parent's referrers still hold `id` (they are brought up
                // to date below): it counts only if it keeps a row there.
                let stays = rows.iter().any(|r| target_of(&r.value) == Some(parent));
                let member = delta > 0 || self.has_live_kids(pid, field, (!stays).then_some(id))?;
                self.set_member(PARENTS, Some(field), pid, member)?;
            }
        }
        let absent = rows.iter().any(|r| matches!(r.value, Value::Nothing));
        self.set_member(ABSENT, Some(field), id, absent)?;
        let valued = rows.iter().filter(|r| !matches!(r.value, Value::Nothing)).count();
        self.derive_count(field, id, valued as u64, value, count_before)?;
        // Trigrams give candidates, which a search always checks: a text
        // going leaves its own where they are, and only a text coming writes
        // (doc "Text search in the store").
        if let Some(text) = &text {
            self.derive_grams(field, id, text, new_text.as_deref())?;
        }
        if search_text(value).is_some() {
            let long =
                rows.iter().filter_map(|r| search_text(&r.value)).any(|t| t.len() > TRIGRAM_MAX);
            self.set_member(LONG_TEXTS, Some(field), id, long)?;
        }
        if let (Some((target, tid)), false) = (target, eventful) {
            let held = rows.iter().any(|r| target_of(&r.value) == Some(target));
            self.chunk_member(SETS, &referrers_prefix(field, tid), id, held)?;
        }
        match value {
            Value::TreeRef { parent: Some(parent), .. } => {
                self.derive_descendants(uuid, id, field, *parent, delta > 0)?;
            }
            Value::HierarchyRef { parent: Some(parent), .. } => {
                self.derive_hierarchy(uuid, id, field, *parent, delta > 0)?;
            }
            _ => {}
        }
        Ok(())
    }

    /// `id` holds `valued` values of `field` now that rows came or went, and
    /// held `before` of them before the write — however many rows the write
    /// changed: its count is written, bit by bit — every bit the count had or
    /// has, since one that was set may have to go.
    fn derive_count(
        &self,
        field: &str,
        id: u32,
        valued: u64,
        value: &Value,
        before: u64,
    ) -> Result<()> {
        if matches!(value, Value::Nothing) {
            return Ok(());
        }
        let bits = 64 - (valued | before).leading_zeros() as u8;
        anyhow::ensure!(bits <= COUNT_BITS, "{valued} values of '{field}' on one metarecord");
        for bit in 0..bits {
            self.chunk_member(SETS, &count_prefix(field, bit), id, valued >> bit & 1 == 1)?;
        }
        Ok(())
    }

    /// The ids holding a value of `field`, as the transaction sees them:
    /// those of any bit of the counts.
    fn holders_now(&self, field: &str) -> Result<RoaringBitmap> {
        let mut out = RoaringBitmap::new();
        for bit in 0..COUNT_BITS {
            out |= self.read_chunked(SETS, &count_prefix(field, bit))?;
        }
        Ok(out)
    }

    /// The ids holding a row of `field`, `Nothing` or not, as the transaction
    /// sees them.
    pub(super) fn holder_ids_now(&self, field: &str) -> Result<RoaringBitmap> {
        let mut out = self.holders_now(field)?;
        out |= self.read_chunked(SETS, &set_prefix(ABSENT, Some(field)))?;
        Ok(out)
    }

    /// The ids holding `value` in `field` as the transaction sees them — for
    /// `Nothing`, the field's `absent` set ([`crate::store::Rows::value_holder_ids`]).
    pub(super) fn value_holders_now(&self, field: &str, value: &Value) -> Result<RoaringBitmap> {
        match value_key(value) {
            Some(key) => self.effective(field, value, &key),
            None => self.read_chunked(SETS, &set_prefix(ABSENT, Some(field))),
        }
    }

    /// The ids holding more than `n` rows of `field` — its values, and the
    /// `Nothing` row it may hold beside them — as the transaction sees them:
    /// read off the counts, no record read (the query source's `rows_over`).
    pub(super) fn rows_over_now(&self, field: &str, n: u64) -> Result<RoaringBitmap> {
        let mut counts = Vec::with_capacity(COUNT_BITS as usize);
        for bit in 0..COUNT_BITS {
            counts.push(self.read_chunked(SETS, &count_prefix(field, bit))?);
        }
        let unknown = self.read_chunked(SETS, &set_prefix(ABSENT, Some(field)))?;
        // With a `Nothing` row, one value less makes as many rows.
        let mut over = counted_over(&counts, n);
        over |= match n.checked_sub(1) {
            Some(less) => counted_over(&counts, less) & &unknown,
            None => unknown,
        };
        Ok(over)
    }

    /// A field's trigram state, as the transaction has it.
    fn gram_state(&self, field: &str) -> Result<GramState> {
        if let Some(state) = self.grams.borrow().get(field) {
            return Ok(*state);
        }
        let state =
            GramState::decode(self.t.grams.get(&self.txn.borrow(), &gram_state_key(field))?)?;
        self.grams.borrow_mut().insert(field.to_string(), state);
        Ok(state)
    }

    /// For a text about to join a field whose trigrams are kept by value:
    /// its key, when the field may not hold that text yet — no row of the
    /// partition has it, or its key is a cut one, which another text could
    /// share. `None` when the text's trigrams are already written, or the
    /// field keeps them by metarecord.
    fn new_text(&self, field: &str, value: &Value) -> Result<Option<Vec<u8>>> {
        if self.gram_state(field)?.by_ids {
            return Ok(None);
        }
        let Some((part, key)) = text_part_key(value) else { return Ok(None) };
        let text = if part == VALUE { &key[1..] } else { &key[..] };
        if long_prefix(text).is_none()
            && self.chunked_exists(SETS, &part_posting_prefix(field, part, &key))?
        {
            return Ok(None);
        }
        Ok(Some(key))
    }

    /// The trigrams of a text `id` gained in `field`. By value: those of a
    /// text the field did not hold (`new_text`, its key), each written once —
    /// and the field turns by metarecord when its trigrams name too many
    /// texts ([`GRAM_VALUES_MAX`]), every holder pending. By metarecord: `id`
    /// joins the pending ones, whose trigrams a fold builds.
    fn derive_grams(
        &self,
        field: &str,
        id: u32,
        text: &str,
        new_text: Option<&[u8]>,
    ) -> Result<()> {
        let mut state = self.gram_state(field)?;
        if state.by_ids {
            return self.set_member(GRAM_PENDING, Some(field), id, true);
        }
        let Some(key) = new_text else { return Ok(()) };
        let before = state;
        {
            let mut w = self.txn.borrow_mut();
            for gram in trigrams(text) {
                let prefix = gram_values_prefix(field, &gram);
                let k = [&prefix[..], key].concat();
                if self.t.grams.get(&w, &k)?.is_some() {
                    continue;
                }
                if self.t.grams.prefix_iter(&w, &prefix)?.next().transpose()?.is_none() {
                    state.grams += 1;
                }
                self.t.grams.put(&mut w, &k, &[])?;
                state.entries += 1;
            }
        }
        if state.entries > GRAM_VALUES_MAX * state.grams {
            state.by_ids = true;
            state.leftovers = true;
            // No trigram is kept by metarecord yet: every holder is pending.
            let holders = self.holders_now(field)?;
            self.chunk_apply(SETS, &set_prefix(GRAM_PENDING, Some(field)), &holders, true)?;
        }
        if state != before {
            self.grams.borrow_mut().insert(field.to_string(), state);
            self.grams_changed.borrow_mut().insert(field.to_string());
        }
        Ok(())
    }

    /// Builds the trigrams the writes left pending (doc "Text search in the
    /// store"): in each field kept by metarecord, those of the pending ids'
    /// texts, as they are now; the entries the field wrote while it was by
    /// value go. What a text that changed or left wrote before stays — a
    /// candidate too many, never one too few.
    pub(super) fn fold_grams(&self) -> Result<()> {
        for field in self.gram_fields()? {
            self.fold_gram_field(&field)?;
        }
        Ok(())
    }

    /// The fields that have trigram state.
    pub(super) fn gram_fields(&self) -> Result<Vec<String>> {
        let mut fields: Vec<String> = Vec::new();
        for entry in self.t.grams.prefix_iter(&self.txn.borrow(), &[GRAM_STATE])? {
            let (name, _) = unesc(&entry?.0[1..])?;
            fields.push(String::from_utf8(name).context("a field name")?);
        }
        fields.extend(self.grams_changed.borrow().iter().cloned());
        fields.sort();
        fields.dedup();
        Ok(fields)
    }

    /// What [`Self::fold_gram_field`] has to do for `field`, as a size to
    /// rank it by: the ids pending, one more for leftovers to purge.
    pub(super) fn gram_fold_size(&self, field: &str) -> Result<u64> {
        let state = self.gram_state(field)?;
        if !state.by_ids {
            return Ok(0);
        }
        let pending = self.read_chunked(SETS, &set_prefix(GRAM_PENDING, Some(field)))?;
        Ok(pending.len() + u64::from(state.leftovers))
    }

    /// [`Self::fold_grams`] for one field.
    pub(super) fn fold_gram_field(&self, field: &str) -> Result<()> {
        let t = self.t;
        let field = field.to_string();
        {
            let mut state = self.gram_state(&field)?;
            if !state.by_ids {
                return Ok(());
            }
            let pending = self.read_chunked(SETS, &set_prefix(GRAM_PENDING, Some(&field)))?;
            for id in &pending {
                let uuid = match t.uuids.get(&self.txn.borrow(), &id.to_be_bytes())? {
                    Some(uuid) => uuid_of(uuid),
                    None => continue,
                };
                for row in Rows::rows_named(self, uuid, &field)? {
                    let Some(text) = search_text(&row.value).filter(|t| t.len() <= TRIGRAM_MAX)
                    else {
                        continue;
                    };
                    for gram in trigrams(&text) {
                        self.chunk_member(GRAMS, &gram_prefix(&field, &gram), id, true)?;
                    }
                }
            }
            self.chunk_apply(SETS, &set_prefix(GRAM_PENDING, Some(&field)), &pending, false)?;
            if state.leftovers {
                let prefix = [&[GRAM_VALUES][..], &name_key(&field)].concat();
                let mut w = self.txn.borrow_mut();
                let mut gone = Vec::new();
                for entry in t.grams.prefix_iter(&w, &prefix)? {
                    gone.push(entry?.0.to_vec());
                }
                for k in gone {
                    t.grams.delete(&mut w, &k)?;
                }
                state.leftovers = false;
                state.entries = 0;
                state.grams = 0;
                self.grams.borrow_mut().insert(field.clone(), state);
                self.grams_changed.borrow_mut().insert(field.clone());
            }
        }
        Ok(())
    }

    /// An edge `uuid` → `parent` of a hierarchy came (`add`) or went (doc
    /// "Hierarchy references"). Coming, the node and everything below it join
    /// the descendants of `parent` and of each of its ancestors, by every path
    /// up. Going cannot subtract: an ancestor may still reach those nodes
    /// another way. So the descendants of `parent` and of its ancestors — the
    /// only bitmaps the edge was in — are recomputed from their children, the
    /// stored bitmaps of the children outside that set standing for their
    /// subtrees.
    fn derive_hierarchy(
        &self,
        uuid: Uuid,
        id: u32,
        field: &str,
        parent: Uuid,
        add: bool,
    ) -> Result<()> {
        let ancestors = self.hierarchy_ancestors(field, parent)?;
        if add {
            let _ = uuid;
            let mut moving = self.read_chunked(SETS, &descendants_prefix(field, id))?;
            moving.insert(id);
            for ancestor in &ancestors {
                let prefix = descendants_prefix(field, self.target_id(ancestor.as_bytes())?);
                self.chunk_apply(SETS, &prefix, &moving, true)?;
            }
            return Ok(());
        }
        let mut recomputed = Vec::with_capacity(ancestors.len());
        for ancestor in &ancestors {
            recomputed.push(self.reachable_below(field, *ancestor, &ancestors)?);
        }
        for (ancestor, now) in ancestors.iter().zip(recomputed) {
            let prefix = descendants_prefix(field, self.target_id(ancestor.as_bytes())?);
            let before = self.read_chunked(SETS, &prefix)?;
            self.chunk_apply(SETS, &prefix, &(&before - &now), false)?;
            self.chunk_apply(SETS, &prefix, &(&now - &before), true)?;
        }
        Ok(())
    }

    /// `node` and every node above it in a hierarchy, by every parent, each
    /// once.
    fn hierarchy_ancestors(&self, field: &str, node: Uuid) -> Result<BTreeSet<Uuid>> {
        let mut seen = BTreeSet::from([node]);
        let mut todo = vec![node];
        while let Some(n) = todo.pop() {
            for row in Rows::rows_named(self, n, field)? {
                if let Value::HierarchyRef { parent: Some(p), .. } = row.value {
                    if seen.insert(p) {
                        todo.push(p);
                    }
                }
            }
        }
        Ok(seen)
    }

    /// The ids below `node`, walked down its children; a child outside
    /// `walked` is not walked into, its stored descendant bitmap read
    /// instead.
    fn reachable_below(
        &self,
        field: &str,
        node: Uuid,
        walked: &BTreeSet<Uuid>,
    ) -> Result<RoaringBitmap> {
        let mut out = RoaringBitmap::new();
        let mut seen = BTreeSet::from([node]);
        let mut todo = vec![node];
        while let Some(n) = todo.pop() {
            for (child, child_id) in self.children_of(field, n)? {
                out.insert(child_id);
                if !seen.insert(child) {
                    continue;
                }
                if walked.contains(&child) {
                    todo.push(child);
                } else {
                    out |= self.read_chunked(SETS, &descendants_prefix(field, child_id))?;
                }
            }
        }
        Ok(out)
    }

    /// The nodes placed directly under `parent` in `field`, with their ids.
    fn children_of(&self, field: &str, parent: Uuid) -> Result<Vec<(Uuid, u32)>> {
        let pid = self.target_id(parent.as_bytes())?;
        let prefix = [&pid.to_be_bytes()[..], &name_key(field)].concat();
        let r = self.txn.borrow();
        let mut out = Vec::new();
        for entry in self.t.kids.prefix_iter(&r, &prefix)? {
            let (k, _) = entry?;
            let id = dense(&k[prefix.len()..]);
            let uuid = self.t.uuids.get(&r, &id.to_be_bytes())?.context("a kid without a uuid")?;
            out.push((uuid_of(uuid), id));
        }
        Ok(out)
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
        let _ = uuid;
        let mut moving = self.read_chunked(SETS, &descendants_prefix(field, id))?;
        moving.insert(id);
        let mut up = Some(parent);
        for _ in 0..crate::log::MAX_TREE_DEPTH {
            let Some(ancestor) = up else { return Ok(()) };
            let above = self.target_id(ancestor.as_bytes())?;
            self.chunk_apply(SETS, &descendants_prefix(field, above), &moving, add)?;
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
        self.universe.borrow_mut().clear();
        self.grams.borrow_mut().clear();
        self.grams_changed.borrow_mut().clear();
        self.events.borrow_mut().clear();
        let mut w = self.txn.borrow_mut();
        let t = self.t;
        for db in [t.ids, t.uuids, t.sets, t.kids, t.grams, t.hist, t.events] {
            db.clear(&mut w)?;
        }
        Ok(())
    }
}

impl KvTxn<'_> {
    /// Removes from the primary tables what the deleted metarecords left
    /// there (doc "Deleting a metarecord in the store"): the forest keys that
    /// are no row's position any more. Held against the rows themselves, not
    /// the universe — a reindex is also what repairs the derived data.
    pub(super) fn drop_left_behind(&self) -> Result<()> {
        let t = self.t;
        let mut gone: Vec<(super::Db, Vec<u8>)> = Vec::new();
        {
            let r = self.txn.borrow();
            // The records of the metarecords that are gone, which stayed for a
            // restoration: held against the primary key of each uuid.
            for entry in t.records.iter(&r)? {
                let (k, _) = entry?;
                let exists = match t.uuids.get(&r, k)? {
                    Some(uuid) => t.metarecords.get(&r, uuid)?.is_some(),
                    None => false,
                };
                if !exists {
                    gone.push((t.records, k.to_vec()));
                }
            }
            // What the forest holds is read from the records that remain.
            let records_gone: std::collections::HashSet<Vec<u8>> =
                gone.iter().map(|(_, k)| k.clone()).collect();
            for entry in t.forest.iter(&r)? {
                let (k, v) = entry?;
                let (field, _) = unesc(k)?;
                let field = String::from_utf8(field).context("a field name")?;
                let record = match t.records.get(&r, &v[super::FOREST_ID..])? {
                    Some(rows) if !records_gone.contains(&v[super::FOREST_ID..]) => dec_rows(rows)?,
                    _ => Vec::new(),
                };
                let placed = record.iter().any(|row| {
                    row.name == field && KvTxn::forest_key(&field, &row.value).as_deref() == Some(k)
                });
                if !placed {
                    gone.push((t.forest, k.to_vec()));
                }
            }
        }
        let mut w = self.txn.borrow_mut();
        for (db, k) in gone {
            db.delete(&mut w, &k)?;
        }
        Ok(())
    }

    /// Re-files every record under its metarecord's uuid (16 bytes, where a
    /// dense id is 4): the first half of a renumbering, while the ids still
    /// say whose each record is. A batch of records in memory at a time.
    pub(super) fn file_records_by_uuid(&self) -> Result<()> {
        self.refile_records(4, |t, r, k| {
            let uuid = t.uuids.get(r, k)?.context("a record of no metarecord")?;
            Ok(uuid.to_vec())
        })
    }

    /// The second half: every record filed under a uuid goes under the dense
    /// id its metarecord has now.
    pub(super) fn file_records_by_id(&self) -> Result<()> {
        self.refile_records(16, |t, r, k| {
            let id = id_of(t, r, k)?.context("a record of a metarecord with no dense id")?;
            Ok(id.to_be_bytes().to_vec())
        })
    }

    /// Moves every record whose key is `len` bytes long to the key `to` gives
    /// it.
    fn refile_records(
        &self,
        len: usize,
        to: impl Fn(&Tables, &RoTxn<'_>, &[u8]) -> Result<Vec<u8>>,
    ) -> Result<()> {
        const BATCH: usize = 10_000;
        let t = self.t;
        let mut after: Option<Vec<u8>> = None;
        loop {
            let (last, moves) = {
                let r = self.txn.borrow();
                let lo = match &after {
                    Some(k) => std::ops::Bound::Excluded(k.as_slice()),
                    None => std::ops::Bound::Unbounded,
                };
                let range = (lo, std::ops::Bound::Unbounded);
                let mut moves = Vec::new();
                let mut last = None;
                for entry in t.records.range(&r, &range)? {
                    let (k, v) = entry?;
                    last = Some(k.to_vec());
                    if k.len() != len {
                        continue;
                    }
                    moves.push((k.to_vec(), to(&t, &r, k)?, v.to_vec()));
                    if moves.len() >= BATCH {
                        break;
                    }
                }
                (last, moves)
            };
            let Some(last) = last else { return Ok(()) };
            after = Some(last);
            let mut w = self.txn.borrow_mut();
            for (from, new, value) in &moves {
                t.records.delete(&mut w, from)?;
                t.records.put(&mut w, new, value)?;
            }
        }
    }

    /// Writes into every forest entry the dense id its metarecord has now:
    /// after a reindex, which numbers the metarecords again, and for a store
    /// from before the entries held one. A batch of entries in memory at a
    /// time.
    pub(super) fn stamp_forest_ids(&self) -> Result<()> {
        const BATCH: usize = 10_000;
        let t = self.t;
        let mut after: Option<Vec<u8>> = None;
        loop {
            let batch: Vec<(Vec<u8>, Vec<u8>)> = {
                let r = self.txn.borrow();
                let lo = match &after {
                    Some(k) => std::ops::Bound::Excluded(k.as_slice()),
                    None => std::ops::Bound::Unbounded,
                };
                let range = (lo, std::ops::Bound::Unbounded);
                let mut batch = Vec::new();
                for entry in t.forest.range(&r, &range)?.take(BATCH) {
                    let (k, v) = entry?;
                    let id = id_of(&t, &r, &v[..16])?.with_context(|| {
                        format!("a position of {}, which has no dense id", uuid_of(v))
                    })?;
                    batch.push((k.to_vec(), [&v[..16], &id.to_be_bytes()[..]].concat()));
                }
                batch
            };
            let Some((last, _)) = batch.last() else { return Ok(()) };
            after = Some(last.clone());
            let mut w = self.txn.borrow_mut();
            for (k, v) in &batch {
                t.forest.put(&mut w, k, v)?;
            }
        }
    }
}

// ── Rebuilding and checking ─────────────────────────────────────────────────

/// The positioned metarecords in forest order: each forest walked depth
/// first from its roots, a node's children in key order (their name bytes),
/// the forest of the most positions first (ties by field name). A record
/// comes at its first position; a subtree's ids then follow one another, so
/// its bitmaps compress to a few runs however its records were discovered
/// (doc "Store tables").
fn forest_order(t: &Tables, r: &RoTxn<'_>) -> Result<Vec<Uuid>> {
    // Each forest's key prefix (its field name, terminated) and its size.
    let mut forests: Vec<(Vec<u8>, u64)> = Vec::new();
    for e in t.forest.iter(r)? {
        let (k, _) = e?;
        // The field's end is its `00 00` terminator (an escaped `00` is
        // followed by `FF`).
        let end = (0..k.len().saturating_sub(1))
            .find(|&i| k[i] == 0 && k[i + 1] == 0)
            .context("a forest key without its field")?;
        match forests.last_mut() {
            Some((prefix, n)) if prefix[..] == k[..end + 2] => *n += 1,
            _ => forests.push((k[..end + 2].to_vec(), 1)),
        }
    }
    forests.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let mut order = Vec::new();
    for (field, _) in &forests {
        // Per forest: a node of several parents (`hierarchy_ref`) is walked
        // once.
        let mut walked = BTreeSet::new();
        let mut stack = vec![super::ROOT];
        while let Some(node) = stack.pop() {
            if node != super::ROOT {
                order.push(uuid_of(&node));
            }
            let mut children = Vec::new();
            for e in t.forest.prefix_iter(r, &[&field[..], &node].concat())? {
                let (_, v) = e?;
                let child: [u8; 16] = v[..16].try_into().expect("a 16-byte uuid");
                if walked.insert(child) {
                    children.push(child);
                }
            }
            // Depth first: the first child's subtree before the second's.
            stack.extend(children.into_iter().rev());
        }
    }
    Ok(order)
}

impl KvStore {
    /// Derives every derived key space again from the primary data, in one
    /// transaction. Dense ids follow forest order ([`forest_order`]), then,
    /// for the metarecords in no forest, discovery order: the order of each
    /// one's first row (then the metarecords without rows). The records,
    /// which are filed under their dense id, are filed under the new one.
    pub fn reindex(&mut self) -> Result<()> {
        let txn = KvTxn::new(self)?;
        // What the lists hold is written into the records first: the records
        // are filed again below, and the rows are derived from them.
        txn.fold_all_mass()?;
        // The records a deletion gave back are named by id: copy what the log
        // reads in them before the ids change.
        txn.freeze_all_given_back()?;
        // Before the ids are numbered again.
        txn.stubs_to_uuids()?;
        txn.drop_left_behind()?;
        let order = {
            let r = txn.txn.borrow();
            let mut seen = BTreeSet::new();
            let mut order = Vec::new();
            for u in forest_order(&txn.t, &r)? {
                if seen.insert(u) {
                    order.push(u);
                }
            }
            for entry in txn.t.records.iter(&r)? {
                let (id, _) = entry?;
                let uuid = txn.t.uuids.get(&r, id)?.context("a record of no metarecord")?;
                let u = uuid_of(uuid);
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
        // The records are filed under their uuid while the ids are numbered
        // again, then under the new dense ids.
        txn.file_records_by_uuid()?;
        txn.clear_derived()?;
        txn.meta_put("next_id", 1)?;
        txn.next(NUMBERING)?;
        for &uuid in &order {
            txn.derive_created(uuid)?;
        }
        txn.file_records_by_id()?;
        txn.stamp_forest_ids()?;
        for &uuid in &order {
            for row in Rows::rows(&txn, uuid)? {
                // Nothing was counted yet: the derived tables were cleared.
                txn.derive_row(uuid, &row.name, &row.value, 1, 0)?;
            }
        }
        txn.fold_grams()?;
        txn.meta_put("derived", DERIVED_VERSION)?;
        txn.meta_put("kept", 0)?;
        txn.finish()?;
        self.kept.store(false, std::sync::atomic::Ordering::Relaxed);
        self.mass.store(false, std::sync::atomic::Ordering::Relaxed);
        self.deletes.store(false, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// Integrates what the writes left pending for a quiet moment: the
    /// trigrams of the fields kept by metarecord (doc "Text search in the
    /// store"). A reindex does as much.
    /// Folds the biggest list pending — a part's entries, a field's events
    /// or a field's pending trigrams — in one transaction of its own, so
    /// that what waits for the store waits for one list (doc "Folding in
    /// idle time"). Returns whether there was one.
    pub fn fold_step(&mut self) -> Result<bool> {
        let txn = KvTxn::new(self)?;
        let folded = txn.fold_biggest()?;
        txn.finish()?;
        if folded {
            self.mass.store(self.pending_mass()? > 0, std::sync::atomic::Ordering::Relaxed);
            let r = self.env.read_txn()?;
            let deleting = self
                .t
                .mass
                .prefix_iter(&r, &super::mass::DELETES_PART.to_be_bytes())?
                .next()
                .is_some();
            self.deletes.store(deleting, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(folded)
    }

    /// How worn the store is: what writes left pending, and the ids no
    /// metarecord holds (doc "Folding in idle time").
    pub fn wear(&self) -> Result<Wear> {
        let r = self.env.read_txn()?;
        let mut wear = Wear::default();
        let mut parts = std::collections::BTreeSet::new();
        for entry in self.t.mass.iter(&r)? {
            let (k, v) = entry?;
            wear.mass_entries += 1;
            wear.mass_bytes += (k.len() + v.len()) as u64;
            parts.insert(u32::from_be_bytes(k[..4].try_into().expect("a part")));
        }
        wear.mass_parts = parts.len();
        let mut fields = std::collections::BTreeSet::new();
        for entry in self.t.events.iter(&r)? {
            let (k, v) = entry?;
            wear.events += 1;
            wear.event_bytes += (k.len() + v.len()) as u64;
            fields.insert(unesc(k)?.0);
        }
        wear.event_fields = fields.len();
        for entry in self.t.sets.prefix_iter(&r, &[GRAM_PENDING])? {
            wear.trigrams_pending += decode_set(entry?.1)?.len();
        }
        // The counter holds the next id to hand out; the first is 1.
        wear.ids_allocated =
            (self.t.meta.get(&r, b"next_id")?.map_or(1, from_be) - 1).max(0) as u64;
        let live = self.t.metarecords.len(&r)?;
        wear.ids_live = live;
        Ok(wear)
    }

    pub fn fold(&mut self) -> Result<()> {
        let txn = KvTxn::new(self)?;
        txn.fold_all_mass()?;
        txn.fold_all_events()?;
        txn.fold_grams()?;
        txn.finish()?;
        self.mass.store(false, std::sync::atomic::Ordering::Relaxed);
        self.deletes.store(false, std::sync::atomic::Ordering::Relaxed);
        Ok(())
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
        let mut hist = Histograms::new();
        let mut grams = GramCheck::default();
        let first = Some((&mut hist, &mut grams));
        self.walk_entries(&r, &mut diff, first, &mut |side, entry| {
            sums.add(side, &entry);
            Ok(())
        })?;
        let wanted = sums.differing();
        if wanted.iter().any(|buckets| !buckets.is_empty()) {
            let mut detail = Detail::new(wanted);
            // The first pass already said what it found outside the entries.
            let mut said = Diff::default();
            self.walk_entries(&r, &mut said, None, &mut |side, entry| {
                detail.add(&self.t, &r, side, &entry)
            })?;
            detail.report(&mut diff);
        }
        // A deletion pending leaves the rows it deleted counted: the fold gives
        // them back.
        if !self.deletes_pending() {
            self.check_histograms(&r, hist, &mut diff)?;
        }
        Ok(diff.lines)
    }

    /// Every entry the primary data derives ([`Side::Expected`]) and every
    /// entry the derived tables hold ([`Side::Got`]), postings aside, handed
    /// to `sink` one at a time. What cannot be an entry — a metarecord with
    /// no dense id, an id the two mappings disagree on — goes to `diff`.
    /// `first`, on the first of the two passes, gets the histograms the rows
    /// derive — a count per bucket, not an entry per row, small enough to be
    /// held — and holds the trigrams to the texts ([`GramCheck`]).
    fn walk_entries(
        &self,
        r: &RoTxn<'_>,
        diff: &mut Diff,
        first: Option<(&mut Histograms, &mut GramCheck)>,
        sink: &mut dyn FnMut(Side, Entry<'_>) -> Result<()>,
    ) -> Result<()> {
        let t = &self.t;
        let (mut hist, mut grams) = match first {
            Some((hist, grams)) => (Some(hist), Some(grams)),
            None => (None, None),
        };

        // The universe and the id mappings. `live`: the ids of the
        // metarecords that exist — what the universe must be. A deleted
        // metarecord keeps its id and its entries everywhere else (doc
        // "Deleting a metarecord in the store"), so the derived tables are
        // held to the rows *on those ids*.
        let mut metarecords = 0u64;
        let mut live = RoaringBitmap::new();
        // A deletion of many records leaves their keys until the fold: those
        // whose id is out of the universe are not metarecords any more.
        let deleting = self.deletes_pending();
        let stored = if deleting { read_set(t, r, UNIVERSE, None)? } else { RoaringBitmap::new() };
        for entry in t.metarecords.iter(r)? {
            let (k, _) = entry?;
            match id_of(t, r, &k[..16])? {
                Some(id) if deleting && !stored.contains(id) => continue,
                _ => {}
            }
            metarecords += 1;
            match id_of(t, r, &k[..16])? {
                Some(id) => {
                    live.insert(id);
                    sink(Side::Expected, Entry::set(UNIVERSE, "", id))?
                }
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
        // An id never given names nothing, deleted or not: it is held to
        // the rows like one that exists, and found unexpected.
        let next = t.meta.get(r, b"next_id")?.map_or(1, from_be);
        let sifted = |id: u32| !live.contains(id) && i64::from(id) < next;
        if mapped < metarecords {
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
        let mut parent_of: HashMap<(u32, Uuid), (Vec<Uuid>, Option<u32>)> = HashMap::new();
        let pending = self.mass_pending();
        let mut parts = super::mass::Parts::default();
        for record_id in super::mass::record_ids(t, r, pending)? {
            let k = record_id.to_be_bytes();
            let Some(named) = t.uuids.get(r, &k)? else {
                diff.push(format!("records: dense id {record_id} names no metarecord"));
                continue;
            };
            // An id only an entry names may be a metarecord's that is gone.
            if t.metarecords.get(r, named)?.is_none() || (deleting && !live.contains(record_id)) {
                continue;
            }
            let uuid: [u8; 16] = named.try_into().expect("a 16-byte uuid");
            let id = Some(record_id);
            if id_of(t, r, &uuid)? != id {
                diff.push(format!(
                    "rows of {}: filed under dense id {record_id}, which the ids do not give it",
                    Uuid::from_bytes(uuid)
                ));
            }
            let mut record = parts.rows(t, r, record_id, pending)?;
            // The fields of a record come together, in name order.
            record.sort_by(|a, b| a.name.cmp(&b.name));
            for row in record {
                if group.uuid != uuid || group.field != row.name {
                    if let Some(grams) = grams.as_deref_mut() {
                        grams.check(t, r, &group, diff)?;
                    }
                    group.flush(sink)?;
                    group = Group { uuid, id, field: row.name.clone(), ..Group::default() };
                }
                if let Some(key) = value_key(&row.value) {
                    if let (Some(hist), Some(bucket)) = (hist.as_deref_mut(), hist_bucket(&key)) {
                        *hist.entry(hist_key(&row.name, &bucket)).or_default() += 1;
                    }
                    if row.value.position().is_none() {
                        *group.parts.entry((VALUE, key)).or_default() += 1;
                    }
                }
                if let Some(key) = name_part_key(&row.value) {
                    *group.parts.entry((NAME, key)).or_default() += 1;
                }
                // What the row points at, by the dense id it is filed under.
                let target = match target_of(&row.value) {
                    Some(target) if target == ZERO => Some(NO_TARGET),
                    Some(target) => {
                        let id = id_of(t, r, &target)?;
                        if id.is_none() {
                            diff.push(format!(
                                "ids: {} is pointed at and has no dense id",
                                uuid_of(&target)
                            ));
                        }
                        id
                    }
                    None => None,
                };
                if let Some(target) = target {
                    group.targets.insert(target);
                }
                if let (Some((parent, _)), Some(target)) = (row.value.position(), target) {
                    let parent = &parent;
                    *group.kids.entry(target).or_default() += 1;
                    if let Some(parent_id) = Some(target).filter(|id| live.contains(*id)) {
                        match parents.get_mut(&row.name) {
                            Some(set) => {
                                set.insert(parent_id);
                            }
                            None => {
                                parents.insert(
                                    row.name.clone(),
                                    RoaringBitmap::from_iter([parent_id]),
                                );
                            }
                        }
                    }
                    if let Some(p) = parent {
                        let next = fields.len() as u32;
                        let field = *fields.entry(row.name.clone()).or_insert(next);
                        let entry = parent_of.entry((field, Uuid::from_bytes(uuid))).or_default();
                        entry.0.push(*p);
                        entry.1 = id;
                    }
                }
                if let Some(text) = search_text(&row.value) {
                    if text.len() <= TRIGRAM_MAX {
                        let (_, key) = text_part_key(&row.value).expect("a text has a key");
                        group.grams.extend(trigrams(&text).into_iter().map(|g| (g, key.clone())));
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
        }
        if let Some(grams) = grams {
            grams.check(t, r, &group, diff)?;
        }
        group.flush(sink)?;
        for (field, set) in &parents {
            for id in set {
                sink(Side::Expected, Entry::set(PARENTS, field, id))?;
            }
        }
        let names: HashMap<u32, &str> =
            fields.iter().map(|(name, i)| (*i, name.as_str())).collect();
        // Every ancestor once, by every parent: a forest's node has one, a
        // hierarchy's several (doc "Hierarchy references").
        for ((field, _), (first, id)) in &parent_of {
            let Some(id) = *id else { continue };
            let mut seen: BTreeSet<Uuid> = BTreeSet::new();
            let mut todo: Vec<Uuid> = first.clone();
            while let Some(ancestor) = todo.pop() {
                if !seen.insert(ancestor) {
                    continue;
                }
                // An ancestor without an id was reported with the row that
                // points at it.
                if let Some(above) = id_of(t, r, ancestor.as_bytes())? {
                    let entry = Entry {
                        space: DESCENDANT,
                        tag: 0,
                        field: names[field],
                        key: &above.to_be_bytes(),
                        id,
                        count: 1,
                    };
                    sink(Side::Expected, entry)?;
                }
                if let Some((parents, _)) = parent_of.get(&(*field, ancestor)) {
                    todo.extend(parents.iter().copied());
                }
            }
        }
        drop(parent_of);

        // The fields with events pending (doc "Mass writes in the store"):
        // what is stored of their values is behind by them, and is held to
        // the rows as replayed — each event list as it reads on the value
        // partition, and on the referrers.
        let mut pending: HashMap<String, (Vec<events::Event>, Vec<events::Event>)> = HashMap::new();
        for entry in t.events.iter(r)? {
            let (name, _) = unesc(entry?.0)?;
            let field = String::from_utf8(name).context("a field name")?;
            if let std::collections::hash_map::Entry::Vacant(slot) = pending.entry(field) {
                let list = events::read_events(t, r, slot.key())?;
                let by_target = events::by_target(&list, |uuid| id_of(t, r, uuid))?;
                slot.insert((list, by_target));
            }
        }

        // Got: the derived tables, as they are.
        for entry in t.sets.iter(r)? {
            let (k, v) = entry?;
            let kind = k[0];
            if kind == POSTINGS || kind == NAME_POSTINGS {
                // A posting is an entry per holder of its value or name.
                let (name, rest) = unesc(&k[1..])?;
                let field = std::str::from_utf8(&name).context("a field name")?;
                let key = &rest[..rest.len() - 2];
                let tag = if kind == POSTINGS { VALUE } else { NAME };
                let replayed = pending.get(field).filter(|_| tag == VALUE);
                for id in decode_set(v)? {
                    if replayed.is_some_and(|(e, _)| !events::replay_one(e, key, id, true)) {
                        continue;
                    }
                    if !sifted(id) {
                        let entry = Entry { space: PART, tag, field, key, id, count: 1 };
                        sink(Side::Got, entry)?;
                    }
                }
                continue;
            }
            if kind == GRAM_PENDING || kind == RESERVED {
                // Not an equality: see `GramCheck`; a reserved id is no
                // metarecord's.
                continue;
            }
            let (name, rest) =
                if kind == UNIVERSE { (Vec::new(), &k[1..]) } else { unesc(&k[1..])? };
            let field = std::str::from_utf8(&name).context("a field name")?;
            let (space, tag, key) = match kind {
                REFERRERS => (REFERRER, 0, &rest[..4]),
                DESCENDANTS => (DESCENDANT, 0, &rest[..4]),
                // One bit of the counts: a set of its own.
                COUNT => (SET, COUNT_TAG + rest[0], &rest[..0]),
                _ => (SET, kind, &rest[..0]),
            };
            let replayed = pending.get(field).filter(|_| kind == REFERRERS);
            for id in decode_set(v)? {
                if replayed.is_some_and(|(_, e)| !events::replay_one(e, key, id, true)) {
                    continue;
                }
                if kind == UNIVERSE || !sifted(id) {
                    sink(Side::Got, Entry { space, tag, field, key, id, count: 1 })?;
                }
            }
        }
        // What the events add and nothing stores yet, under the values they
        // name.
        for (field, (list, by_target)) in &pending {
            for key in events::named(list) {
                let run = posting_prefix(field, &key);
                let mut stored = RoaringBitmap::new();
                for entry in t.sets.prefix_iter(r, &run)? {
                    let (k, v) = entry?;
                    if k.len() == run.len() + 2 {
                        stored |= decode_set(v)?;
                    }
                }
                for id in events::replay(list, &key, stored.clone()) - stored {
                    if !sifted(id) {
                        let entry =
                            Entry { space: PART, tag: VALUE, field, key: &key, id, count: 1 };
                        sink(Side::Got, entry)?;
                    }
                }
            }
            for key in events::named(by_target) {
                let mut stored = RoaringBitmap::new();
                for entry in t.sets.prefix_iter(r, &referrers_prefix(field, dense(&key)))? {
                    stored |= decode_set(entry?.1)?;
                }
                for id in events::replay(by_target, &key, stored.clone()) - stored {
                    if !sifted(id) {
                        let entry =
                            Entry { space: REFERRER, tag: 0, field, key: &key, id, count: 1 };
                        sink(Side::Got, entry)?;
                    }
                }
            }
        }
        for entry in t.kids.iter(r)? {
            let (k, v) = entry?;
            let (name, rest) = unesc(&k[4..])?;
            let entry = Entry {
                space: KID,
                tag: 0,
                field: std::str::from_utf8(&name).context("a field name")?,
                key: &k[..4],
                id: dense(rest),
                count: from_be(v),
            };
            if !sifted(entry.id) {
                sink(Side::Got, entry)?;
            }
        }
        Ok(())
    }
}

/// The histograms a check derives from the rows: rows per [`hist_key`].
type Histograms = BTreeMap<Vec<u8>, i64>;

impl KvStore {
    /// The histograms, against what the rows derive (`expected`), bucket by
    /// bucket.
    fn check_histograms(&self, r: &RoTxn<'_>, expected: Histograms, diff: &mut Diff) -> Result<()> {
        let show = |key: &[u8]| -> Result<String> {
            let (name, bucket) = unesc(key)?;
            Ok(format!("({:?}, {bucket:?})", String::from_utf8_lossy(&name)))
        };
        // A field with events pending counts the rows a replacement removed
        // until its fold: its histogram is not held to the rows.
        let mut behind: BTreeSet<Vec<u8>> = BTreeSet::new();
        for entry in self.t.events.iter(r)? {
            let (name, _) = unesc(entry?.0)?;
            behind.insert(name);
        }
        let settled = |key: &[u8]| unesc(key).is_ok_and(|(name, _)| !behind.contains(&name));
        // The posting keys of each bucket, counted from the postings.
        let mut expected = expected;
        for entry in self.t.sets.prefix_iter(r, &[POSTINGS])? {
            let (k, _) = entry?;
            let (name, rest) = unesc(&k[1..])?;
            if let (Some(bucket), Ok(field)) =
                (hist_bucket(&rest[..rest.len() - 2]), String::from_utf8(name))
            {
                *expected.entry(hist_chunks_key(&field, &bucket)).or_default() += 1;
            }
        }
        let mut expected = expected.into_iter().filter(|(k, _)| settled(k)).peekable();
        for entry in self.t.hist.iter(r)? {
            let (k, v) = entry?;
            if !settled(k) {
                continue;
            }
            let got = from_be(v);
            while let Some((want_k, want)) = expected.next_if(|(want_k, _)| want_k[..] < *k) {
                diff.push(format!("histogram missing: {}: {want}", show(&want_k)?));
            }
            match expected.next_if(|(want_k, _)| want_k[..] == *k) {
                Some((_, want)) if want == got => {}
                Some((_, want)) => {
                    diff.push(format!("histogram of {}: {got} rows, expected {want}", show(k)?))
                }
                None => diff.push(format!("histogram unexpected: {}: {got}", show(k)?)),
            }
        }
        for (want_k, want) in expected {
            diff.push(format!("histogram missing: {}: {want}", show(&want_k)?));
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
const SPACES: [&str; 5] = ["set", "referrer", "descendant", "part", "kid"];
const SET: usize = 0;
/// The tag of a set holding a bit of the counts ([`COUNT`]): this, plus the
/// bit's number — past every set kind.
const COUNT_TAG: u8 = 100;
const REFERRER: usize = 1;
const DESCENDANT: usize = 2;
const PART: usize = 3;
const KID: usize = 4;

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
    /// A partition key, or the uuid a referrer, a descendant or a child is
    /// filed under.
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
        // What the entry is filed under, for the key spaces keyed by a
        // target's dense id.
        let target = || -> Result<String> {
            let id = dense(self.key);
            Ok(match t.uuids.get(r, self.key)? {
                _ if id == NO_TARGET => "no parent".to_string(),
                Some(uuid) => uuid_of(uuid).to_string(),
                None => format!("dense id {id} (no uuid)"),
            })
        };
        Ok(match self.space {
            SET => format!("({}, {field:?}, {who})", self.tag),
            REFERRER | DESCENDANT => format!("({field:?}, {}, {who})", target()?),
            PART => format!("({field:?}, {}, {:?}, {who}): {}", self.tag, self.key, self.count),
            _ => format!("({}, {field:?}, {who}): {}", target()?, self.count),
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
    /// Rows per parent, by the parent's dense id.
    kids: BTreeMap<u32, i64>,
    /// Each trigram of the field's short texts, with the text's key
    /// ([`text_part_key`]).
    grams: BTreeSet<([u8; 3], Vec<u8>)>,
    /// The dense ids of what the rows point at.
    targets: BTreeSet<u32>,
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
        // A posting holds a record once, however many rows have the key.
        for (part, key) in self.parts.keys() {
            emit(PART, *part, key, 1)?;
        }
        for (parent, rows) in &self.kids {
            emit(KID, 0, &parent.to_be_bytes(), *rows)?;
        }
        for target in &self.targets {
            emit(REFERRER, 0, &target.to_be_bytes(), 1)?;
        }
        for (kind, holds) in [(ABSENT, self.absent), (LONG_TEXTS, self.long)] {
            if holds {
                emit(SET, kind, &[], 1)?;
            }
        }
        // The count of values, a set per bit.
        for bit in 0..COUNT_BITS {
            if self.valued >> bit & 1 == 1 {
                emit(SET, COUNT_TAG + bit, &[], 1)?;
            }
        }
        Ok(())
    }
}

/// The trigrams, held to the texts (doc "Text search in the store"). They
/// are not compared entry by entry as the other key spaces are: a text that
/// changed or went leaves its trigrams, so the tables hold more than the rows
/// derive. What must hold is that no text lacks one — by value, an entry per
/// trigram of each text; by metarecord, the id in each trigram's bitmap,
/// unless it awaits a fold.
#[derive(Default)]
struct GramCheck {
    /// Per field: its state, and its pending ids.
    fields: HashMap<String, (GramState, RoaringBitmap)>,
    /// The chunks last decoded, by their key.
    chunks: HashMap<Vec<u8>, RoaringBitmap>,
}

impl GramCheck {
    fn check(&mut self, t: &Tables, r: &RoTxn<'_>, group: &Group, diff: &mut Diff) -> Result<()> {
        let Some(id) = group.id else { return Ok(()) };
        if group.grams.is_empty() {
            return Ok(());
        }
        let field = group.field.as_str();
        if !self.fields.contains_key(field) {
            let state = GramState::decode(t.grams.get(r, &gram_state_key(field))?)?;
            let mut pending = RoaringBitmap::new();
            for entry in t.sets.prefix_iter(r, &set_prefix(GRAM_PENDING, Some(field)))? {
                pending |= decode_set(entry?.1)?;
            }
            self.fields.insert(field.to_string(), (state, pending));
        }
        let (state, pending) = &self.fields[field];
        if !state.by_ids {
            for (gram, key) in &group.grams {
                let k = [&gram_values_prefix(field, gram)[..], key].concat();
                if t.grams.get(r, &k)?.is_none() {
                    let gram = String::from_utf8_lossy(gram);
                    diff.push(format!("gram missing: ({field:?}, {gram:?}, text key {key:?})"));
                }
            }
            return Ok(());
        }
        if pending.contains(id) {
            return Ok(());
        }
        let mut last = None;
        for (gram, _) in &group.grams {
            if last.replace(gram) == Some(gram) {
                continue;
            }
            let k = [&gram_prefix(field, gram)[..], &((id >> 16) as u16).to_be_bytes()].concat();
            if !self.chunks.contains_key(&k) {
                if self.chunks.len() >= 1024 {
                    self.chunks.clear();
                }
                let chunk = match t.grams.get(r, &k)? {
                    Some(bytes) => decode_set(bytes)?,
                    None => RoaringBitmap::new(),
                };
                self.chunks.insert(k.clone(), chunk);
            }
            if !self.chunks[&k].contains(id) {
                let gram = String::from_utf8_lossy(gram);
                diff.push(format!(
                    "gram missing: ({field:?}, {gram:?}, {})",
                    Uuid::from_bytes(group.uuid)
                ));
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
        for space in [REFERRER, DESCENDANT, SET, PART, KID] {
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

    /// A root, a folder and files under it, with texts and a value they share.
    fn populated() -> (KvStore, crate::kvstore::TestDir) {
        let (mut store, dir) = test_store();
        let tree = |parent, name: &str| Value::TreeRef { parent, name: TreeName::from(name) };
        let mut w = Writer::begin(&mut store, None).unwrap();
        let root = w.create_metarecord(vec![Field::new("loc", tree(None, ""))]).unwrap().uuid;
        let folder =
            w.create_metarecord(vec![Field::new("loc", tree(Some(root), "dir"))]).unwrap().uuid;
        for i in 0..12 {
            w.create_metarecord(vec![
                Field::new("loc", tree(Some(folder), &format!("file{i}.txt"))),
                Field::new("kind", Value::String("text".into())),
                Field::new("title", Value::String(format!("title number {i}"))),
                Field::new("link", Value::Ref(folder)),
                Field::new("size", Value::Int(i as i64 * 1_000)),
            ])
            .unwrap();
        }
        w.commit().unwrap();
        (store, dir)
    }

    /// Applies `tamper` to the tables, behind the store's back.
    fn tamper(store: &KvStore, tamper: impl FnOnce(&Tables, &mut super::super::Wtxn<'_>)) {
        let mut w = super::super::Wtxn::new(store.env.write_txn().unwrap(), false);
        tamper(&store.t, &mut w);
        w.into_parts().0.commit().unwrap();
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
    fn a_value_key_starts_with_its_type_tag() {
        let u = uuid::Uuid::new_v4();
        let values = [
            Value::Bool(true),
            Value::Int(-3),
            Value::Float(2.5),
            Value::String("x".into()),
            Value::DateTime(1_000),
            Value::Ref(u),
            Value::RefBase(u),
            Value::ExternalRef { repo: u, metarecord: u },
            Value::TreeRef { parent: Some(u), name: "n".into() },
        ];
        for v in values {
            let ty = crate::rows::encode_value(&v).value_type;
            assert_eq!(value_key(&v).unwrap()[0], type_tag(ty).unwrap(), "{ty}");
        }
        assert_eq!(type_tag("nothing"), None);
    }

    #[test]
    fn test_a_healthy_store_checks_clean() {
        let (store, _dir) = populated();
        assert_eq!(store.check_derived().unwrap(), Vec::<String>::new());
    }

    #[test]
    fn test_a_lost_trigram_chunk_is_named() {
        let (store, _dir) = populated();
        let key = first_key(&store, |t| &t.grams, &gram_values_prefix("title", b"num"));
        tamper(&store, |t, w| {
            t.grams.delete(w, &key).unwrap();
        });
        let diff = store.check_derived().unwrap();
        assert!(has(&diff, "gram missing: "), "{}", some(&diff));
        assert!(!has(&diff, "gram unexpected: "), "{}", some(&diff));
        assert!(diff.iter().all(|l| l.starts_with("gram ")), "only the trigrams differ: {diff:?}");
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
            ("set missing: ", count_prefix("kind", 0)),
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
    fn test_postings_are_held_to_the_rows() {
        let (store, _dir) = populated();
        let posting = posting_prefix("kind", &value_key(&Value::String("text".into())).unwrap());
        let key = first_key(&store, |t| &t.sets, &posting);

        // One holder short: that holder's entry is missing.
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
        assert!(has(&diff, "part missing: "), "{}", some(&diff));
        assert!(!has(&diff, "part unexpected: "), "{}", some(&diff));

        // Gone: every holder's entry is.
        tamper(&store, |t, w| {
            t.sets.delete(w, &key).unwrap();
        });
        let diff = store.check_derived().unwrap();
        assert!(has(&diff, "part missing: "), "{}", some(&diff));

        // Kept for a value nothing holds.
        let orphan =
            [&posting_prefix("kind", &value_key(&Value::Bool(true)).unwrap())[..], &[0, 0]]
                .concat();
        tamper(&store, |t, w| {
            t.sets.put(w, &key, &short).unwrap();
            t.sets.put(w, &orphan, &short).unwrap();
        });
        let diff = store.check_derived().unwrap();
        assert!(has(&diff, "part unexpected: "), "{}", some(&diff));
    }

    #[test]
    fn test_a_histogram_is_required_and_exact() {
        let (store, _dir) = populated();
        let key = first_key(&store, |t| &t.hist, &name_key("size"));

        // A count off by one.
        let n = {
            let r = store.env.read_txn().unwrap();
            from_be(store.t.hist.get(&r, &key).unwrap().unwrap())
        };
        tamper(&store, |t, w| {
            t.hist.put(w, &key, &be(n + 1)).unwrap();
        });
        let diff = store.check_derived().unwrap();
        assert!(has(&diff, "histogram of "), "{}", some(&diff));

        // A bucket gone.
        tamper(&store, |t, w| {
            t.hist.delete(w, &key).unwrap();
        });
        let diff = store.check_derived().unwrap();
        assert!(has(&diff, "histogram missing: "), "{}", some(&diff));

        // A bucket nothing fills.
        let bogus =
            [&name_key("size")[..], &hist_bucket(&value_key(&Value::Int(-7)).unwrap()).unwrap()]
                .concat();
        tamper(&store, |t, w| {
            t.hist.put(w, &key, &be(n)).unwrap();
            t.hist.put(w, &bogus, &be(3)).unwrap();
        });
        let diff = store.check_derived().unwrap();
        assert_eq!(diff.len(), 1, "{}", some(&diff));
        assert!(has(&diff, "histogram unexpected: "), "{}", some(&diff));
    }

    #[test]
    fn test_the_posting_keys_of_a_bucket_are_counted_and_exact() {
        let (store, _dir) = populated();
        // Twelve sizes, each its own value: a key a bucket holds per value.
        let bucket = hist_bucket(&value_key(&Value::Int(1_000)).unwrap()).unwrap();
        let key = hist_chunks_key("size", &bucket);
        let n = {
            let r = store.env.read_txn().unwrap();
            from_be(store.t.hist.get(&r, &key).unwrap().expect("the keys are counted"))
        };
        assert!(n >= 1, "{n}");

        // Off by one.
        tamper(&store, |t, w| {
            t.hist.put(w, &key, &be(n + 1)).unwrap();
        });
        let diff = store.check_derived().unwrap();
        assert!(has(&diff, "histogram of "), "{}", some(&diff));

        // Gone.
        tamper(&store, |t, w| {
            t.hist.delete(w, &key).unwrap();
        });
        let diff = store.check_derived().unwrap();
        assert!(has(&diff, "histogram missing: "), "{}", some(&diff));
    }

    #[test]
    fn test_a_bucket_is_a_prefix_of_the_value_key() {
        // A number: its type tag and 16 bits; a date: its tag and 34 bits,
        // the rest of its fifth byte cleared; none for another type.
        let n = value_key(&Value::Int(900_000)).unwrap();
        assert_eq!(hist_bucket(&n).unwrap(), n[..3].to_vec());
        let d = value_key(&Value::DateTime(1_700_000_000_000)).unwrap();
        let b = hist_bucket(&d).unwrap();
        assert_eq!(b.len(), 6);
        assert_eq!(b[..5], d[..5]);
        assert_eq!(b[5], d[5] & 0xC0);
        assert_eq!(hist_bucket(&value_key(&Value::String("x".into())).unwrap()), None);
        assert_eq!(hist_bucket(&value_key(&Value::Bool(true)).unwrap()), None);
    }

    #[test]
    fn test_an_id_naming_no_metarecord_is_reported() {
        let (store, _dir) = populated();
        let mut stray = RoaringBitmap::new();
        stray.insert(4_000_000);
        let mut bytes = Vec::new();
        stray.serialize_into(&mut bytes).unwrap();
        tamper(&store, |t, w| {
            let key = [&count_prefix("kind", 0)[..], &61u16.to_be_bytes()].concat();
            t.sets.put(w, &key, &bytes).unwrap();
        });
        let diff = store.check_derived().unwrap();
        assert!(has(&diff, "set unexpected: "), "{}", some(&diff));
        assert!(diff.iter().any(|l| l.contains("4000000")), "{}", some(&diff));
    }
}

/// How worn the store is (`KvStore::wear`): the lists writes left pending, which
/// a fold writes in and every read pays for until then, and the ids that
/// metarecords no longer hold, which only a reindex gathers again.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct Wear {
    /// Entries of the parts' lists, the parts they are in, their bytes.
    pub mass_entries: usize,
    pub mass_parts: usize,
    pub mass_bytes: u64,
    /// Pending events, the fields they are in, their bytes.
    pub events: usize,
    pub event_fields: usize,
    pub event_bytes: u64,
    /// Ids whose trigrams a fold has still to build.
    pub trigrams_pending: u64,
    /// Dense ids ever handed out, and those of metarecords that exist.
    pub ids_allocated: u64,
    pub ids_live: u64,
}

impl Wear {
    /// The bytes a fold would write in.
    pub fn pending_bytes(&self) -> u64 {
        self.mass_bytes + self.event_bytes + self.trigrams_pending
    }

    /// Ids no live metarecord holds (deleted ones, reserved ones).
    pub fn dead_ids(&self) -> u64 {
        self.ids_allocated.saturating_sub(self.ids_live)
    }

    /// The share of the ids that are dead: what a reindex would win back.
    pub fn dead_fraction(&self) -> f64 {
        if self.ids_allocated == 0 {
            0.0
        } else {
            self.dead_ids() as f64 / self.ids_allocated as f64
        }
    }
}
