//! The pending index writes of a field's values (doc "Mass writes in the store").
//!
//! A write to many records at once does not file each record under the value
//! it gains, nor take each out of the values it loses — which it would have
//! to read: it appends one *index write* to the field's list, naming the operation
//! and the records. A value's holders are then what is stored — its run of
//! the value partition or its posting, and for a reference its referrers —
//! with the field's index writes replayed over it, in order:
//!
//! | index write                    | a value's holders become                    |
//! |--------------------------|---------------------------------------------|
//! | add `v` to `B`           | `∪ B` for `v`                               |
//! | remove `v` from `B`      | `∖ B` for `v`                               |
//! | replace by `vals` on `B` | `∖ B`, then `∪ B` for the values of `vals`  |
//!
//! A replacement never names the values it removes. While a field has index writes
//! pending, a write to one record of it is an index write too — of one id — or the
//! replay would put an older index write over it. A *fold* writes the index writes into
//! what is stored and empties the list.
//!
//! | table  | key                 | value                               |
//! |--------|---------------------|-------------------------------------|
//! | index_pending_writes | field · chunk · number | kind · values · the ids of the chunk |

use std::collections::BTreeSet;

use anyhow::{bail, ensure, Context, Result};
use heed::RoTxn;
use metafolder_core::metarecord::Value;
use roaring::RoaringBitmap;

use super::super::{get_value, name_key, put_value, In, KvStore, KvTxn, Out, Tables};
use super::{
    count_prefix, decode_set, hist_bucket, hist_key, posting_prefix, search_text, set_prefix,
    unesc, value_key, Bitmaps, IndexSet, COUNT_BITS, TRIGRAM_MAX,
};

pub(crate) use crate::store::MassKind as Kind;

/// One pending index write of a field.
#[derive(Clone)]
pub(crate) struct IndexWrite {
    pub(crate) kind: Kind,
    pub(crate) values: Vec<Value>,
    /// The value keys of `values`, those of a `Nothing` left out.
    pub(crate) keys: Vec<Vec<u8>>,
    pub(crate) ids: RoaringBitmap,
}

impl IndexWrite {
    pub(crate) fn new(kind: Kind, values: Vec<Value>, ids: RoaringBitmap) -> IndexWrite {
        let keys = values.iter().filter_map(value_key).collect();
        IndexWrite { kind, values, keys, ids }
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Out::default();
        out.int(match self.kind {
            Kind::Add => 0,
            Kind::Remove => 1,
            Kind::Replace => 2,
        })
        .int(self.values.len() as i64);
        for v in &self.values {
            put_value(&mut out, v);
        }
        let mut ids = self.ids.clone();
        ids.optimize();
        let mut bytes = Vec::with_capacity(ids.serialized_size());
        ids.serialize_into(&mut bytes).expect("writing to a vector");
        out.bytes(&bytes);
        out.0
    }

    fn decode(bytes: &[u8]) -> Result<IndexWrite> {
        let mut r = In(bytes);
        let kind = match r.int()? {
            0 => Kind::Add,
            1 => Kind::Remove,
            2 => Kind::Replace,
            other => bail!("an event of kind {other}"),
        };
        let n = r.int()?;
        let mut values = Vec::new();
        for _ in 0..n {
            values.push(get_value(&mut r)?);
        }
        let ids = decode_set(r.bytes()?)?;
        Ok(IndexWrite::new(kind, values, ids))
    }
}

/// The key of a pending index write of `field`, in `chunk`, of write number
/// `seq`.
fn index_write_key(field: &str, chunk: u16, seq: i64) -> Vec<u8> {
    [&name_key(field)[..], &chunk.to_be_bytes(), &seq.to_be_bytes()].concat()
}

/// `ids` cut by chunk of 65 536 ids, in the order of the chunks.
pub(crate) fn by_chunk(ids: &RoaringBitmap) -> Vec<(u16, RoaringBitmap)> {
    let (Some(min), Some(max)) = (ids.min(), ids.max()) else { return Vec::new() };
    let mut out = Vec::new();
    for chunk in (min >> 16)..=(max >> 16) {
        let lo = chunk << 16;
        let mut part = RoaringBitmap::new();
        part.insert_range(lo..=lo | 0xFFFF);
        part &= ids;
        if !part.is_empty() {
            out.push((chunk as u16, part));
        }
    }
    out
}

/// `field`'s pending index writes, chunk by chunk, oldest first in each: for any
/// one id, the order they were written in.
pub(crate) fn read_index_writes(t: &Tables, r: &RoTxn<'_>, field: &str) -> Result<Vec<IndexWrite>> {
    let mut out = Vec::new();
    for entry in t.index_pending_writes.prefix_iter(r, &name_key(field))? {
        out.push(IndexWrite::decode(entry?.1)?);
    }
    Ok(out)
}

/// The holders of the value of key `key`, `stored` with `index_writes` replayed.
pub(crate) fn replay(
    index_writes: &[IndexWrite],
    key: &[u8],
    mut holders: RoaringBitmap,
) -> RoaringBitmap {
    for e in index_writes {
        let named = e.keys.iter().any(|k| k == key);
        match e.kind {
            Kind::Add if named => holders |= &e.ids,
            Kind::Remove if named => holders -= &e.ids,
            Kind::Replace => {
                holders -= &e.ids;
                if named {
                    holders |= &e.ids;
                }
            }
            _ => {}
        }
    }
    holders
}

/// Whether `id`, which held (`held`) or not the value of key `key`, holds it
/// once `index_writes` are replayed.
pub(crate) fn replay_one(index_writes: &[IndexWrite], key: &[u8], id: u32, mut held: bool) -> bool {
    for e in index_writes.iter().filter(|e| e.ids.contains(id)) {
        let named = e.keys.iter().any(|k| k == key);
        match e.kind {
            Kind::Add if named => held = true,
            Kind::Remove if named => held = false,
            Kind::Replace => held = named,
            _ => {}
        }
    }
    held
}

/// The keys `index_writes` name: the only values whose holders an index write may add to.
/// Any other value only loses the records a replacement was written on
/// ([`replaced`]).
pub(crate) fn named(index_writes: &[IndexWrite]) -> BTreeSet<Vec<u8>> {
    index_writes.iter().flat_map(|e| e.keys.iter().cloned()).collect()
}

/// The records `index_writes` replaced the field of: what a value no index write names
/// has lost.
pub(crate) fn replaced(index_writes: &[IndexWrite]) -> RoaringBitmap {
    let mut out = RoaringBitmap::new();
    for e in index_writes.iter().filter(|e| e.kind == Kind::Replace) {
        out |= &e.ids;
    }
    out
}

impl KvStore {
    /// How many index writes `field` has pending.
    pub fn pending_index_writes(&self, field: &str) -> Result<usize> {
        let r = self.env.read_txn()?;
        Ok(read_index_writes(&self.t, &r, field)?.len())
    }
}

impl KvTxn<'_> {
    /// How many index writes `field` has pending, as the transaction sees it.
    pub(in super::super) fn eventful(&self, field: &str) -> Result<usize> {
        if let Some(n) = self.index_writes.borrow().get(field) {
            return Ok(*n);
        }
        let prefix = name_key(field);
        let n = self.t.index_pending_writes.prefix_iter(&self.txn.borrow(), &prefix)?.count();
        self.index_writes.borrow_mut().insert(field.to_string(), n);
        Ok(n)
    }

    /// Whether a write of `values` to many records of `field` can be one
    /// index write: not on a forest, whose positions are written one by one (doc
    /// "One position per forest"); nor the removal of a text too long for
    /// trigrams, the long-text set needing to know whether another such text
    /// stays on the record. A `Nothing` is the holders of `nothing` in the field, and a
    /// text past its key's length a posting under a hashed key, like any
    /// value.
    pub(in super::super) fn mass_eligible(
        &self,
        field: &str,
        kind: Kind,
        values: &[Value],
    ) -> Result<bool> {
        for entry in self.t.field_types.prefix_iter(&self.txn.borrow(), &name_key(field))? {
            let (_, ty) = unesc(entry?.0)?;
            if super::super::codec::is_position_type(ty) {
                return Ok(false);
            }
        }
        let plain = |v: &Value| {
            v.position().is_none()
                && (kind != Kind::Remove || search_text(v).is_none_or(|t| t.len() <= TRIGRAM_MAX))
        };
        Ok(values.iter().all(plain) && (kind == Kind::Replace || values.len() == 1))
    }

    /// The rows of a write to many records of `field`, `ids`, are written:
    /// what they derive, as one index write on `ids` and a few set operations.
    pub(in super::super) fn mass_end_ids(
        &self,
        field: &str,
        kind: Kind,
        values: Vec<Value>,
        ids: RoaringBitmap,
    ) -> Result<()> {
        let listed = values.clone();
        if ids.is_empty() {
            return Ok(());
        }
        // A target no metarecord has gets its id now, as a write to one
        // record gives it: the index write names it by that id until the fold.
        for value in &values {
            if let Value::Ref(target) = value {
                self.target_id(target.as_bytes())?;
            }
        }
        let index_write = IndexWrite::new(kind, values, ids);
        // A `Nothing` added or removed is the holders of `nothing` in the field, at once:
        // no value is counted, and no index write is needed to replay it.
        if kind != Kind::Replace && index_write.keys.is_empty() {
            let nothing = set_prefix(IndexSet::Nothing, Some(field));
            self.chunk_apply(
                IndexSet::Nothing.table(),
                &nothing,
                &index_write.ids,
                kind == Kind::Add,
            )?;
            return self.append_entries(kind, field, &listed, &index_write.ids);
        }
        match kind {
            Kind::Add | Kind::Remove => {
                let (value, key) = (&index_write.values[0], &index_write.keys[0]);
                let have = self.effective(field, value, key)?;
                if kind == Kind::Add {
                    let gain = &index_write.ids - &have;
                    self.count_step(field, &gain, true)?;
                    self.hist_bump(field, key, gain.len() as i64)?;
                    self.mass_grams(field, value, &gain)?;
                    if search_text(value).is_some_and(|t| t.len() > TRIGRAM_MAX) {
                        self.chunk_apply(
                            IndexSet::LongTexts.table(),
                            &set_prefix(IndexSet::LongTexts, Some(field)),
                            &gain,
                            true,
                        )?;
                    }
                } else {
                    let lose = &index_write.ids & &have;
                    self.count_step(field, &lose, false)?;
                    self.hist_bump(field, key, -(lose.len() as i64))?;
                }
            }
            Kind::Replace => {
                let n = index_write.keys.len() as u64;
                for bit in 0..COUNT_BITS {
                    let prefix = count_prefix(field, bit);
                    self.chunk_apply(
                        Bitmaps::ValueCounts,
                        &prefix,
                        &index_write.ids,
                        n >> bit & 1 == 1,
                    )?;
                }
                let unknown = index_write.values.iter().any(|v| matches!(v, Value::Nothing));
                self.chunk_apply(
                    IndexSet::Nothing.table(),
                    &set_prefix(IndexSet::Nothing, Some(field)),
                    &index_write.ids,
                    unknown,
                )?;
                let long = index_write
                    .values
                    .iter()
                    .filter_map(search_text)
                    .any(|t| t.len() > TRIGRAM_MAX);
                self.chunk_apply(
                    IndexSet::LongTexts.table(),
                    &set_prefix(IndexSet::LongTexts, Some(field)),
                    &index_write.ids,
                    long,
                )?;
                for (value, key) in index_write
                    .values
                    .iter()
                    .filter(|v| value_key(v).is_some())
                    .zip(&index_write.keys)
                {
                    // The rows the replacement removed leave their buckets
                    // at the fold, which is what knows them.
                    self.hist_bump(field, key, index_write.ids.len() as i64)?;
                    self.mass_grams(field, value, &index_write.ids)?;
                }
            }
        }
        match kind {
            // An add or a removal goes into the value's posting in every
            // chunk where the field has nothing pending — nearly always:
            // `∪ B` or `∖ B`, one key per chunk. Behind a pending write of a
            // chunk, it waits too, or the replay of the older one would
            // undo it.
            Kind::Add | Kind::Remove => {
                let (direct, waiting) = self.split_pending(field, &index_write.ids)?;
                if !direct.is_empty() {
                    let (value, key) = (&index_write.values[0], &index_write.keys[0]);
                    let have = self.stored_holders(field, key)?;
                    let add = kind == Kind::Add;
                    let ids = if add { &direct - &have } else { &direct & &have };
                    self.store_holders(field, Some(value), key, &ids, add)?;
                }
                if !waiting.is_empty() {
                    self.append_index_write(
                        field,
                        &IndexWrite { ids: waiting, ..index_write.clone() },
                    )?;
                }
            }
            // A set or an unset does not know the values it removes: it
            // waits for the fold, which finds them.
            Kind::Replace => self.append_index_write(field, &index_write)?,
        }
        // The rows the write did not put into the records.
        self.append_entries(kind, field, &listed, &index_write.ids)
    }

    /// The chunks in which `field` has pending index writes, as the
    /// transaction sees them.
    fn pending_chunks(&self, field: &str) -> Result<BTreeSet<u16>> {
        let mut out = BTreeSet::new();
        if self.eventful(field)? == 0 {
            return Ok(out);
        }
        let prefix = name_key(field);
        for entry in self.t.index_pending_writes.prefix_iter(&self.txn.borrow(), &prefix)? {
            let k = entry?.0;
            let chunk = k.get(prefix.len()..prefix.len() + 2).context("a chunk in the key")?;
            out.insert(u16::from_be_bytes(chunk.try_into().expect("two bytes")));
        }
        Ok(out)
    }

    /// Whether `field` has pending index writes in the chunk of `id`: a row
    /// of one record written there is one more, after them.
    pub(in super::super) fn pending_at(&self, field: &str, id: u32) -> Result<bool> {
        if self.eventful(field)? == 0 {
            return Ok(false);
        }
        let prefix = [&name_key(field)[..], &((id >> 16) as u16).to_be_bytes()].concat();
        Ok(self.t.index_pending_writes.prefix_iter(&self.txn.borrow(), &prefix)?.next().is_some())
    }

    /// `ids` split into those of the chunks where `field` has nothing
    /// pending, and the others.
    fn split_pending(
        &self,
        field: &str,
        ids: &RoaringBitmap,
    ) -> Result<(RoaringBitmap, RoaringBitmap)> {
        let chunks = self.pending_chunks(field)?;
        if chunks.is_empty() {
            return Ok((ids.clone(), RoaringBitmap::new()));
        }
        let mut waiting = RoaringBitmap::new();
        for (chunk, part) in by_chunk(ids) {
            if chunks.contains(&chunk) {
                waiting |= part;
            }
        }
        Ok((ids - &waiting, waiting))
    }

    /// A row of one record came or went in a field that has index writes pending:
    /// an index write of one id, in its place after them.
    pub(in super::super) fn single_index_write(
        &self,
        field: &str,
        value: &Value,
        id: u32,
        add: bool,
    ) -> Result<()> {
        let kind = if add { Kind::Add } else { Kind::Remove };
        let index_write =
            IndexWrite::new(kind, vec![value.clone()], RoaringBitmap::from_iter([id]));
        self.append_index_write(field, &index_write)
    }

    /// Files `index_write` under one write number, one key per chunk of 65 536 ids
    /// it names — `field · chunk · number`, as the postings are chunked — so
    /// that a chunk's pending index writes are its own (doc "Mass writes in
    /// the store"). Chunks name disjoint ids: replaying a field's keys in the
    /// order of the table, chunk by chunk, is replaying them in the order of
    /// the numbers for each id.
    fn append_index_write(&self, field: &str, index_write: &IndexWrite) -> Result<()> {
        let pending = self.eventful(field)?;
        let seq = self.next(super::super::State::NextWriteNumber)?;
        let mut filed = 0;
        for (chunk, ids) in by_chunk(&index_write.ids) {
            let key = index_write_key(field, chunk, seq);
            let part = IndexWrite { ids, ..index_write.clone() };
            self.t.index_pending_writes.put(&mut self.txn.borrow_mut(), &key, &part.encode())?;
            filed += 1;
        }
        self.index_writes.borrow_mut().insert(field.to_string(), pending + filed);
        Ok(())
    }

    /// The trigrams of a text `ids` gained in `field` (doc "Text search in
    /// the store"): a text new to the field joins its trigrams' lists; in a
    /// field with trigrams by metarecord, the ids are pending.
    fn mass_grams(&self, field: &str, value: &Value, ids: &RoaringBitmap) -> Result<()> {
        // A text too long has no trigrams: the long-text set holds it.
        let Some(text) = search_text(value).filter(|t| t.len() <= TRIGRAM_MAX) else {
            return Ok(());
        };
        if ids.is_empty() {
            return Ok(());
        }
        if let Some(new) = self.new_text(field, value)? {
            self.list_new_text(field, &text, &new)?;
        }
        if self.gram_state(field)?.by_ids {
            self.chunk_apply(
                IndexSet::TrigramPending.table(),
                &set_prefix(IndexSet::TrigramPending, Some(field)),
                ids,
                true,
            )?;
        }
        Ok(())
    }

    fn hist_bump(&self, field: &str, key: &[u8], delta: i64) -> Result<()> {
        match hist_bucket(key) {
            Some(bucket) if delta != 0 => self.bump_hist(&hist_key(field, &bucket), delta),
            _ => Ok(()),
        }
    }

    /// One more (`up`) or one less value of `field` on every id of `ids`: an
    /// increment of their counts, kept a set per bit — a carry from bit to
    /// bit, in a few set operations whatever the number of ids.
    fn count_step(&self, field: &str, ids: &RoaringBitmap, up: bool) -> Result<()> {
        let mut carry = ids.clone();
        for bit in 0..COUNT_BITS {
            if carry.is_empty() {
                return Ok(());
            }
            let prefix = count_prefix(field, bit);
            let set = &self.read_chunked(Bitmaps::ValueCounts, &prefix)? & &carry;
            let clear = &carry - &set;
            // Up: a set bit clears and carries on, a clear one sets and
            // stops. Down: a clear bit sets and borrows on, a set one clears
            // and stops.
            self.chunk_apply(Bitmaps::ValueCounts, &prefix, &set, false)?;
            self.chunk_apply(Bitmaps::ValueCounts, &prefix, &clear, true)?;
            carry = if up { set } else { clear };
        }
        ensure!(carry.is_empty(), "a count of values of '{field}' out of its bits");
        Ok(())
    }

    /// The holders of a value as stored: its posting.
    fn stored_holders(&self, field: &str, key: &[u8]) -> Result<RoaringBitmap> {
        self.read_chunked(Bitmaps::ValueHolders, &super::posting_prefix(field, key))
    }

    /// The holders of a value as the transaction sees them: stored, the
    /// field's index writes replayed.
    pub(in super::super) fn effective(
        &self,
        field: &str,
        _value: &Value,
        key: &[u8],
    ) -> Result<RoaringBitmap> {
        let stored = self.stored_holders(field, key)?;
        if self.eventful(field)? == 0 {
            return Ok(stored);
        }
        let index_writes = read_index_writes(&self.t, &self.txn.borrow(), field)?;
        Ok(replay(&index_writes, key, stored))
    }

    /// Files `ids` under a value, or takes them out — in the value
    /// partition, the value's posting, and a reference's referrers.
    fn store_holders(
        &self,
        field: &str,
        value: Option<&Value>,
        key: &[u8],
        ids: &RoaringBitmap,
        add: bool,
    ) -> Result<()> {
        let _ = value;
        self.chunk_apply(Bitmaps::ValueHolders, &posting_prefix(field, key), ids, add)?;
        Ok(())
    }

    /// Writes `field`'s pending index writes into what is stored, chunk by
    /// chunk ([`Self::fold_index_writes_chunk`]), and empties its list.
    pub(in super::super) fn fold_index_writes(&self, field: &str) -> Result<()> {
        for chunk in self.pending_chunks(field)? {
            self.fold_index_writes_chunk(field, chunk)?;
        }
        Ok(())
    }

    /// Writes `field`'s pending index writes of one chunk of 65 536 ids into
    /// its postings of that chunk, and deletes them: what one fold step holds
    /// in memory and how long it lasts are bounded by a chunk (doc "Folding in
    /// idle time"). Every pending write is replayed, oldest first: a
    /// replacement removes the ids it names from every value the field has in
    /// the chunk — it does not name the values it removes — and its histogram
    /// buckets lose those rows here, the values it adds having been counted
    /// when it was written (doc "Mass writes in the store").
    pub(in super::super) fn fold_index_writes_chunk(&self, field: &str, chunk: u16) -> Result<()> {
        let t = self.t;
        let prefix = [&name_key(field)[..], &chunk.to_be_bytes()].concat();
        let (keys, index_writes) = {
            let txn = self.txn.borrow();
            let mut keys = Vec::new();
            let mut index_writes = Vec::new();
            for entry in t.index_pending_writes.prefix_iter(&txn, &prefix)? {
                let (k, v) = entry?;
                keys.push(k.to_vec());
                index_writes.push(IndexWrite::decode(v)?);
            }
            (keys, index_writes)
        };
        if index_writes.is_empty() {
            return Ok(());
        }
        let mut values = self.values_in_chunk(field, chunk)?;
        for e in &index_writes {
            let valued = e.values.iter().filter(|v| value_key(v).is_some());
            match e.kind {
                Kind::Add | Kind::Remove => {
                    let add = e.kind == Kind::Add;
                    for key in valued.zip(&e.keys).map(|(_, k)| k) {
                        self.with_chunk(
                            Bitmaps::ValueHolders,
                            &posting_prefix(field, key),
                            chunk,
                            &mut |bm| {
                                if add {
                                    *bm |= &e.ids;
                                } else {
                                    *bm -= &e.ids;
                                }
                            },
                        )?;
                        if add {
                            values.insert(key.clone());
                        }
                    }
                }
                Kind::Replace => {
                    for key in &values {
                        let mut gone = 0;
                        self.with_chunk(
                            Bitmaps::ValueHolders,
                            &posting_prefix(field, key),
                            chunk,
                            &mut |bm| {
                                gone = bm.intersection_len(&e.ids);
                                *bm -= &e.ids;
                            },
                        )?;
                        self.hist_bump(field, key, -(gone as i64))?;
                    }
                    for key in valued.zip(&e.keys).map(|(_, k)| k) {
                        self.with_chunk(
                            Bitmaps::ValueHolders,
                            &posting_prefix(field, key),
                            chunk,
                            &mut |bm| *bm |= &e.ids,
                        )?;
                        values.insert(key.clone());
                    }
                }
            }
        }
        let mut w = self.txn.borrow_mut();
        for k in &keys {
            t.index_pending_writes.delete(&mut w, k)?;
        }
        drop(w);
        let left = self.eventful(field)?.saturating_sub(keys.len());
        self.index_writes.borrow_mut().insert(field.to_string(), left);
        Ok(())
    }

    /// The value keys `field` has a posting of in `chunk`, as the transaction
    /// sees them — `nothing` left out: its posting is kept exact by the write
    /// itself, never replayed (doc "Mass writes in the store"). A walk of the
    /// keys of the field's postings, not of their bitmaps.
    fn values_in_chunk(&self, field: &str, chunk: u16) -> Result<BTreeSet<Vec<u8>>> {
        let posted = name_key(field);
        let tail = chunk.to_be_bytes();
        let mut keys = BTreeSet::new();
        {
            let txn = self.txn.borrow();
            for entry in Bitmaps::ValueHolders.db(&self.t).prefix_iter(&txn, &posted)? {
                let k = entry?.0;
                if k.len() >= posted.len() + 2 && k.ends_with(&tail) {
                    keys.insert(k[posted.len()..k.len() - 2].to_vec());
                }
            }
        }
        // The postings this transaction made are not in the table yet.
        let whole = [&[Bitmaps::ValueHolders as u8][..], &posted[..]].concat();
        for k in self.sets.borrow().keys() {
            if k.starts_with(&whole) && k.len() >= whole.len() + 2 && k.ends_with(&tail) {
                keys.insert(k[whole.len()..k.len() - 2].to_vec());
            }
        }
        keys.remove(&[super::super::derived::NOTHING_TAG][..]);
        Ok(keys)
    }

    /// Every value of `field` that has a holder as stored, the transaction's
    /// own writes included, with its holders: its postings, whole.
    pub(in super::super) fn postings_of(
        &self,
        field: &str,
    ) -> Result<Vec<(Vec<u8>, RoaringBitmap)>> {
        let posted = name_key(field);
        let mut keys = BTreeSet::new();
        {
            let txn = self.txn.borrow();
            for entry in Bitmaps::ValueHolders.db(&self.t).prefix_iter(&txn, &posted)? {
                let k = entry?.0;
                keys.insert(k[posted.len()..k.len() - 2].to_vec());
            }
            // `nothing` is not one of the values a replacement removes here.
            keys.remove(&[super::super::derived::NOTHING_TAG][..]);
        }
        // The postings this transaction made are not in the table yet.
        let whole = [&[Bitmaps::ValueHolders as u8][..], &posted[..]].concat();
        for k in self.sets.borrow().keys() {
            if k.starts_with(&whole) && k.len() >= whole.len() + 2 {
                keys.insert(k[whole.len()..k.len() - 2].to_vec());
            }
        }
        keys.remove(&[super::super::derived::NOTHING_TAG][..]);
        let mut out = Vec::new();
        for key in keys {
            let holders = self.read_chunked(Bitmaps::ValueHolders, &posting_prefix(field, &key))?;
            if !holders.is_empty() {
                out.push((key, holders));
            }
        }
        Ok(out)
    }

    /// The values `field` holds on `ids` as the transaction sees them, each
    /// with the places — among `ids`, in the order of their ids — of the
    /// records holding it; a `Nothing` row too. What a replacement of the
    /// field on `ids` is about to take off, for the log to say.
    ///
    /// It reads the field's values, not its records: the postings, the index writes
    /// pending — and one record for each
    /// distinct value found, to give back the value itself (a key is not
    /// always its value: an int and a float of one number share it).
    pub(in super::super) fn values_held(
        &self,
        field: &str,
        ids: &RoaringBitmap,
    ) -> Result<Vec<(Value, RoaringBitmap)>> {
        let index_writes = read_index_writes(&self.t, &self.txn.borrow(), field)?;
        let mut stored: std::collections::BTreeMap<Vec<u8>, RoaringBitmap> = Default::default();
        for key in named(&index_writes) {
            stored.entry(key).or_default();
        }
        for (key, holders) in self.postings_of(field)? {
            *stored.entry(key).or_default() |= &(holders & ids);
        }
        let places = |holders: &RoaringBitmap| -> RoaringBitmap {
            holders.iter().map(|id| (ids.rank(id) - 1) as u32).collect()
        };
        let mut out = Vec::new();
        for (key, held) in stored {
            let holders = &replay(&index_writes, &key, held) & ids;
            let Some(first) = holders.min() else { continue };
            let rows = self.record_rows(first)?;
            let value = rows
                .into_iter()
                .find(|r| r.name == field && value_key(&r.value).as_deref() == Some(&key[..]))
                .with_context(|| format!("a row of '{field}' that its holder lacks"))?
                .value;
            out.push((value, places(&holders)));
        }
        let unknown = &self
            .read_chunked(IndexSet::Nothing.table(), &set_prefix(IndexSet::Nothing, Some(field)))?
            & ids;
        if !unknown.is_empty() {
            out.push((Value::Nothing, places(&unknown)));
        }
        Ok(out)
    }

    /// [`Self::fold_index_writes`] for every field with index writes pending.
    pub(in super::super) fn fold_all_index_writes(&self) -> Result<()> {
        let mut fields = BTreeSet::new();
        for entry in self.t.index_pending_writes.iter(&self.txn.borrow())? {
            let (name, _) = unesc(entry?.0)?;
            fields.insert(String::from_utf8(name).context("a field name")?);
        }
        for field in fields {
            self.fold_index_writes(&field)?;
        }
        Ok(())
    }

    /// Folds the biggest list pending (see `KvStore::fold_step`): size is the
    /// bytes of a part's entries or of a field's index writes, the ids pending of a
    /// field's trigrams. Returns whether any list was.
    pub(in super::super) fn fold_biggest(&self) -> Result<bool> {
        // A deletion first, whatever the size of the others: its records are
        // read with the entries of their parts still applied.
        if self.fold_one_delete()? {
            return Ok(true);
        }
        enum List {
            Part(u32),
            IndexWrites(String, u16),
            Grams(String),
        }
        let mut best: Option<(u64, List)> = None;
        let mut consider = |size: u64, list: List| {
            if size > 0 && best.as_ref().is_none_or(|(b, _)| size > *b) {
                best = Some((size, list));
            }
        };
        let mut parts: std::collections::BTreeMap<u32, u64> = Default::default();
        for entry in self.t.pending_writes.iter(&self.txn.borrow())? {
            let (k, v) = entry?;
            let part = u32::from_be_bytes(k[..4].try_into().expect("a part"));
            *parts.entry(part).or_default() += (k.len() + v.len()) as u64;
        }
        for (part, size) in parts {
            consider(size, List::Part(part));
        }
        // A field's pending index writes, chunk by chunk: a step folds one.
        let mut chunks: std::collections::BTreeMap<(String, u16), u64> = Default::default();
        for entry in self.t.index_pending_writes.iter(&self.txn.borrow())? {
            let (k, v) = entry?;
            let (name, rest) = unesc(k)?;
            let name = String::from_utf8(name).context("a field name")?;
            let chunk = rest.get(..2).context("a chunk in the key")?;
            let chunk = u16::from_be_bytes(chunk.try_into().expect("two bytes"));
            *chunks.entry((name, chunk)).or_default() += (k.len() + v.len()) as u64;
        }
        for ((field, chunk), size) in chunks {
            consider(size, List::IndexWrites(field, chunk));
        }
        for field in self.gram_fields()? {
            consider(self.gram_fold_size(&field)?, List::Grams(field));
        }
        match best {
            // Last, the history of the records, which the writes leave to the
            // fold (doc "Log storage").
            None => self.index_log_step(super::super::entry::INDEX_BUDGET),
            Some((_, List::Part(part))) => self.fold_part(part).map(|()| true),
            Some((_, List::IndexWrites(field, chunk))) => {
                self.fold_index_writes_chunk(&field, chunk).map(|()| true)
            }
            Some((_, List::Grams(field))) => self.fold_gram_field(&field).map(|()| true),
        }
    }
}

#[cfg(test)]
mod tests {
    use metafolder_core::metarecord::{Field, Value};
    use uuid::Uuid;

    use super::super::super::{test_store, KvStore, KvTxn, State, TestDir};
    use super::*;
    use crate::log::Writer;

    fn s(text: &str) -> Value {
        Value::String(text.into())
    }

    /// Twelve records of `tag` `a`, six in the first chunk of ids and six in
    /// the second, folded; then, pending in both chunks, a set to `b`, an add
    /// of `c` and a removal of `b` on overlapping ids.
    fn two_chunks() -> (KvStore, TestDir, Vec<Uuid>) {
        let (mut store, dir) = test_store();
        let txn = KvTxn::new(&mut store).unwrap();
        txn.meta_put(State::NextMetarecordId, (1 << 16) - 6).unwrap();
        txn.finish().unwrap();
        let mut w = Writer::begin(&mut store, None).unwrap();
        let uuids: Vec<Uuid> = (0..12)
            .map(|_| w.create_metarecord(vec![Field::new("tag", s("a"))]).unwrap().uuid)
            .collect();
        w.commit().unwrap();
        store.fold().unwrap();
        let mut w = Writer::begin(&mut store, None).unwrap();
        w.mass_set("tag", vec![s("b")], &uuids[2..10]).unwrap();
        w.mass_add("tag", s("c"), &uuids[0..8]).unwrap();
        w.mass_remove("tag", &s("b"), &uuids[4..12]).unwrap();
        w.commit().unwrap();
        (store, dir, uuids)
    }

    /// The chunks `field` has pending index writes in.
    fn chunks_pending(store: &mut KvStore, field: &str) -> BTreeSet<u16> {
        let txn = KvTxn::new(store).unwrap();
        txn.pending_chunks(field).unwrap()
    }

    /// Each value's holders, as a transaction reads them.
    fn holders(store: &mut KvStore) -> Vec<RoaringBitmap> {
        let txn = KvTxn::new(store).unwrap();
        ["a", "b", "c"]
            .iter()
            .map(|v| {
                let key = value_key(&s(v)).unwrap();
                txn.effective("tag", &s(v), &key).unwrap()
            })
            .collect()
    }

    fn check(store: &KvStore) {
        let diff = store.check_derived().unwrap();
        assert!(diff.is_empty(), "derived data diverges from a rebuild:\n{}", diff.join("\n"));
    }

    #[test]
    fn the_fold_of_a_fields_chunk_reads_and_writes_that_chunk_only() {
        let (mut store, _dir, _) = two_chunks();
        assert_eq!(chunks_pending(&mut store, "tag"), BTreeSet::from([0, 1]));
        let txn = KvTxn::new(&mut store).unwrap();
        txn.fold_index_writes_chunk("tag", 0).unwrap();
        // The postings of the field the fold loaded: of its chunk only.
        let posted = [&[Bitmaps::ValueHolders as u8][..], &name_key("tag")].concat();
        let loaded: BTreeSet<u16> = txn
            .sets
            .borrow()
            .keys()
            .filter(|k| k.starts_with(&posted))
            .map(|k| u16::from_be_bytes([k[k.len() - 2], k[k.len() - 1]]))
            .collect();
        assert_eq!(loaded, BTreeSet::from([0]), "chunks of the field's postings read");
        txn.finish().unwrap();
        assert_eq!(chunks_pending(&mut store, "tag"), BTreeSet::from([1]));
        check(&store);
    }

    #[test]
    fn a_field_reads_the_same_unfolded_half_folded_and_folded() {
        let (mut store, _dir, _) = two_chunks();
        let before = holders(&mut store);
        // a: 0, 1, 10, 11 (the set took the others); b: 2, 3; c: 0 to 7 —
        // as dense ids, from the first.
        let first = (1u32 << 16) - 6;
        let ids = |of: &[u32]| of.iter().map(|i| first + i).collect::<RoaringBitmap>();
        assert_eq!(
            before,
            vec![ids(&[0, 1, 10, 11]), ids(&[2, 3]), ids(&[0, 1, 2, 3, 4, 5, 6, 7])]
        );
        let txn = KvTxn::new(&mut store).unwrap();
        txn.fold_index_writes_chunk("tag", 1).unwrap();
        txn.finish().unwrap();
        assert_eq!(holders(&mut store), before, "half folded");
        check(&store);
        let txn = KvTxn::new(&mut store).unwrap();
        txn.fold_index_writes_chunk("tag", 0).unwrap();
        txn.finish().unwrap();
        assert_eq!(holders(&mut store), before, "folded");
        assert!(chunks_pending(&mut store, "tag").is_empty());
        check(&store);
    }

    #[test]
    fn a_fold_step_folds_one_chunk_of_a_field() {
        let (mut store, _dir, _) = two_chunks();
        let before = holders(&mut store);
        let mut seen = vec![chunks_pending(&mut store, "tag")];
        while store.fold_step().unwrap() {
            let pending = chunks_pending(&mut store, "tag");
            if seen.last() != Some(&pending) {
                seen.push(pending);
            }
            check(&store);
        }
        assert_eq!(
            seen,
            vec![BTreeSet::from([0, 1]), BTreeSet::from([1]), BTreeSet::new()],
            "the chunks pending after each step"
        );
        assert_eq!(holders(&mut store), before);
    }

    /// The posting of `nothing` is kept exact by the write itself, never
    /// replayed nor folded (doc "Mass writes in the store"): a set or an unset
    /// knows what it does to it — its key is known in advance —, and an add
    /// over records holding `nothing` is a set (`Writer::add_over_nothing`).
    #[test]
    fn the_holders_of_nothing_are_exact_at_the_write_and_through_the_fold() {
        use crate::store::Rows;
        let (mut store, _dir) = test_store();
        let mut w = Writer::begin(&mut store, None).unwrap();
        let uuids: Vec<Uuid> = (0..6)
            .map(|_| w.create_metarecord(vec![Field::new("tag", Value::Nothing)]).unwrap().uuid)
            .collect();
        w.commit().unwrap();
        let ids: Vec<u32> =
            uuids.iter().map(|u| store.dense_ids(&[*u]).unwrap().min().unwrap()).collect();
        let of = |at: &[usize]| at.iter().map(|&i| ids[i]).collect::<RoaringBitmap>();
        let nothing = |store: &KvStore| store.value_holder_ids("tag", &Value::Nothing).unwrap();

        // An add over `nothing`: those records leave it at once.
        let mut w = Writer::begin(&mut store, None).unwrap();
        w.mass_add("tag", s("x"), &uuids[0..4]).unwrap();
        w.commit().unwrap();
        assert_eq!(nothing(&store), of(&[4, 5]), "after the add");
        // A set to `nothing`, pending for the other values: they join it at once.
        let mut w = Writer::begin(&mut store, None).unwrap();
        w.mass_set("tag", vec![Value::Nothing], &uuids[0..2]).unwrap();
        w.commit().unwrap();
        assert!(store.pending_index_writes("tag").unwrap() > 0, "the set waits for the fold");
        assert_eq!(nothing(&store), of(&[0, 1, 4, 5]), "after the set to nothing");
        // An unset: those records hold nothing at all.
        let mut w = Writer::begin(&mut store, None).unwrap();
        w.mass_unset("tag", &uuids[1..3]).unwrap();
        w.commit().unwrap();
        assert_eq!(nothing(&store), of(&[0, 4, 5]), "after the unset");
        check(&store);

        store.fold().unwrap();
        assert_eq!(store.pending_index_writes("tag").unwrap(), 0);
        assert_eq!(nothing(&store), of(&[0, 4, 5]), "after the fold");
        assert_eq!(store.rows(uuids[0]).unwrap().len(), 1);
        assert!(store.rows(uuids[1]).unwrap().is_empty(), "unset");
        check(&store);
    }
}
