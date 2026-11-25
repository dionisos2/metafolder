//! The derived key spaces (doc "Store tables"): what the query source
//! reads, written in each write's own transaction and rebuilt from the
//! primary data by [`KvStore::reindex`].
//!
//! Each table and its keys are in doc "Store tables"; [`Bitmaps`] names the
//! tables of bitmaps.
//!
//! A *set* is a bitmap of dense ids cut in chunks of 65 536 ids (the high 16
//! bits), so a write rewrites one chunk and a read unions a few: per field the
//! ids holding a value, a `nothing` (a key among the holders of the field's
//! values), and — on a `tree_ref` field — a child (`index_parents`). The forest
//! is `index_raw_holder` (field · parent id · name → id), so a node's children
//! are one prefix and its names sort like their bytes. A value is a *posting*:
//! its holders as one set in `index_value_holders` (field · value key · chunk),
//! so an equality reads a key per 65 536 holders and the values are in order of
//! their keys.
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

#[path = "index_writes.rs"]
pub(super) mod index_writes;

use anyhow::{bail, Context, Result};
use heed::RoTxn;
use metafolder_core::metarecord::Value;
use roaring::RoaringBitmap;
use uuid::Uuid;

use super::{be, from_be, name_key, uuid_of, KvStore, KvTxn, Tables};
use crate::index::keys::{dt_key, num_key};
use crate::store::Rows;

/// The format of the derived key spaces. A store stamped with another (or
/// none: a store from before they existed) is reindexed when it opens. 5:
/// descendant bitmaps for every forest (4 kept `mfr_path`'s only); 6: the
/// histograms; 7: the trigrams in two modes (doc "Text search in the store");
/// 8: what points at a metarecord filed under its dense id, and no target
/// partition; 9: the count of values in place of the `present` and `multi`
/// sets; 10: the fields' pending index writes; 11: every value a posting, and no value partition; 12: a position's name a posting too, and no `parts`; 13: a position's value no posting (the forest holds it); 14: the histograms count the posting keys of each bucket; 15: the record of a deleted metarecord stays until a reindex; 16: values in the compact encoding; 17: the universe and the reserved ids out of `sets` (the numbering primary), `field_types` an index of one-byte types; 18: a table per kind of set (doc "Store tables"); 19: the forest an index, `index_raw_holder` (field · parent id · name → id), and no `kids`; 23: the trigrams, the histograms and the pending index writes in tables of their own; 24: a pending index write filed by chunk (field · chunk · write number); 22: `nothing` a key among the holders of a field's values, and no `absent` set; 21: a position's name among the holders of its field's values; 20: the referrers of a position in `index_parent_holders`, a `ref`'s the holders of its value (none filed, no id reserved); 25: no byte of a former kind at the head of an index key, and no `FF` before a bucket in `index_histogram_keys`; 26: a mode per trigram, not per field — a list of texts by value, a bitmap of ids by metarecord; 27: a write files its ids in the bitmaps of its trigrams by metarecord, and no pending ids (`index_trigram_pending` goes); 28: a trigram turns by metarecord in the write that grows its list past the limit, and no list frozen for a fold (no mode 2, no trigrams turning in the state); 29: `uuids` holds the uuids of 4096 consecutive dense ids under one key (doc "Store tables").
pub(super) const DERIVED_VERSION: i64 = 29;

/// The sets of the index, by what they hold (doc "Store tables"): each is in
/// a table ([`IndexSet::table`]) under a key of its own ([`set_key`]). Their
/// numbers are no key's, only the check's tags.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub(super) enum IndexSet {
    /// The holders of `nothing` in a field: a key among the holders of its
    /// values ([`NOTHING_TAG`]).
    Nothing = 2,
    /// The ids that are the parent of a node in a `tree_ref` field.
    Parents = 3,
    /// The texts too long to be split in trigrams (see [`TRIGRAM_MAX`]).
    LongTexts = 4,
    /// The ids pointing at a metarecord in a field — a folder's children —
    /// keyed by field *and* the target's dense id ([`KvTxn::target_id`]): a
    /// folder filter is one bitmap, a read per 65 536 children.
    ParentHolders = 5,
    /// Every id below a node of a forest, keyed by field and the node's dense
    /// id — what makes a subtree one bitmap read (doc "The forest in the
    /// store"). Kept for every `tree_ref` field: a node holds one position per
    /// forest (doc "One position per forest"), so the tree is a tree and a
    /// move is set arithmetic.
    Descendants = 6,
    /// The holders of one value of a field (keyed by field and value key,
    /// then chunk): every value that has a holder. A value's holders are its
    /// run, in the order of the keys, and go with its last holder.
    ValueHolders = 7,
    /// How many non-`nothing` rows of a field each id holds, as one set per
    /// bit of the count (keyed by field and bit number): the ids holding a
    /// value are those in any of them, the ids holding several those in any
    /// but the first. A count is what a write to many records can keep
    /// without reading one: adding a value to the ids that lack it is an
    /// increment, which on bitmaps is a few set operations whatever the
    /// number of ids (doc "Store tables").
    ValueCounts = 11,
}

impl IndexSet {
    /// The table the set is in.
    pub(super) fn table(self) -> Bitmaps {
        match self {
            IndexSet::Parents => Bitmaps::Parents,
            IndexSet::LongTexts => Bitmaps::LongTexts,
            IndexSet::ParentHolders => Bitmaps::ParentHolders,
            IndexSet::Descendants => Bitmaps::Descendants,
            IndexSet::ValueHolders | IndexSet::Nothing => Bitmaps::ValueHolders,
            IndexSet::ValueCounts => Bitmaps::ValueCounts,
        }
    }
}

/// The type tag a position's name is filed under among the holders of the
/// values of its field: a text's — a position's `:value` is its name (doc
/// "Field aspects"), and a position field holds no other value.
pub(super) const NAME_TAG: u8 = 2;

/// The key of `nothing` among the holders of a field's values: past every
/// value's type tag, so that a range of one type never meets it; a walk of
/// every value steps over it.
pub(super) const NOTHING_TAG: u8 = 0xFF;

/// The bits a count is kept on: no record holds 2³² values of a field.
pub(super) const COUNT_BITS: u8 = 32;

/// The key of one bit of `field`'s counts, without its chunk.
pub(super) fn count_prefix(field: &str, bit: u8) -> Vec<u8> {
    [&name_key(field)[..], &[bit]].concat()
}

/// `field`'s counts, a set per bit, the lowest first — as many as the
/// largest count needs.
pub(super) fn read_counts(t: &Tables, r: &RoTxn<'_>, field: &str) -> Result<Vec<RoaringBitmap>> {
    let prefix = set_prefix(IndexSet::ValueCounts, Some(field));
    let mut out: Vec<RoaringBitmap> = Vec::new();
    for entry in Bitmaps::ValueCounts.db(t).prefix_iter(r, &prefix)? {
        let (k, v) = entry?;
        // The bit, then the chunk.
        if k.len() == prefix.len() + 3 {
            let bit = k[prefix.len()] as usize;
            if out.len() <= bit {
                out.resize(bit + 1, RoaringBitmap::new());
            }
            add_set(&mut out[bit], decode_set(v)?);
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

/// The dense id standing for "no parent": what a forest's roots are filed
/// under. No metarecord gets it — ids start at 1.
pub(super) const NO_TARGET: u32 = 0;

/// The key of a position in `index_raw_holder`: field · the parent's dense
/// id ([`NO_TARGET`] for a root) · the name (doc "The forest in the store").
/// Its value is the child's dense id.
pub(super) fn raw_key(field: &str, parent: u32, name: &[u8]) -> Vec<u8> {
    [&raw_prefix(field, parent)[..], &super::node_key(name)].concat()
}

/// The children of `parent` in `field`: the prefix of their keys.
pub(super) fn raw_prefix(field: &str, parent: u32) -> Vec<u8> {
    [&name_key(field)[..], &parent.to_be_bytes()].concat()
}

/// The dense id a position's parent is filed under, as committed: `None`
/// when the parent's uuid has none (so no position is filed under it).
pub(super) fn parent_id(t: &Tables, r: &RoTxn<'_>, parent: Option<Uuid>) -> Result<Option<u32>> {
    match parent {
        None => Ok(Some(NO_TARGET)),
        Some(p) => id_of(t, r, p.as_bytes()),
    }
}

/// [`raw_key`] of a position value, its parent's id as committed.
pub(super) fn raw_key_of(
    t: &Tables,
    r: &RoTxn<'_>,
    field: &str,
    value: &Value,
) -> Result<Option<Vec<u8>>> {
    let Some((parent, name)) = value.position() else { return Ok(None) };
    Ok(parent_id(t, r, parent)?.map(|pid| raw_key(field, pid, name.as_bytes())))
}

/// The key of the posting of a value key in `field`, without its chunk. A
/// value key is self-delimiting (fixed-width, or a text ended by its
/// terminator or marker and hash), so no posting's prefix is another's.
pub(super) fn posting_prefix(field: &str, key: &[u8]) -> Vec<u8> {
    part_posting_prefix(field, VALUE, key)
}

/// The key of the posting of `key` in a partition of `field`, without its chunk.
pub(super) fn part_posting_prefix(field: &str, part: u8, key: &[u8]) -> Vec<u8> {
    if part == NAME {
        return [&name_key(field)[..], &[NAME_TAG], key].concat();
    }
    [&name_key(field)[..], key].concat()
}

/// The key of the descendants of `node` in `field`, without its chunk.
pub(super) fn descendants_prefix(field: &str, node: u32) -> Vec<u8> {
    [&name_key(field)[..], &node.to_be_bytes()].concat()
}

/// The key of the holders of `target` as a parent in `field`, without its chunk.
pub(super) fn parent_holders_prefix(field: &str, target: u32) -> Vec<u8> {
    [&name_key(field)[..], &target.to_be_bytes()].concat()
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

/// The key of the count of posting keys in a bucket of `field`'s histogram,
/// in `index_histogram_keys`: how many keys a walk of the bucket reads, a key
/// per chunk of each value.
pub(super) fn hist_chunks_key(field: &str, bucket: &[u8]) -> Vec<u8> {
    hist_key(field, bucket)
}

/// The tables of chunked bitmaps (doc "Store tables"): a key there is the
/// bitmap's key and a 2-byte chunk. The transaction's chunk cache files a
/// chunk under its table's byte and that key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub(super) enum Bitmaps {
    Parents,
    LongTexts,
    ParentHolders,
    Descendants,
    ValueHolders,
    ValueCounts,
    TrigramHolders,
    /// `live_metarecords`, primary: its key is the chunk alone.
    LiveMetarecords,
}

impl Bitmaps {
    /// The derived ones, which a reindex clears.
    pub(super) const DERIVED: [Bitmaps; 7] = [
        Bitmaps::Parents,
        Bitmaps::LongTexts,
        Bitmaps::ParentHolders,
        Bitmaps::Descendants,
        Bitmaps::ValueHolders,
        Bitmaps::ValueCounts,
        Bitmaps::TrigramHolders,
    ];

    pub(super) fn db(self, t: &Tables) -> super::table::Db {
        match self {
            Bitmaps::Parents => t.index_parents,
            Bitmaps::LongTexts => t.index_long_texts,
            Bitmaps::ParentHolders => t.index_parent_holders,
            Bitmaps::Descendants => t.index_descendants,
            Bitmaps::ValueHolders => t.index_value_holders,
            Bitmaps::ValueCounts => t.index_value_counts,
            Bitmaps::TrigramHolders => t.index_trigram_holders,
            Bitmaps::LiveMetarecords => t.live_metarecords,
        }
    }

    /// The table a cache key's first byte names.
    fn of_byte(byte: u8) -> Bitmaps {
        Self::DERIVED
            .into_iter()
            .chain([Bitmaps::LiveMetarecords])
            .find(|b| *b as u8 == byte)
            .expect("a chunk cache key names its table")
    }
}

/// The live metarecords, as committed: the union of the chunks of
/// `live_metarecords`.
pub(super) fn read_live(t: &Tables, r: &RoTxn<'_>) -> Result<RoaringBitmap> {
    let mut out = RoaringBitmap::new();
    for entry in t.live_metarecords.iter(r)? {
        add_set(&mut out, decode_set(entry?.1)?);
    }
    Ok(out)
}

/// The longest text (lower-cased, in bytes) split in trigrams. A longer one —
/// a note, lyrics — would write a key per trigram on every edit; its id joins
/// the field's long-text set instead, and every search checks it.
pub(crate) const TRIGRAM_MAX: usize = 512;

/// Which holders of a field a key is among: of a value, or — on a position
/// field — of a name, filed under [`NAME_TAG`] ([`part_posting_prefix`]).
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

/// How many texts a trigram names by value before it turns by metarecord:
/// under it a new text adds itself to the trigram's list (one key, rewritten),
/// and a search reads no metarecord; over it the trigram is in most of the
/// field's texts — `fil` among file names — and its holders are a bitmap of
/// ids, built by the write that grows the list past it. A starting point, to
/// be measured.
pub(crate) const GRAM_TEXTS_MAX: usize = 64;

/// The key of a trigram's bitmap of ids, without its chunk.
pub(super) fn gram_prefix(field: &str, gram: &[u8; 3]) -> Vec<u8> {
    [&name_key(field)[..], gram].concat()
}

/// The mode byte after a trigram's key in `index_trigram_texts`: its list of
/// texts, by value (format 26 and 27 also had 2, a list frozen until a fold
/// turned it).
pub(super) const GRAM_BY_VALUE: u8 = 0;

/// The key of a trigram's list of texts in `index_trigram_texts`.
pub(super) fn gram_list_key(field: &str, gram: &[u8; 3], mode: u8) -> Vec<u8> {
    [&name_key(field)[..], gram, &[mode]].concat()
}

/// A text a trigram's list names: the partition its key is in, and the key.
pub(super) type GramText = (u8, Vec<u8>);

/// A trigram's list of texts, sorted: each a partition byte, a length (two
/// bytes) and the key.
pub(super) fn encode_gram_texts(texts: &[GramText]) -> Vec<u8> {
    let mut out = Vec::with_capacity(texts.iter().map(|(_, k)| k.len() + 3).sum());
    for (part, key) in texts {
        out.push(*part);
        out.extend_from_slice(&(key.len() as u16).to_be_bytes());
        out.extend_from_slice(key);
    }
    out
}

pub(super) fn decode_gram_texts(mut bytes: &[u8]) -> Result<Vec<GramText>> {
    let mut out = Vec::new();
    while !bytes.is_empty() {
        anyhow::ensure!(bytes.len() >= 3, "a trigram's text list cut short");
        let len = u16::from_be_bytes([bytes[1], bytes[2]]) as usize;
        anyhow::ensure!(bytes.len() >= 3 + len, "a trigram's text list cut short");
        out.push((bytes[0], bytes[3..3 + len].to_vec()));
        bytes = &bytes[3 + len..];
    }
    Ok(out)
}

/// How one trigram of a field is kept (doc "Text search in the store").
pub(super) enum GramMode {
    /// The texts holding it, read from its list.
    ByValue(Vec<GramText>),
    /// Its holders, a bitmap of ids.
    ByIds,
    /// No text of the field holds it.
    Absent,
}

/// A trigram's mode from the first key of `index_trigram_texts` under its
/// prefix (`list`: the key's mode byte and value), and whether it has a
/// bitmap.
pub(super) fn gram_mode_of(list: Option<(u8, &[u8])>, has_ids: bool) -> Result<GramMode> {
    Ok(match list {
        Some((GRAM_BY_VALUE, v)) => GramMode::ByValue(decode_gram_texts(v)?),
        Some((mode, _)) => anyhow::bail!("a trigram list of mode {mode}"),
        None if has_ids => GramMode::ByIds,
        None => GramMode::Absent,
    })
}

/// The key of a field's trigram state.
pub(super) fn gram_state_key(field: &str) -> Vec<u8> {
    name_key(field)
}

/// A field's trigram state.
#[derive(Clone, Default, PartialEq, Debug)]
pub(super) struct GramState {
    /// Some trigram of the field is kept by metarecord: an id gaining a
    /// text, new to the field or not, is filed in their bitmaps.
    pub(super) by_ids: bool,
}

impl GramState {
    pub(super) fn decode(bytes: Option<&[u8]>) -> Result<GramState> {
        let Some(b) = bytes else { return Ok(GramState::default()) };
        anyhow::ensure!(b.len() == 1, "a trigram state of {} bytes", b.len());
        Ok(GramState { by_ids: b[0] != 0 })
    }

    fn encode(&self) -> Vec<u8> {
        vec![self.by_ids as u8]
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

/// The key of a chunk of a set, in its table ([`IndexSet::table`]).
pub(super) fn set_key(set: IndexSet, field: Option<&str>, chunk: u16) -> Vec<u8> {
    // `nothing` is a value with a key of its own among the holders of the
    // field's values: its type byte alone (doc "Store tables").
    if set == IndexSet::Nothing {
        let field = field.expect("the holders of nothing are a field's");
        return [&name_key(field)[..], &[NOTHING_TAG], &chunk.to_be_bytes()].concat();
    }
    let mut k = Vec::new();
    if let Some(field) = field {
        k.extend_from_slice(&name_key(field));
    }
    k.extend_from_slice(&chunk.to_be_bytes());
    k
}

/// The prefix of every chunk of a set.
pub(super) fn set_prefix(set: IndexSet, field: Option<&str>) -> Vec<u8> {
    let mut k = set_key(set, field, 0);
    k.truncate(k.len() - 2);
    k
}

pub(super) fn dense(b: &[u8]) -> u32 {
    u32::from_be_bytes(b[..4].try_into().expect("a 4-byte id"))
}

pub(super) fn decode_set(bytes: &[u8]) -> Result<RoaringBitmap> {
    RoaringBitmap::deserialize_from(bytes).context("a derived set chunk")
}

/// The most ids [`add_set`] inserts one by one rather than by a union.
const SMALL_SET: u64 = 16;

/// Adds `set` to `out`. A union of roaring bitmaps rewrites the containers it
/// touches, whatever the size of `set`: accumulating the postings of a range
/// of distinct sizes — 10 000 sets of one id — that way cost 12 ms where
/// inserting their ids costs 0.8 (measured, roaring 0.12). A set of a few ids
/// is inserted one by one, a larger one united.
/// [`add_set`] of a set's encoding: a set of a few ids in one chunk — most
/// postings of a field of many values — is read off its bytes, no bitmap
/// built (1.9 ms instead of 2.7 for the 10 000 postings of a range).
pub(super) fn add_encoded(out: &mut RoaringBitmap, bytes: &[u8]) -> Result<()> {
    // The portable format without run containers, one array container: a
    // cookie, a count of containers, a key and a cardinality, an offset, the
    // values.
    if bytes.len() >= 16
        && bytes[..4] == 12346u32.to_le_bytes()
        && bytes[4..8] == 1u32.to_le_bytes()
    {
        let key = u32::from(u16::from_le_bytes([bytes[8], bytes[9]])) << 16;
        let card = usize::from(u16::from_le_bytes([bytes[10], bytes[11]])) + 1;
        if card as u64 <= SMALL_SET && bytes.len() == 16 + 2 * card {
            for v in bytes[16..].chunks_exact(2) {
                out.insert(key | u32::from(u16::from_le_bytes([v[0], v[1]])));
            }
            return Ok(());
        }
    }
    add_set(out, decode_set(bytes)?);
    Ok(())
}

pub(super) fn add_set(out: &mut RoaringBitmap, set: RoaringBitmap) {
    if set.len() <= SMALL_SET {
        out.extend(set.iter());
    } else {
        *out |= set;
    }
}

// ── Reading ─────────────────────────────────────────────────────────────────

/// A whole set: the union of its chunks.
pub(super) fn read_set(
    t: &Tables,
    r: &RoTxn<'_>,
    set: IndexSet,
    field: Option<&str>,
) -> Result<RoaringBitmap> {
    let prefix = set_prefix(set, field);
    let mut out = RoaringBitmap::new();
    for entry in set.table().db(t).prefix_iter(r, &prefix)? {
        let (k, v) = entry?;
        // The field name's terminator keeps a longer name out of the prefix;
        // only the 2-byte chunk follows it.
        if k.len() == prefix.len() + 2 {
            add_set(&mut out, decode_set(v)?);
        }
    }
    Ok(out)
}

/// The bitmap filed under `prefix` (a set's key without its chunk), as
/// committed: its chunks unioned.
pub(super) fn read_prefix_set(
    t: &Tables,
    r: &RoTxn<'_>,
    table: Bitmaps,
    prefix: &[u8],
) -> Result<RoaringBitmap> {
    let mut out = RoaringBitmap::new();
    for entry in table.db(t).prefix_iter(r, prefix)? {
        let (k, v) = entry?;
        if k.len() == prefix.len() + 2 {
            add_set(&mut out, decode_set(v)?);
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
        let k = chunk.to_be_bytes();
        let cached = [&[Bitmaps::LiveMetarecords as u8][..], &k].concat();
        if let Some(bm) = self.sets.borrow().get(&cached) {
            return Ok(f(bm));
        }
        let mut seen = self.universe.borrow_mut();
        if let std::collections::hash_map::Entry::Vacant(slot) = seen.entry(chunk) {
            slot.insert(match self.t.live_metarecords.get(r, &k)? {
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
        let mut kids =
            self.read_chunked(Bitmaps::ParentHolders, &parent_holders_prefix(field, parent))?;
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
        set: IndexSet,
        field: Option<&str>,
        id: u32,
        member: bool,
    ) -> Result<()> {
        self.chunk_member(set.table(), &set_prefix(set, field), id, member)
    }

    /// [`Self::set_member`] for a chunked bitmap of `table`, the key without
    /// its chunk.
    fn chunk_member(&self, table: Bitmaps, prefix: &[u8], id: u32, member: bool) -> Result<()> {
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
        table: Bitmaps,
        prefix: &[u8],
        chunk: u16,
        change: &mut dyn FnMut(&mut RoaringBitmap),
    ) -> Result<()> {
        let db = table.db(&self.t);
        let k = [&[table as u8][..], prefix, &chunk.to_be_bytes()].concat();
        let mut cache = self.sets.borrow_mut();
        if !cache.contains_key(&k) {
            let loaded = match db.get(&self.txn.borrow(), &k[1..])? {
                Some(bytes) => {
                    if table == Bitmaps::ValueHolders {
                        self.chunks_existing.borrow_mut().insert(k.clone());
                    }
                    decode_set(bytes)?
                }
                None => RoaringBitmap::new(),
            };
            cache.insert(k.clone(), loaded);
            let whole = [&[table as u8][..], prefix].concat();
            self.cached_chunks.borrow_mut().entry(whole).or_default().insert(chunk);
        }
        change(cache.get_mut(&k).expect("just loaded"));
        Ok(())
    }

    /// A whole bitmap as the transaction sees it: its stored chunks, the
    /// cached ones in their stead.
    pub(super) fn read_chunked(&self, table: Bitmaps, prefix: &[u8]) -> Result<RoaringBitmap> {
        let db = table.db(&self.t);
        let whole = [&[table as u8][..], prefix].concat();
        let cached = self.cached_chunks.borrow().get(&whole).cloned().unwrap_or_default();
        let cache = self.sets.borrow();
        let mut out = RoaringBitmap::new();
        for entry in db.scan(&self.txn.borrow(), prefix)? {
            let (k, v) = entry?;
            if k.len() == prefix.len() + 2 {
                let chunk = u16::from_be_bytes([k[k.len() - 2], k[k.len() - 1]]);
                if !cached.contains(&chunk) {
                    add_set(&mut out, decode_set(v)?);
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
        table: Bitmaps,
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
            let state = self.grams.borrow()[&field].encode();
            self.t.index_trigram_state.put(&mut w, &gram_state_key(&field), &state)?;
        }
        // The posting chunks that came or went, by bucket of their value.
        let mut chunks: BTreeMap<Vec<u8>, i64> = BTreeMap::new();
        for (k, mut bm) in self.sets.borrow_mut().drain() {
            let table = Bitmaps::of_byte(k[0]);
            if table == Bitmaps::ValueHolders {
                let existed = self.chunks_existing.borrow_mut().remove(&k);
                let delta = i64::from(!bm.is_empty()) - i64::from(existed);
                if let (true, Ok((name, rest))) = (delta != 0, unesc(&k[1..])) {
                    let bucket = hist_bucket(&rest[..rest.len() - 2]);
                    if let (Some(bucket), Ok(field)) = (bucket, String::from_utf8(name)) {
                        *chunks.entry(hist_chunks_key(&field, &bucket)).or_default() += delta;
                    }
                }
            }
            let db = table.db(&self.t);
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
            self.bump(self.t.index_histogram_keys, &key, delta)?;
        }
        Ok(())
    }

    /// Whether a chunked bitmap has any member, as the transaction sees it —
    /// without decoding the stored chunks.
    fn chunked_exists(&self, table: Bitmaps, prefix: &[u8]) -> Result<bool> {
        let db = table.db(&self.t);
        let whole = [&[table as u8][..], prefix].concat();
        let cached = self.cached_chunks.borrow().get(&whole).cloned().unwrap_or_default();
        {
            let cache = self.sets.borrow();
            let key = |chunk: u16| [&whole[..], &chunk.to_be_bytes()].concat();
            if cached.iter().any(|&c| cache.get(&key(c)).is_some_and(|bm| !bm.is_empty())) {
                return Ok(true);
            }
        }
        for entry in db.scan(&self.txn.borrow(), prefix)? {
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

    /// [`Self::bump`] on the rows of a bucket ([`hist_key`]).
    pub(super) fn bump_hist(&self, k: &[u8], delta: i64) -> Result<()> {
        self.bump(self.t.index_histogram_rows, k, delta)
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
    /// A key of `index_raw_holder` per field of positions (doc "Store
    /// tables": the field comes first in every index key).
    fn parent_fields(&self, parent: u32) -> Result<BTreeSet<String>> {
        let mut out = BTreeSet::new();
        let (_, positional) = self.fields_held()?;
        let r = self.txn.borrow();
        for field in positional {
            if self
                .t
                .index_raw_holder
                .prefix_iter(&r, &raw_prefix(&field, parent))?
                .next()
                .is_some()
            {
                out.insert(field);
            }
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
        self.new_id(target)
    }

    /// The next dense id, given to `uuid`.
    fn new_id(&self, uuid: &[u8; 16]) -> Result<u32> {
        let id = u32::try_from(self.next(super::State::NextMetarecordId)?)
            .context("dense ids exhausted")?;
        self.put_uuid(id, uuid)?;
        self.t.ids.put(&mut self.txn.borrow_mut(), uuid, &id.to_be_bytes())?;
        Ok(id)
    }

    /// Whether `id` was reserved for a uuid no metarecord has had: not live,
    /// and no record — a deleted metarecord's stays until a reindex (doc
    /// "Store tables").
    pub(super) fn reserved(&self, id: u32) -> Result<bool> {
        let txn = self.txn.borrow();
        if self.live_in(&txn, id)? {
            return Ok(false);
        }
        drop(txn);
        Ok(!self.has_record(id)?)
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
            Some(reserved) if self.reserved(reserved)? => reserved,
            Some(_) => bail!("metarecord {uuid} was deleted: it is given back, not created"),
            None => self.new_id(uuid.as_bytes())?,
        };
        self.chunk_member(Bitmaps::LiveMetarecords, &[], id, true)?;
        // Every metarecord has a record, an empty one included.
        if !self.has_record(id)? {
            self.put_stored(id, super::mass::Record::plain(Vec::new()))?;
        }
        for field in self.parent_fields(id)? {
            if self.has_live_kids(id, &field, None)? {
                self.set_member(IndexSet::Parents, Some(&field), id, true)?;
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
        self.chunk_member(Bitmaps::LiveMetarecords, &[], id, false)
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
        let rows = self.record_rows(id)?;
        let rows = rows.as_slice();
        // A position a metarecord that exists took since is not given back.
        for row in rows {
            let Some(k) = self.raw_key_now(&row.name, &row.value)? else { continue };
            let txn = self.txn.borrow();
            if let Some(v) = self.t.index_raw_holder.get(&txn, &k)? {
                let holder = dense(v);
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
        // as that deletion's own undo does it, for this one operation: its
        // pending write names the id, and the operation is its rank.
        if self.deletes_pending() {
            let entries = self.part_entries(super::mass::part_of(id))?;
            let op = entries.iter().filter(|e| e.ids.contains(id)).find_map(|e| e.deleted_by(id));
            if let Some(op) = op {
                if self.mass_undelete_log(op, op)?.is_some() {
                    return Ok(Some(self.record_rows(id)?));
                }
                // The deletion is folded before the record is given back.
                while self.fold_one_delete()? {}
            }
        }
        self.put_given_back(id, rows)?;
        for row in rows {
            self.count_type(&row.name, &row.value, 1)?;
            self.add_hist(&row.name, &row.value)?;
            self.file_position(&row.name, &row.value, id)?;
            if row.value.position().is_some() {
                let pid =
                    self.target_id(&target_of(&row.value).expect("a position has a parent"))?;
                if pid != NO_TARGET {
                    self.set_member(IndexSet::Parents, Some(&row.name), pid, true)?;
                }
            }
        }
        self.chunk_member(Bitmaps::LiveMetarecords, &[], id, true)?;
        for field in self.parent_fields(id)? {
            if self.has_live_kids(id, &field, None)? {
                self.set_member(IndexSet::Parents, Some(&field), id, true)?;
            }
        }
        Ok(Some(rows.to_vec()))
    }

    /// Writes the rows of a record given back under its id. The entries of a
    /// write to many records still naming the id are older than this write:
    /// they change none of its rows, and add none to a field it lacks — the
    /// record says so, or they would apply to the rows again on reading.
    pub(super) fn put_given_back(&self, id: u32, rows: &[crate::rows::FieldRow]) -> Result<()> {
        let entries =
            if self.has_pending_writes() { self.load_record(id)?.entries } else { Vec::new() };
        if entries.is_empty() {
            return self.put_stored(id, super::mass::Record::plain(rows.to_vec()));
        }
        let n = self.next(super::State::NextWriteNumber)? as u64;
        let fields: BTreeSet<&str> = entries.iter().map(|e| e.field.as_str()).collect();
        let mut record = super::mass::Record::plain(rows.to_vec());
        for field in fields {
            record.fix_field(field, n);
        }
        self.put_stored(id, record)
    }

    /// The histogram forgets a row of a metarecord that was deleted: what the
    /// fold of a deletion of many records gives back, the histograms counting
    /// rows that no universe sifts.
    pub(super) fn drop_hist(&self, field: &str, value: &Value) -> Result<()> {
        if let Some(bucket) = value_key(value).as_deref().and_then(hist_bucket) {
            self.bump_hist(&hist_key(field, &bucket), -1)?;
        }
        Ok(())
    }

    /// The histogram counts a row again: what a deletion's fold took off, put
    /// back with the record.
    pub(super) fn add_hist(&self, field: &str, value: &Value) -> Result<()> {
        if let Some(bucket) = value_key(value).as_deref().and_then(hist_bucket) {
            self.bump_hist(&hist_key(field, &bucket), 1)?;
        }
        Ok(())
    }

    /// A row of `uuid` goes with its metarecord, which is being removed whole
    /// (not [`Self::derive_row`], which unpicks every entry of the row): only
    /// what no universe can sift is kept exact — the histograms, which count
    /// rows, and whether the row's parent still has a child.
    pub(super) fn derive_dropped(&self, uuid: Uuid, field: &str, value: &Value) -> Result<()> {
        if let Some(bucket) = value_key(value).as_deref().and_then(hist_bucket) {
            self.bump_hist(&hist_key(field, &bucket), -1)?;
        }
        if value.position().is_some() {
            let parent = target_of(value).expect("a position has a parent key");
            let pid = self.target_id(&parent)?;
            let Some(id) = self.id(uuid.as_bytes())? else { return Ok(()) };
            if pid != NO_TARGET && !self.has_live_kids(pid, field, Some(id))? {
                self.set_member(IndexSet::Parents, Some(field), pid, false)?;
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
        // A field with index writes pending in this id's chunk takes this
        // row as one more, of one id: what is stored there is behind, and is
        // not written past them.
        let eventful = self.pending_at(field, id)?;
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
                self.single_index_write(field, value, id, delta > 0)?;
            } else {
                // A record is in a value's posting while one of its rows has
                // its key: a row going leaves it only if no other does.
                let held = delta > 0
                    || rows.iter().any(|r| value_key(&r.value).as_deref() == Some(&k[..]));
                self.chunk_member(Bitmaps::ValueHolders, &posting_prefix(field, &k), id, held)?;
            }
            if let Some(bucket) = hist_bucket(&k) {
                self.bump_hist(&hist_key(field, &bucket), delta)?;
            }
        }
        if let Some(k) = name_part_key(value) {
            let held = delta > 0
                || rows.iter().any(|r| name_part_key(&r.value).as_deref() == Some(&k[..]));
            self.chunk_member(
                Bitmaps::ValueHolders,
                &part_posting_prefix(field, NAME, &k),
                id,
                held,
            )?;
        }
        // What the row points at, by the dense id it is filed under.
        // A position is filed under its parent (a reserved id for a parent
        // not created yet); a `ref`'s referrers are its value's holders.
        let target = match target_of(value).filter(|_| value.position().is_some()) {
            Some(target) => Some((target, self.target_id(&target)?)),
            None => None,
        };
        if let (Some((parent, pid)), true) = (target, value.position().is_some()) {
            if pid != NO_TARGET {
                // The parent's referrers still hold `id` (they are brought up
                // to date below): it counts only if it keeps a row there.
                let stays = rows.iter().any(|r| target_of(&r.value) == Some(parent));
                let member = delta > 0 || self.has_live_kids(pid, field, (!stays).then_some(id))?;
                self.set_member(IndexSet::Parents, Some(field), pid, member)?;
            }
        }
        let nothing = rows.iter().any(|r| matches!(r.value, Value::Nothing));
        self.set_member(IndexSet::Nothing, Some(field), id, nothing)?;
        let valued = rows.iter().filter(|r| !matches!(r.value, Value::Nothing)).count();
        self.derive_count(field, id, valued as u64, value, count_before)?;
        // Trigrams give candidates, which a search always checks: a text
        // going leaves its own where they are, and only a text coming writes
        // (doc "Text search in the store").
        if let Some(text) = &text {
            self.file_grams(field, text, new_text.as_ref(), &RoaringBitmap::from_iter([id]))?;
        }
        if search_text(value).is_some() {
            let long =
                rows.iter().filter_map(|r| search_text(&r.value)).any(|t| t.len() > TRIGRAM_MAX);
            self.set_member(IndexSet::LongTexts, Some(field), id, long)?;
        }
        if let (Some((target, tid)), false) = (target, eventful) {
            let held = rows.iter().any(|r| target_of(&r.value) == Some(target));
            self.chunk_member(
                Bitmaps::ParentHolders,
                &parent_holders_prefix(field, tid),
                id,
                held,
            )?;
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
            self.chunk_member(
                Bitmaps::ValueCounts,
                &count_prefix(field, bit),
                id,
                valued >> bit & 1 == 1,
            )?;
        }
        Ok(())
    }

    /// The ids holding a value of `field`, as the transaction sees them:
    /// those of any bit of the counts.
    fn holders_now(&self, field: &str) -> Result<RoaringBitmap> {
        let mut out = RoaringBitmap::new();
        for bit in 0..COUNT_BITS {
            out |= self.read_chunked(Bitmaps::ValueCounts, &count_prefix(field, bit))?;
        }
        Ok(out)
    }

    /// The ids holding a row of `field`, `Nothing` or not, as the transaction
    /// sees them.
    pub(super) fn holder_ids_now(&self, field: &str) -> Result<RoaringBitmap> {
        let mut out = self.holders_now(field)?;
        out |= self
            .read_chunked(IndexSet::Nothing.table(), &set_prefix(IndexSet::Nothing, Some(field)))?;
        Ok(out)
    }

    /// The ids holding `value` in `field` as the transaction sees them — for
    /// `Nothing`, the holders of `nothing` in the field ([`crate::store::Rows::value_holder_ids`]).
    pub(super) fn value_holders_now(&self, field: &str, value: &Value) -> Result<RoaringBitmap> {
        match value_key(value) {
            Some(key) => self.effective(field, value, &key),
            None => self.read_chunked(
                IndexSet::Nothing.table(),
                &set_prefix(IndexSet::Nothing, Some(field)),
            ),
        }
    }

    /// The ids holding more than `n` rows of `field` — its values, and the
    /// `Nothing` row it may hold beside them — as the transaction sees them:
    /// read off the counts, no record read (the query source's `rows_over`).
    pub(super) fn rows_over_now(&self, field: &str, n: u64) -> Result<RoaringBitmap> {
        let mut counts = Vec::with_capacity(COUNT_BITS as usize);
        for bit in 0..COUNT_BITS {
            counts.push(self.read_chunked(Bitmaps::ValueCounts, &count_prefix(field, bit))?);
        }
        let unknown = self
            .read_chunked(IndexSet::Nothing.table(), &set_prefix(IndexSet::Nothing, Some(field)))?;
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
            return Ok(state.clone());
        }
        let state = GramState::decode(
            self.t.index_trigram_state.get(&self.txn.borrow(), &gram_state_key(field))?,
        )?;
        self.grams.borrow_mut().insert(field.to_string(), state.clone());
        Ok(state)
    }

    fn set_gram_state(&self, field: &str, state: GramState) {
        self.grams.borrow_mut().insert(field.to_string(), state);
        self.grams_changed.borrow_mut().insert(field.to_string());
    }

    /// How a trigram of `field` is kept, as the transaction sees it.
    fn gram_mode(&self, field: &str, gram: &[u8; 3]) -> Result<GramMode> {
        let prefix = [&name_key(field)[..], gram].concat();
        let first = {
            let r = self.txn.borrow();
            let first = self
                .t
                .index_trigram_texts
                .prefix_iter(&r, &prefix)?
                .next()
                .transpose()?
                .map(|(k, v)| (k.to_vec(), v.to_vec()));
            first
        };
        let list = match first {
            Some((k, v)) => {
                anyhow::ensure!(k.len() == prefix.len() + 1, "a trigram list key");
                Some((k[prefix.len()], v))
            }
            None => None,
        };
        let has_ids = list.is_none()
            && self.chunked_exists(Bitmaps::TrigramHolders, &gram_prefix(field, gram))?;
        gram_mode_of(list.as_ref().map(|(m, v)| (*m, &v[..])), has_ids)
    }

    /// For a text about to join `field`: its partition and key, when the
    /// field may not hold that text yet — no row of the partition has it, or
    /// its key is a cut one, which another text could share. `None` when the
    /// text's trigrams already name it.
    fn new_text(&self, field: &str, value: &Value) -> Result<Option<GramText>> {
        let Some((part, key)) = text_part_key(value) else { return Ok(None) };
        let text = if part == VALUE { &key[1..] } else { &key[..] };
        if long_prefix(text).is_none()
            && self
                .chunked_exists(Bitmaps::ValueHolders, &part_posting_prefix(field, part, &key))?
        {
            return Ok(None);
        }
        Ok(Some((part, key)))
    }

    /// The trigrams of a text `ids` gained in `field` (doc "Text search in
    /// the store"). A text new to the field joins the lists of its trigrams
    /// kept by value; a list growing past [`GRAM_TEXTS_MAX`] turns by
    /// metarecord ([`Self::turn_gram`]). The ids join the bitmaps of its
    /// trigrams by metarecord, at once: a write to many records files them
    /// chunk by chunk.
    fn file_grams(
        &self,
        field: &str,
        text: &str,
        new: Option<&GramText>,
        ids: &RoaringBitmap,
    ) -> Result<()> {
        let mut state = self.gram_state(field)?;
        // A text the field holds already, none of its trigrams by metarecord:
        // its lists name it, nothing to read.
        if new.is_none() && !state.by_ids {
            return Ok(());
        }
        let before = state.clone();
        for gram in trigrams(text) {
            let mut list = match self.gram_mode(field, &gram)? {
                GramMode::ByValue(list) => list,
                GramMode::Absent => Vec::new(),
                GramMode::ByIds => {
                    self.chunk_apply(
                        Bitmaps::TrigramHolders,
                        &gram_prefix(field, &gram),
                        ids,
                        true,
                    )?;
                    continue;
                }
            };
            let Some(new) = new else { continue };
            let Err(at) = list.binary_search(new) else { continue };
            list.insert(at, new.clone());
            if list.len() > GRAM_TEXTS_MAX {
                self.turn_gram(field, &gram, &list, ids)?;
                state.by_ids = true;
            } else {
                self.t.index_trigram_texts.put(
                    &mut self.txn.borrow_mut(),
                    &gram_list_key(field, &gram, GRAM_BY_VALUE),
                    &encode_gram_texts(&list),
                )?;
            }
        }
        if state != before {
            self.set_gram_state(field, state);
        }
        Ok(())
    }

    /// A trigram of `field` whose list, `texts`, grew past
    /// [`GRAM_TEXTS_MAX`] turns by metarecord: its bitmap gets the holders of
    /// each text as the transaction sees them — a posting with the field's
    /// pending index writes replayed (doc "Mass writes in the store") — and
    /// the ids of the write, whose own posting may wait behind them; its list
    /// goes. A read per text, once in a trigram's life.
    fn turn_gram(
        &self,
        field: &str,
        gram: &[u8; 3],
        texts: &[GramText],
        ids: &RoaringBitmap,
    ) -> Result<()> {
        let pending = if self.eventful(field)? > 0 {
            index_writes::read_index_writes(&self.t, &self.txn.borrow(), field)?
        } else {
            Vec::new()
        };
        let mut holders = ids.clone();
        for (part, key) in texts {
            let stored =
                self.read_chunked(Bitmaps::ValueHolders, &part_posting_prefix(field, *part, key))?;
            holders |= if *part == VALUE && !pending.is_empty() {
                index_writes::replay(&pending, key, stored)
            } else {
                stored
            };
        }
        self.chunk_apply(Bitmaps::TrigramHolders, &gram_prefix(field, gram), &holders, true)?;
        self.t
            .index_trigram_texts
            .delete(&mut self.txn.borrow_mut(), &gram_list_key(field, gram, GRAM_BY_VALUE))?;
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
            let mut moving =
                self.read_chunked(Bitmaps::Descendants, &descendants_prefix(field, id))?;
            moving.insert(id);
            for ancestor in &ancestors {
                let prefix = descendants_prefix(field, self.target_id(ancestor.as_bytes())?);
                self.chunk_apply(Bitmaps::Descendants, &prefix, &moving, true)?;
            }
            return Ok(());
        }
        let mut recomputed = Vec::with_capacity(ancestors.len());
        for ancestor in &ancestors {
            recomputed.push(self.reachable_below(field, *ancestor, &ancestors)?);
        }
        for (ancestor, now) in ancestors.iter().zip(recomputed) {
            let prefix = descendants_prefix(field, self.target_id(ancestor.as_bytes())?);
            let before = self.read_chunked(Bitmaps::Descendants, &prefix)?;
            self.chunk_apply(Bitmaps::Descendants, &prefix, &(&before - &now), false)?;
            self.chunk_apply(Bitmaps::Descendants, &prefix, &(&now - &before), true)?;
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
                    out |= self
                        .read_chunked(Bitmaps::Descendants, &descendants_prefix(field, child_id))?;
                }
            }
        }
        Ok(out)
    }

    /// The nodes placed directly under `parent` in `field`, with their ids.
    /// Read off `index_raw_holder`, by name: a child under one parent with
    /// two names (a `hierarchy_ref`) comes once.
    fn children_of(&self, field: &str, parent: Uuid) -> Result<Vec<(Uuid, u32)>> {
        let pid = self.target_id(parent.as_bytes())?;
        let prefix = raw_prefix(field, pid);
        let r = self.txn.borrow();
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for entry in self.t.index_raw_holder.prefix_iter(&r, &prefix)? {
            let id = dense(entry?.1);
            if !seen.insert(id) {
                continue;
            }
            let uuid = self.uuid_now(id)?.context("a child without a uuid")?;
            out.push((uuid, id));
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
        let mut moving = self.read_chunked(Bitmaps::Descendants, &descendants_prefix(field, id))?;
        moving.insert(id);
        let mut up = Some(parent);
        for _ in 0..crate::log::MAX_TREE_DEPTH {
            let Some(ancestor) = up else { return Ok(()) };
            let above = self.target_id(ancestor.as_bytes())?;
            self.chunk_apply(
                Bitmaps::Descendants,
                &descendants_prefix(field, above),
                &moving,
                add,
            )?;
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
        self.index_writes.borrow_mut().clear();
        self.forget_uuid_ranges();
        let mut w = self.txn.borrow_mut();
        let t = self.t;
        // The numbering (`ids`, `live_metarecords`) is primary: a reindex
        // renumbers it, as it clears and gives the ids again below.
        for db in [
            t.ids,
            t.uuids,
            t.live_metarecords,
            t.field_types,
            t.index_raw_holder,
            t.index_trigram_texts,
            t.index_trigram_holders,
            t.index_trigram_state,
            t.index_histogram_rows,
            t.index_histogram_keys,
            t.index_pending_writes,
        ] {
            db.clear(&mut w)?;
        }
        for table in Bitmaps::DERIVED {
            table.db(&t).clear(&mut w)?;
        }
        Ok(())
    }
}

impl KvTxn<'_> {
    /// The key of a position in `index_raw_holder`, its parent's id as the
    /// transaction sees it: `None` when the parent has no id, under which no
    /// position can be filed.
    pub(super) fn raw_key_now(&self, field: &str, value: &Value) -> Result<Option<Vec<u8>>> {
        raw_key_of(&self.t, &self.txn.borrow(), field, value)
    }

    /// Files the position `value` of the metarecord `id` in
    /// `index_raw_holder`, its parent given an id if it has none yet (a child
    /// written before its parent: the id is reserved, doc "Store tables").
    pub(super) fn file_position(&self, field: &str, value: &Value, id: u32) -> Result<()> {
        let Some((parent, name)) = value.position() else { return Ok(()) };
        let pid = match parent {
            None => NO_TARGET,
            Some(p) => self.target_id(p.as_bytes())?,
        };
        let k = raw_key(field, pid, name.as_bytes());
        self.t.index_raw_holder.put(&mut self.txn.borrow_mut(), &k, &id.to_be_bytes())?;
        Ok(())
    }

    /// Removes from the primary tables what the deleted metarecords left
    /// there (doc "Deleting a metarecord in the store"): their records.
    pub(super) fn drop_left_behind(&self) -> Result<()> {
        // The table is walked and changed under its own keys.
        self.flush_and_forget_records()?;
        let t = self.t;
        let mut gone: Vec<(super::Db, Vec<u8>)> = Vec::new();
        {
            let r = self.txn.borrow();
            // The records of the metarecords that are gone, which stayed for a
            // restoration: held against the primary key of each uuid.
            for entry in t.metarecords.iter(&r)? {
                let (k, _) = entry?;
                if !self.live_in(&r, dense(k))? {
                    gone.push((t.metarecords, k.to_vec()));
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
    pub(super) fn file_records_by_uuid(&self, who: &HashMap<u32, [u8; 16]>) -> Result<()> {
        self.refile_records(4, |_, _, k| {
            let uuid = who.get(&dense(k)).context("a record of no live metarecord")?;
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
        // The records move to other keys: what the transaction decoded is
        // written first, and named by its former key no more.
        self.flush_and_forget_records()?;
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
                for entry in t.metarecords.range(&r, &range)? {
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
                t.metarecords.delete(&mut w, from)?;
                t.metarecords.put(&mut w, new, value)?;
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
fn forest_order(t: &Tables, r: &RoTxn<'_>) -> Result<Vec<u32>> {
    // Each forest's key prefix (its field name, terminated) and its size,
    // off `index_raw_holder`: an index, so a damaged one costs the order, and
    // the records it misses are numbered after (doc "Store tables").
    let mut forests: Vec<(Vec<u8>, u64)> = Vec::new();
    for e in t.index_raw_holder.iter(r)? {
        let (k, _) = e?;
        // The field's end is its `00 00` terminator (an escaped `00` is
        // followed by `FF`).
        let end = (0..k.len().saturating_sub(1))
            .find(|&i| k[i] == 0 && k[i + 1] == 0)
            .context("a position key without its field")?;
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
        let mut stack = vec![NO_TARGET];
        while let Some(node) = stack.pop() {
            if node != NO_TARGET {
                order.push(node);
            }
            let mut children = Vec::new();
            for e in
                t.index_raw_holder.prefix_iter(r, &[&field[..], &node.to_be_bytes()].concat())?
            {
                let child = dense(e?.1);
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
        // The log names its targets by uuid before the ids are numbered
        // again (doc "Log storage").
        while txn.index_log_step(super::entry::INDEX_BUDGET)? {}
        txn.drop_left_behind()?;
        // A reindex invalidates the work under way anyway: the contexts left
        // empty go (doc "Log contexts").
        txn.drop_empty_log_contexts()?;
        // Whose each live id is, off the primary numbering (`ids`), not off
        // its index.
        let who: HashMap<u32, [u8; 16]> = {
            let r = txn.txn.borrow();
            let live = read_live(&txn.t, &r)?;
            let mut who = HashMap::with_capacity(live.len() as usize);
            for entry in txn.t.ids.iter(&r)? {
                let (uuid, id) = entry?;
                if live.contains(dense(id)) {
                    who.insert(dense(id), uuid.try_into().expect("a 16-byte uuid"));
                }
            }
            who
        };
        let order = {
            let r = txn.txn.borrow();
            let mut seen = BTreeSet::new();
            let mut order = Vec::new();
            for id in forest_order(&txn.t, &r)? {
                if let Some(u) = who.get(&id).map(|u| Uuid::from_bytes(*u)) {
                    if seen.insert(u) {
                        order.push(u);
                    }
                }
            }
            let mut ids: Vec<u32> = who.keys().copied().collect();
            ids.sort_unstable();
            for id in ids {
                let u = Uuid::from_bytes(who[&id]);
                if seen.insert(u) {
                    order.push(u);
                }
            }
            order
        };
        // The records are filed under their uuid while the ids are numbered
        // again, then under the new dense ids.
        txn.file_records_by_uuid(&who)?;
        txn.clear_derived()?;
        txn.meta_put(super::State::NextMetarecordId, 1)?;
        txn.next(super::State::NextReindex)?;
        for &uuid in &order {
            txn.derive_created(uuid)?;
        }
        txn.file_records_by_id()?;
        for &uuid in &order {
            let id = txn.dense_id(uuid.as_bytes())?.context("a metarecord numbered")?;
            for row in Rows::rows(&txn, uuid)? {
                txn.file_position(&row.name, &row.value, id)?;
                // Nothing was counted yet: the derived tables were cleared.
                txn.count_type(&row.name, &row.value, 1)?;
                txn.derive_row(uuid, &row.name, &row.value, 1, 0)?;
            }
        }
        txn.meta_put(super::State::DerivedVersion, DERIVED_VERSION)?;
        txn.finish()?;
        self.writes_pending.store(false, std::sync::atomic::Ordering::Relaxed);
        self.deletes.store(false, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// Folds the biggest list pending — a part's entries, a field's index
    /// writes — in one transaction of its own, so
    /// that what waits for the store waits for one list (doc "Folding in
    /// idle time"). Returns whether there was one.
    pub fn fold_step(&mut self) -> Result<bool> {
        let txn = KvTxn::new(self)?;
        let folded = txn.fold_biggest()?;
        txn.finish()?;
        if folded {
            self.writes_pending
                .store(self.pending_writes()? > 0, std::sync::atomic::Ordering::Relaxed);
            let r = self.env.read_txn()?;
            self.deletes.store(self.deletion_pending(&r)?, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(folded)
    }

    /// How worn the store is: what writes left pending, and the ids no
    /// metarecord holds (doc "Folding in idle time").
    pub fn wear(&self) -> Result<Wear> {
        let r = self.env.read_txn()?;
        let mut wear = Wear::default();
        let mut parts = std::collections::BTreeSet::new();
        for entry in self.t.pending_writes.iter(&r)? {
            let (k, v) = entry?;
            wear.pending_writes += 1;
            wear.pending_write_bytes += (k.len() + v.len()) as u64;
            parts.insert(u32::from_be_bytes(k[..4].try_into().expect("a part")));
        }
        wear.pending_write_parts = parts.len();
        let mut fields = std::collections::BTreeSet::new();
        for entry in self.t.index_pending_writes.iter(&r)? {
            let (k, v) = entry?;
            wear.index_writes += 1;
            wear.index_write_bytes += (k.len() + v.len()) as u64;
            fields.insert(unesc(k)?.0);
        }
        wear.index_write_fields = fields.len();
        // The counter holds the next id to hand out; the first is 1.
        wear.ids_allocated =
            (super::State::NextMetarecordId.get(&self.t, &r)?.unwrap_or(1) - 1).max(0) as u64;
        wear.ids_live = read_live(&self.t, &r)?.len();
        Ok(wear)
    }

    /// Integrates everything the writes left pending for a quiet moment: the
    /// parts' entries, the fields' index writes, the log entries not yet
    /// indexed by metarecord (doc "Folding in idle time"). A reindex leaves
    /// nothing pending.
    pub fn fold(&mut self) -> Result<()> {
        let txn = KvTxn::new(self)?;
        txn.fold_all_mass()?;
        txn.fold_all_index_writes()?;
        while txn.index_log_step(super::entry::INDEX_BUDGET)? {}
        txn.finish()?;
        self.writes_pending.store(false, std::sync::atomic::Ordering::Relaxed);
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
        // `live_metarecords` is primary (doc "Store tables"): each live id
        // must have its uuid in `uuids`, the index of `ids`.
        let live = read_live(t, r)?;
        let metarecords = live.len();
        for id in &live {
            if super::uuid_at(t, r, id)?.is_none() {
                diff.push(format!("uuids: the live dense id {id} names no metarecord"));
            }
        }
        let mut mapped = 0u64;
        for entry in t.ids.iter(r)? {
            let (k, v) = entry?;
            mapped += 1;
            let back = super::uuid_at(t, r, dense(v))?;
            if back != Some(uuid_of(k)) {
                diff.push(format!("ids: {} -> {}, but uuids says {back:?}", uuid_of(k), dense(v)));
            }
        }
        // An id never given names nothing, deleted or not: it is held to
        // the rows like one that exists, and found unexpected.
        let next = super::State::NextMetarecordId.get(t, r)?.unwrap_or(1);
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
        let pending = self.has_pending_writes();
        let mut parts = super::mass::Parts::default();
        for record_id in super::mass::record_ids(t, r, pending)? {
            let Some(named) = super::uuid_at(t, r, record_id)? else {
                diff.push(format!("records: dense id {record_id} names no metarecord"));
                continue;
            };
            // An id only an entry names may be a metarecord's that is gone.
            if !live.contains(record_id) {
                continue;
            }
            let uuid: [u8; 16] = *named.as_bytes();
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
                    let key = [&[NAME_TAG][..], &key].concat();
                    *group.parts.entry((VALUE, key)).or_default() += 1;
                }
                // The parent a position is filed under, by its dense id.
                let target = match target_of(&row.value).filter(|_| row.value.position().is_some())
                {
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
                if let (Some((parent, name)), Some(target)) = (row.value.position(), target) {
                    let parent = &parent;
                    group.raws.insert(
                        [&target.to_be_bytes()[..], &super::node_key(name.as_bytes())].concat(),
                    );
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
                        let text_key = text_part_key(&row.value).expect("a text has a key");
                        group
                            .grams
                            .extend(trigrams(&text).into_iter().map(|g| (g, text_key.clone())));
                    } else {
                        group.long = true;
                    }
                }
                if matches!(row.value, Value::Nothing) {
                    group.nothing = true;
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
                sink(Side::Expected, Entry::set(IndexSet::Parents, field, id))?;
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

        // The fields with index writes pending (doc "Mass writes in the store"):
        // what is stored of their values is behind by them, and is held to
        // the rows as replayed — each index write list as it reads on the value
        // partition, and on the referrers.
        let mut pending: HashMap<String, (Vec<index_writes::IndexWrite>, ())> = HashMap::new();
        for entry in t.index_pending_writes.iter(r)? {
            let (name, _) = unesc(entry?.0)?;
            let field = String::from_utf8(name).context("a field name")?;
            if let std::collections::hash_map::Entry::Vacant(slot) = pending.entry(field) {
                let list = index_writes::read_index_writes(t, r, slot.key())?;
                slot.insert((list, ()));
            }
        }

        // Got: the derived tables, as they are.
        let tables = [
            (Bitmaps::Parents, IndexSet::Parents),
            (Bitmaps::LongTexts, IndexSet::LongTexts),
            (Bitmaps::ParentHolders, IndexSet::ParentHolders),
            (Bitmaps::Descendants, IndexSet::Descendants),
            (Bitmaps::ValueHolders, IndexSet::ValueHolders),
            (Bitmaps::ValueCounts, IndexSet::ValueCounts),
        ];
        for (table, set) in tables {
            for entry in table.db(t).iter(r)? {
                let (k, v) = entry?;
                let (name, rest) = unesc(k)?;
                let field = std::str::from_utf8(&name).context("a field name")?;
                if set == IndexSet::ValueHolders {
                    // A posting is an entry per holder of its value or name.
                    let key = &rest[..rest.len() - 2];
                    let tag = VALUE;
                    if key == [NOTHING_TAG] {
                        // `nothing`'s holders: what the rows say as absence.
                        for id in decode_set(v)? {
                            if !sifted(id) {
                                sink(Side::Got, Entry::set(IndexSet::Nothing, field, id))?;
                            }
                        }
                        continue;
                    }
                    let replayed = pending.get(field).filter(|_| tag == VALUE);
                    for id in decode_set(v)? {
                        if replayed
                            .is_some_and(|(e, _)| !index_writes::replay_one(e, key, id, true))
                        {
                            continue;
                        }
                        if !sifted(id) {
                            let entry = Entry { space: PART, tag, field, key, id, count: 1 };
                            sink(Side::Got, entry)?;
                        }
                    }
                    continue;
                }
                let (space, tag, key) = match set {
                    IndexSet::ParentHolders => (PARENT_HOLDER, 0, &rest[..4]),
                    IndexSet::Descendants => (DESCENDANT, 0, &rest[..4]),
                    // One bit of the counts: a set of its own.
                    IndexSet::ValueCounts => (SET, COUNT_TAG + rest[0], &rest[..0]),
                    _ => (SET, set as u8, &rest[..0]),
                };
                for id in decode_set(v)? {
                    if !sifted(id) {
                        sink(Side::Got, Entry { space, tag, field, key, id, count: 1 })?;
                    }
                }
            }
        }
        // What the index writes add and nothing stores yet, under the values they
        // name.
        for (field, (list, _)) in &pending {
            for key in index_writes::named(list) {
                let run = posting_prefix(field, &key);
                let mut stored = RoaringBitmap::new();
                for entry in Bitmaps::ValueHolders.db(t).prefix_iter(r, &run)? {
                    let (k, v) = entry?;
                    if k.len() == run.len() + 2 {
                        add_set(&mut stored, decode_set(v)?);
                    }
                }
                for id in index_writes::replay(list, &key, stored.clone()) - stored {
                    if !sifted(id) {
                        let entry =
                            Entry { space: PART, tag: VALUE, field, key: &key, id, count: 1 };
                        sink(Side::Got, entry)?;
                    }
                }
            }
        }
        for entry in t.index_raw_holder.iter(r)? {
            let (k, v) = entry?;
            let (name, rest) = unesc(k)?;
            let entry = Entry {
                space: RAW,
                tag: 0,
                field: std::str::from_utf8(&name).context("a field name")?,
                key: rest,
                id: dense(v),
                count: 1,
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
        // A field with index writes pending counts the rows a replacement removed
        // until its fold: its histogram is not held to the rows.
        let mut behind: BTreeSet<Vec<u8>> = BTreeSet::new();
        for entry in self.t.index_pending_writes.iter(r)? {
            let (name, _) = unesc(entry?.0)?;
            behind.insert(name);
        }
        let settled = |key: &[u8]| unesc(key).is_ok_and(|(name, _)| !behind.contains(&name));
        // The posting keys of each bucket, counted from the postings.
        let mut keys = Histograms::new();
        for entry in Bitmaps::ValueHolders.db(&self.t).iter(r)? {
            let (k, _) = entry?;
            let (name, rest) = unesc(k)?;
            if let (Some(bucket), Ok(field)) =
                (hist_bucket(&rest[..rest.len() - 2]), String::from_utf8(name))
            {
                *keys.entry(hist_chunks_key(&field, &bucket)).or_default() += 1;
            }
        }
        for (db, expected, what) in [
            (self.t.index_histogram_rows, expected, "rows"),
            (self.t.index_histogram_keys, keys, "posting keys"),
        ] {
            let mut expected = expected.into_iter().filter(|(k, _)| settled(k)).peekable();
            for entry in db.iter(r)? {
                let (k, v) = entry?;
                if !settled(k) {
                    continue;
                }
                let got = from_be(v);
                while let Some((want_k, want)) = expected.next_if(|(want_k, _)| want_k[..] < *k) {
                    diff.push(format!("histogram missing: {}: {want} {what}", show(&want_k)?));
                }
                match expected.next_if(|(want_k, _)| want_k[..] == *k) {
                    Some((_, want)) if want == got => {}
                    Some((_, want)) => diff
                        .push(format!("histogram of {}: {got} {what}, expected {want}", show(k)?)),
                    None => diff.push(format!("histogram unexpected: {}: {got} {what}", show(k)?)),
                }
            }
            for (want_k, want) in expected {
                diff.push(format!("histogram missing: {}: {want} {what}", show(&want_k)?));
            }
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
const SPACES: [&str; 5] = ["set", "parent holder", "descendant", "part", "raw holder"];
const SET: usize = 0;
/// The tag of a set holding a bit of the counts ([`IndexSet::ValueCounts`]): this, plus the
/// bit's number — past every [`IndexSet`].
const COUNT_TAG: u8 = 100;

/// The set a tag of the check stands for, as a line names it.
fn set_name(tag: u8) -> String {
    if tag >= COUNT_TAG {
        return format!("count bit {}", tag - COUNT_TAG);
    }
    let sets = [
        IndexSet::Nothing,
        IndexSet::Parents,
        IndexSet::LongTexts,
        IndexSet::ParentHolders,
        IndexSet::Descendants,
        IndexSet::ValueHolders,
        IndexSet::ValueCounts,
    ];
    sets.into_iter()
        .find(|s| *s as u8 == tag)
        .map_or_else(|| format!("set {tag}"), |s| format!("{s:?}"))
}
const PARENT_HOLDER: usize = 1;
const DESCENDANT: usize = 2;
const PART: usize = 3;
const RAW: usize = 4;

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
    /// The [`IndexSet`], or the partition.
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
    fn set(set: IndexSet, field: &'a str, id: u32) -> Entry<'a> {
        Entry { space: SET, tag: set as u8, field, key: &[], id, count: 1 }
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
        let who = match super::uuid_at(t, r, self.id)? {
            Some(uuid) => uuid.to_string(),
            None => format!("dense id {} (no metarecord)", self.id),
        };
        let field = self.field;
        // What the entry is filed under, for the key spaces keyed by a
        // target's dense id.
        let target = || -> Result<String> {
            let id = dense(self.key);
            Ok(match super::uuid_at(t, r, id)? {
                _ if id == NO_TARGET => "no parent".to_string(),
                Some(uuid) => uuid.to_string(),
                None => format!("dense id {id} (no uuid)"),
            })
        };
        Ok(match self.space {
            SET => format!("({}, {field:?}, {who})", set_name(self.tag)),
            PARENT_HOLDER | DESCENDANT => format!("({field:?}, {}, {who})", target()?),
            PART => format!("({field:?}, {}, {:?}, {who}): {}", self.tag, self.key, self.count),
            _ => format!(
                "({}, {field:?}, {:?}, {who})",
                target()?,
                String::from_utf8_lossy(&self.key[4..])
            ),
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
    /// The keys of its positions in `index_raw_holder`, the field aside: the
    /// parent's dense id · the name.
    raws: BTreeSet<Vec<u8>>,
    /// Each trigram of the field's short texts, with the text's key
    /// ([`text_part_key`]).
    grams: BTreeSet<([u8; 3], GramText)>,
    /// The dense ids of what the rows point at.
    targets: BTreeSet<u32>,
    /// Non-`Nothing` rows.
    valued: usize,
    nothing: bool,
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
        for raw in &self.raws {
            emit(RAW, 0, raw, 1)?;
        }
        for target in &self.targets {
            emit(PARENT_HOLDER, 0, &target.to_be_bytes(), 1)?;
        }
        for (set, holds) in [(IndexSet::Nothing, self.nothing), (IndexSet::LongTexts, self.long)] {
            if holds {
                emit(SET, set as u8, &[], 1)?;
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
/// derive. What must hold is that no text lacks one: a trigram by value lists
/// the text, one by metarecord has the id in its bitmap.
#[derive(Default)]
struct GramCheck {
    /// The trigrams' modes last read, by field and trigram.
    modes: HashMap<(String, [u8; 3]), GramMode>,
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
        for (gram, text) in &group.grams {
            let at = (field.to_string(), *gram);
            if !self.modes.contains_key(&at) {
                if self.modes.len() >= 4096 {
                    self.modes.clear();
                }
                let prefix = [&name_key(field)[..], gram].concat();
                let list = match t.index_trigram_texts.prefix_iter(r, &prefix)?.next() {
                    Some(entry) => {
                        let (k, v) = entry?;
                        Some((k[prefix.len()..].first().copied().unwrap_or(0xFF), v))
                    }
                    None => None,
                };
                let has_ids = list.is_none()
                    && t.index_trigram_holders
                        .prefix_iter(r, &gram_prefix(field, gram))?
                        .next()
                        .is_some();
                self.modes.insert(at.clone(), gram_mode_of(list, has_ids)?);
            }
            let mut in_bitmap = || -> Result<bool> {
                let k =
                    [&gram_prefix(field, gram)[..], &((id >> 16) as u16).to_be_bytes()].concat();
                if !self.chunks.contains_key(&k) {
                    if self.chunks.len() >= 1024 {
                        self.chunks.clear();
                    }
                    let chunk = match t.index_trigram_holders.get(r, &k)? {
                        Some(bytes) => decode_set(bytes)?,
                        None => RoaringBitmap::new(),
                    };
                    self.chunks.insert(k.clone(), chunk);
                }
                Ok(self.chunks[&k].contains(id))
            };
            let found = match &self.modes[&at] {
                GramMode::ByValue(list) => list.binary_search(text).is_ok(),
                GramMode::ByIds => in_bitmap()?,
                GramMode::Absent => false,
            };
            if !found {
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
        for space in [PARENT_HOLDER, DESCENDANT, SET, PART, RAW] {
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

    /// A set added from its encoding, read directly or decoded, is the set:
    /// one id, a few in one chunk, a few over two, many, a run.
    #[test]
    fn test_a_set_added_from_its_encoding_is_the_set() {
        let sets: Vec<RoaringBitmap> = vec![
            RoaringBitmap::from_iter([7]),
            RoaringBitmap::from_iter([3, 9, 65_000]),
            RoaringBitmap::from_iter([5, 70_000]),
            RoaringBitmap::from_iter((0..1000).map(|i| i * 3)),
            RoaringBitmap::from_iter(1..10_000),
            RoaringBitmap::from_iter((0..16).map(|i| (5 << 16) + i * 11)),
            RoaringBitmap::from_iter(0..17),
        ];
        for mut set in sets {
            set.optimize();
            let mut bytes = Vec::new();
            set.serialize_into(&mut bytes).unwrap();
            let mut out = RoaringBitmap::from_iter([1, 200_000]);
            add_encoded(&mut out, &bytes).unwrap();
            assert_eq!(out, &set | &RoaringBitmap::from_iter([1, 200_000]), "{set:?}");
        }
    }

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
        db: impl Fn(&Tables) -> super::super::Db,
        prefix: &[u8],
    ) -> Vec<u8> {
        let r = store.env.read_txn().unwrap();
        let mut keys = db(&store.t).iter(&r).unwrap().map(|e| e.unwrap().0.to_vec());
        keys.find(|k| k.starts_with(prefix)).expect("a key under the prefix")
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

    /// A key of an index begins with its field: no byte of a former kind
    /// before it (doc "Store tables"), and a bucket's count of posting keys
    /// has no `FF` between its field and its bucket.
    #[test]
    fn test_an_index_key_begins_with_its_field() {
        let (store, _dir) = populated();
        let fields = ["loc", "kind", "title", "link", "size"];
        let r = store.env.read_txn().unwrap();
        let t = &store.t;
        let tables = [
            ("index_value_holders", t.index_value_holders, true),
            ("index_value_counts", t.index_value_counts, true),
            ("index_parent_holders", t.index_parent_holders, true),
            ("index_descendants", t.index_descendants, true),
            ("index_parents", t.index_parents, true),
            ("index_trigram_texts", t.index_trigram_texts, true),
            // Only a field with a trigram by metarecord has a state.
            ("index_trigram_state", t.index_trigram_state, false),
            ("index_histogram_rows", t.index_histogram_rows, true),
            ("index_histogram_keys", t.index_histogram_keys, true),
            ("index_trigram_holders", t.index_trigram_holders, false),
            ("index_long_texts", t.index_long_texts, false),
        ];
        for (name, db, filled) in tables {
            let mut n = 0;
            for entry in db.iter(&r).unwrap() {
                let (k, _) = entry.unwrap();
                n += 1;
                let (field, rest) = unesc(k).unwrap();
                let field = String::from_utf8_lossy(&field).to_string();
                assert!(fields.contains(&field.as_str()), "{name}: a key of field {field:?}");
                if name == "index_histogram_keys" {
                    assert_ne!(rest.first(), Some(&0xFF), "{name}: no FF before the bucket");
                }
            }
            assert!(!filled || n > 0, "{name} holds keys");
        }
    }

    #[test]
    fn test_a_healthy_store_checks_clean() {
        let (store, _dir) = populated();
        assert_eq!(store.check_derived().unwrap(), Vec::<String>::new());
    }

    #[test]
    fn test_a_lost_trigram_chunk_is_named() {
        let (store, _dir) = populated();
        // A trigram's list without the first of its texts.
        let key = first_key(&store, |t| t.index_trigram_texts, &gram_prefix("title", b"num"));
        tamper(&store, |t, w| {
            let list = decode_gram_texts(t.index_trigram_texts.get(w, &key).unwrap().unwrap());
            let rest = encode_gram_texts(&list.unwrap()[1..]);
            t.index_trigram_texts.put(w, &key, &rest).unwrap();
        });
        let diff = store.check_derived().unwrap();
        assert!(has(&diff, "gram missing: "), "{}", some(&diff));
        assert!(!has(&diff, "gram unexpected: "), "{}", some(&diff));
        assert!(diff.iter().all(|l| l.starts_with("gram ")), "only the trigrams differ: {diff:?}");
    }

    /// A trigram by metarecord missing an id: more texts share `tit` than a
    /// list holds, so it turned in the write that grew its list past it.
    #[test]
    fn test_an_id_lost_from_a_trigram_bitmap_is_named() {
        let (mut store, _dir) = populated();
        let mut w = Writer::begin(&mut store, None).unwrap();
        for i in 0..GRAM_TEXTS_MAX {
            let title = Value::String(format!("title {i}"));
            w.create_metarecord(vec![Field::new("title", title)]).unwrap();
        }
        w.commit().unwrap();
        assert_eq!(store.check_derived().unwrap(), Vec::<String>::new());
        let key = first_key(&store, |t| t.index_trigram_holders, &gram_prefix("title", b"tit"));
        tamper(&store, |t, w| {
            let mut ids =
                decode_set(t.index_trigram_holders.get(w, &key).unwrap().unwrap()).unwrap();
            let first = ids.min().unwrap();
            ids.remove(first);
            let mut bytes = Vec::new();
            ids.serialize_into(&mut bytes).unwrap();
            t.index_trigram_holders.put(w, &key, &bytes).unwrap();
        });
        let diff = store.check_derived().unwrap();
        assert!(has(&diff, "gram missing: "), "{}", some(&diff));
        assert!(diff.iter().all(|l| l.starts_with("gram ")), "only the trigrams differ: {diff:?}");
    }

    #[test]
    fn test_a_wrong_row_count_is_named_on_both_sides() {
        let (store, _dir) = populated();
        let key = {
            let r = store.env.read_txn().unwrap();
            let (k, _) = store.t.index_raw_holder.iter(&r).unwrap().next().unwrap().unwrap();
            k.to_vec()
        };
        tamper(&store, |t, w| {
            t.index_raw_holder.put(w, &key, &7u32.to_be_bytes()).unwrap();
        });
        let diff = store.check_derived().unwrap();
        assert!(
            has(&diff, "raw holder missing: ") && has(&diff, "raw holder unexpected: "),
            "{}",
            some(&diff)
        );
    }

    #[test]
    fn test_a_lost_set_chunk_is_named_by_its_key_space() {
        let cases: [(&str, Bitmaps, Vec<u8>); 3] = [
            ("set missing: ", Bitmaps::ValueCounts, count_prefix("kind", 0)),
            ("descendant missing: ", Bitmaps::Descendants, vec![]),
            ("parent holder missing: ", Bitmaps::ParentHolders, vec![]),
        ];
        for (expected, table, prefix) in cases {
            let (store, _dir) = populated();
            let key = first_key(&store, |t| table.db(t), &prefix);
            tamper(&store, |t, w| {
                table.db(t).delete(w, &key).unwrap();
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
        let key = first_key(&store, |t| t.index_value_holders, &posting);

        // One holder short: that holder's entry is missing.
        let short = {
            let r = store.env.read_txn().unwrap();
            let mut set =
                decode_set(store.t.index_value_holders.get(&r, &key).unwrap().unwrap()).unwrap();
            set.remove(set.max().unwrap());
            let mut bytes = Vec::new();
            set.serialize_into(&mut bytes).unwrap();
            bytes
        };
        tamper(&store, |t, w| {
            t.index_value_holders.put(w, &key, &short).unwrap();
        });
        let diff = store.check_derived().unwrap();
        assert!(has(&diff, "part missing: "), "{}", some(&diff));
        assert!(!has(&diff, "part unexpected: "), "{}", some(&diff));

        // Gone: every holder's entry is.
        tamper(&store, |t, w| {
            t.index_value_holders.delete(w, &key).unwrap();
        });
        let diff = store.check_derived().unwrap();
        assert!(has(&diff, "part missing: "), "{}", some(&diff));

        // Kept for a value nothing holds.
        let orphan =
            [&posting_prefix("kind", &value_key(&Value::Bool(true)).unwrap())[..], &[0, 0]]
                .concat();
        tamper(&store, |t, w| {
            t.index_value_holders.put(w, &key, &short).unwrap();
            t.index_value_holders.put(w, &orphan, &short).unwrap();
        });
        let diff = store.check_derived().unwrap();
        assert!(has(&diff, "part unexpected: "), "{}", some(&diff));
    }

    #[test]
    fn test_a_histogram_is_required_and_exact() {
        let (store, _dir) = populated();
        let key = first_key(&store, |t| t.index_histogram_rows, &name_key("size"));

        // A count off by one.
        let n = {
            let r = store.env.read_txn().unwrap();
            from_be(store.t.index_histogram_rows.get(&r, &key).unwrap().unwrap())
        };
        tamper(&store, |t, w| {
            t.index_histogram_rows.put(w, &key, &be(n + 1)).unwrap();
        });
        let diff = store.check_derived().unwrap();
        assert!(has(&diff, "histogram of "), "{}", some(&diff));

        // A bucket gone.
        tamper(&store, |t, w| {
            t.index_histogram_rows.delete(w, &key).unwrap();
        });
        let diff = store.check_derived().unwrap();
        assert!(has(&diff, "histogram missing: "), "{}", some(&diff));

        // A bucket nothing fills.
        let bogus =
            [&name_key("size")[..], &hist_bucket(&value_key(&Value::Int(-7)).unwrap()).unwrap()]
                .concat();
        tamper(&store, |t, w| {
            t.index_histogram_rows.put(w, &key, &be(n)).unwrap();
            t.index_histogram_rows.put(w, &bogus, &be(3)).unwrap();
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
            from_be(
                store.t.index_histogram_keys.get(&r, &key).unwrap().expect("the keys are counted"),
            )
        };
        assert!(n >= 1, "{n}");

        // Off by one.
        tamper(&store, |t, w| {
            t.index_histogram_keys.put(w, &key, &be(n + 1)).unwrap();
        });
        let diff = store.check_derived().unwrap();
        assert!(has(&diff, "histogram of "), "{}", some(&diff));

        // Gone.
        tamper(&store, |t, w| {
            t.index_histogram_keys.delete(w, &key).unwrap();
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
            t.index_value_counts.put(w, &key, &bytes).unwrap();
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
    pub pending_writes: usize,
    pub pending_write_parts: usize,
    pub pending_write_bytes: u64,
    /// Pending index writes, the fields they are in, their bytes.
    pub index_writes: usize,
    pub index_write_fields: usize,
    pub index_write_bytes: u64,
    /// Dense ids ever handed out, and those of metarecords that exist.
    pub ids_allocated: u64,
    pub ids_live: u64,
}

impl Wear {
    /// The bytes a fold would write in.
    pub fn pending_bytes(&self) -> u64 {
        self.pending_write_bytes + self.index_write_bytes
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
