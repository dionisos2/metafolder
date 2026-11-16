//! The pending events of a field's values (doc "Mass writes in the store").
//!
//! A write to many records at once does not file each record under the value
//! it gains, nor take each out of the values it loses — which it would have
//! to read: it appends one *event* to the field's list, naming the operation
//! and the records. A value's holders are then what is stored — its run of
//! the value partition or its posting, and for a reference its referrers —
//! with the field's events replayed over it, in order:
//!
//! | event                    | a value's holders become                    |
//! |--------------------------|---------------------------------------------|
//! | add `v` to `B`           | `∪ B` for `v`                               |
//! | remove `v` from `B`      | `∖ B` for `v`                               |
//! | replace by `vals` on `B` | `∖ B`, then `∪ B` for the values of `vals`  |
//!
//! A replacement never names the values it removes. While a field has events
//! pending, a write to one record of it is an event too — of one id — or the
//! replay would put an older event over it. A *fold* writes the events into
//! what is stored and empties the list.
//!
//! | table  | key                 | value                               |
//! |--------|---------------------|-------------------------------------|
//! | events | field · number      | kind · values · a bitmap of ids     |

use std::collections::BTreeSet;

use anyhow::{bail, ensure, Context, Result};
use heed::RoTxn;
use metafolder_core::metarecord::Value;
use roaring::RoaringBitmap;
use uuid::Uuid;

use super::super::{get_value, name_key, put_value, In, KvStore, KvTxn, Out, Tables};
use super::{
    count_prefix, decode_set, hist_bucket, hist_key, long_prefix, posting_prefix, referrers_prefix,
    search_text, set_prefix, unesc, value_key, ABSENT, COUNT_BITS, GRAM_PENDING, LONG_TEXTS,
    POSTINGS, REFERRERS, SETS, TRIGRAM_MAX,
};

pub(crate) use crate::store::MassKind as Kind;

/// One pending event of a field.
#[derive(Clone)]
pub(crate) struct Event {
    pub(crate) kind: Kind,
    pub(crate) values: Vec<Value>,
    /// The value keys of `values`, those of a `Nothing` left out.
    pub(crate) keys: Vec<Vec<u8>>,
    pub(crate) ids: RoaringBitmap,
}

impl Event {
    pub(crate) fn new(kind: Kind, values: Vec<Value>, ids: RoaringBitmap) -> Event {
        let keys = values.iter().filter_map(value_key).collect();
        Event { kind, values, keys, ids }
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

    fn decode(bytes: &[u8]) -> Result<Event> {
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
        Ok(Event::new(kind, values, ids))
    }
}

/// `field`'s pending events, oldest first.
pub(crate) fn read_events(t: &Tables, r: &RoTxn<'_>, field: &str) -> Result<Vec<Event>> {
    let mut out = Vec::new();
    for entry in t.events.prefix_iter(r, &name_key(field))? {
        out.push(Event::decode(entry?.1)?);
    }
    Ok(out)
}

/// The holders of the value of key `key`, `stored` with `events` replayed.
pub(crate) fn replay(events: &[Event], key: &[u8], mut holders: RoaringBitmap) -> RoaringBitmap {
    for e in events {
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
/// once `events` are replayed.
pub(crate) fn replay_one(events: &[Event], key: &[u8], id: u32, mut held: bool) -> bool {
    for e in events.iter().filter(|e| e.ids.contains(id)) {
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

/// The keys `events` name: the only values whose holders an event may add to.
/// Any other value only loses the records a replacement was written on
/// ([`replaced`]).
pub(crate) fn named(events: &[Event]) -> BTreeSet<Vec<u8>> {
    events.iter().flat_map(|e| e.keys.iter().cloned()).collect()
}

/// The records `events` replaced the field of: what a value no event names
/// has lost.
pub(crate) fn replaced(events: &[Event]) -> RoaringBitmap {
    let mut out = RoaringBitmap::new();
    for e in events.iter().filter(|e| e.kind == Kind::Replace) {
        out |= &e.ids;
    }
    out
}

/// `events` as they read on a field's referrers: keyed by the dense id of
/// the metarecord a `ref` value points at, as the referrers are.
pub(crate) fn by_target(
    events: &[Event],
    mut id_of: impl FnMut(&[u8; 16]) -> Result<Option<u32>>,
) -> Result<Vec<Event>> {
    let mut out = Vec::with_capacity(events.len());
    for e in events {
        let mut keys = Vec::new();
        for v in &e.values {
            if let Value::Ref(u) = v {
                if let Some(id) = id_of(u.as_bytes())? {
                    keys.push(id.to_be_bytes().to_vec());
                }
            }
        }
        out.push(Event { kind: e.kind, values: e.values.clone(), keys, ids: e.ids.clone() });
    }
    Ok(out)
}

impl KvStore {
    /// How many events `field` has pending.
    pub fn pending_events(&self, field: &str) -> Result<usize> {
        let r = self.env.read_txn()?;
        Ok(read_events(&self.t, &r, field)?.len())
    }
}

impl KvTxn<'_> {
    /// How many events `field` has pending, as the transaction sees it.
    pub(in super::super) fn eventful(&self, field: &str) -> Result<usize> {
        if let Some(n) = self.events.borrow().get(field) {
            return Ok(*n);
        }
        let prefix = name_key(field);
        let n = self.t.events.prefix_iter(&self.txn.borrow(), &prefix)?.count();
        self.events.borrow_mut().insert(field.to_string(), n);
        Ok(n)
    }

    /// Whether a write of `values` to many records of `field` can be one
    /// event: not on a forest, whose positions are written one by one (doc
    /// "One position per forest"); and, for a value added or removed, a
    /// value whose key holds it whole.
    pub(in super::super) fn mass_eligible(
        &self,
        field: &str,
        kind: Kind,
        values: &[Value],
    ) -> Result<bool> {
        for entry in self.t.field_types.prefix_iter(&self.txn.borrow(), &name_key(field))? {
            let (_, ty) = unesc(entry?.0)?;
            if matches!(ty, b"tree_ref" | b"hierarchy_ref") {
                return Ok(false);
            }
        }
        let plain = |v: &Value| match value_key(v) {
            None => kind == Kind::Replace,
            Some(key) => {
                v.position().is_none()
                    && !(key[0] == 2 && long_prefix(&key[1..]).is_some())
                    && search_text(v).is_none_or(|t| t.len() <= TRIGRAM_MAX)
            }
        };
        Ok(values.iter().all(plain) && (kind == Kind::Replace || values.len() == 1))
    }

    /// From here to [`Self::mass_end`], the rows written to `field` derive
    /// nothing: the event does, for all of them.
    pub(in super::super) fn mass_begin(&self, field: &str) {
        *self.mass.borrow_mut() = Some(field.to_string());
    }

    /// The rows of a write to many records of `field` are written: what they
    /// derive, as one event on `ids` and a few set operations.
    pub(in super::super) fn mass_end(
        &self,
        field: &str,
        kind: Kind,
        values: Vec<Value>,
        uuids: &[Uuid],
    ) -> Result<()> {
        *self.mass.borrow_mut() = None;
        let mut ids = RoaringBitmap::new();
        for u in uuids {
            ids.insert(self.id(u.as_bytes())?.with_context(|| format!("no dense id for {u}"))?);
        }
        self.mass_end_ids(field, kind, values, ids)
    }

    /// [`Self::mass_end`] for the ids of the records.
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
        let event = Event::new(kind, values, ids);
        match kind {
            Kind::Add | Kind::Remove => {
                let (value, key) = (&event.values[0], &event.keys[0]);
                let have = self.effective(field, value, key)?;
                if kind == Kind::Add {
                    let gain = &event.ids - &have;
                    self.count_step(field, &gain, true)?;
                    self.hist_bump(field, key, gain.len() as i64)?;
                    self.mass_grams(field, value, &gain)?;
                } else {
                    let lose = &event.ids & &have;
                    self.count_step(field, &lose, false)?;
                    self.hist_bump(field, key, -(lose.len() as i64))?;
                }
            }
            Kind::Replace => {
                let n = event.keys.len() as u64;
                for bit in 0..COUNT_BITS {
                    let prefix = count_prefix(field, bit);
                    self.chunk_apply(SETS, &prefix, &event.ids, n >> bit & 1 == 1)?;
                }
                let unknown = event.values.iter().any(|v| matches!(v, Value::Nothing));
                self.chunk_apply(SETS, &set_prefix(ABSENT, Some(field)), &event.ids, unknown)?;
                self.chunk_apply(SETS, &set_prefix(LONG_TEXTS, Some(field)), &event.ids, false)?;
                for (value, key) in
                    event.values.iter().filter(|v| value_key(v).is_some()).zip(&event.keys)
                {
                    // The rows the replacement removed leave their buckets
                    // at the fold, which is what knows them.
                    self.hist_bump(field, key, event.ids.len() as i64)?;
                    self.mass_grams(field, value, &event.ids)?;
                }
            }
        }
        self.append_event(field, &event)?;
        // The rows the write did not put into the records.
        self.append_entries(kind, field, &listed, &event.ids)
    }

    /// A row of one record came or went in a field that has events pending:
    /// an event of one id, in its place after them.
    pub(in super::super) fn single_event(
        &self,
        field: &str,
        value: &Value,
        id: u32,
        add: bool,
    ) -> Result<()> {
        let kind = if add { Kind::Add } else { Kind::Remove };
        let event = Event::new(kind, vec![value.clone()], RoaringBitmap::from_iter([id]));
        self.append_event(field, &event)
    }

    fn append_event(&self, field: &str, event: &Event) -> Result<()> {
        let pending = self.eventful(field)?;
        let seq = self.next("event_seq")?;
        let key = [&name_key(field)[..], &seq.to_be_bytes()].concat();
        self.t.events.put(&mut self.txn.borrow_mut(), &key, &event.encode())?;
        self.events.borrow_mut().insert(field.to_string(), pending + 1);
        Ok(())
    }

    /// The trigrams of a text `ids` gained in `field` (doc "Text search in
    /// the store"): by value, those of a text new to the field; by
    /// metarecord, the ids pending.
    fn mass_grams(&self, field: &str, value: &Value, ids: &RoaringBitmap) -> Result<()> {
        let Some(text) = search_text(value) else { return Ok(()) };
        let Some(first) = ids.min() else { return Ok(()) };
        if self.gram_state(field)?.by_ids {
            return self.chunk_apply(SETS, &set_prefix(GRAM_PENDING, Some(field)), ids, true);
        }
        let new_text = self.new_text(field, value)?;
        self.derive_grams(field, first, &text, new_text.as_deref())?;
        // The field may have turned by metarecord on that text.
        if self.gram_state(field)?.by_ids {
            self.chunk_apply(SETS, &set_prefix(GRAM_PENDING, Some(field)), ids, true)?;
        }
        Ok(())
    }

    fn hist_bump(&self, field: &str, key: &[u8], delta: i64) -> Result<()> {
        match hist_bucket(key) {
            Some(bucket) if delta != 0 => self.bump(self.t.hist, &hist_key(field, &bucket), delta),
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
            let set = &self.read_chunked(SETS, &prefix)? & &carry;
            let clear = &carry - &set;
            // Up: a set bit clears and carries on, a clear one sets and
            // stops. Down: a clear bit sets and borrows on, a set one clears
            // and stops.
            self.chunk_apply(SETS, &prefix, &set, false)?;
            self.chunk_apply(SETS, &prefix, &clear, true)?;
            carry = if up { set } else { clear };
        }
        ensure!(carry.is_empty(), "a count of values of '{field}' out of its bits");
        Ok(())
    }

    /// The holders of a value as stored: its posting.
    fn stored_holders(&self, field: &str, key: &[u8]) -> Result<RoaringBitmap> {
        self.read_chunked(SETS, &super::posting_prefix(field, key))
    }

    /// The holders of a value as the transaction sees them: stored, the
    /// field's events replayed.
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
        let events = read_events(&self.t, &self.txn.borrow(), field)?;
        Ok(replay(&events, key, stored))
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
        self.chunk_apply(SETS, &posting_prefix(field, key), ids, add)?;
        if let Some(Value::Ref(target)) = value {
            let tid = self.target_id(target.as_bytes())?;
            self.chunk_apply(SETS, &referrers_prefix(field, tid), ids, add)?;
        }
        Ok(())
    }

    /// Writes `field`'s pending events into what is stored, oldest first, and
    /// empties its list. A replacement is where the values it removed are
    /// found: the field's stored values are walked for the records it was
    /// written on.
    pub(in super::super) fn fold_events(&self, field: &str) -> Result<()> {
        let t = self.t;
        let events = read_events(&t, &self.txn.borrow(), field)?;
        if events.is_empty() {
            return Ok(());
        }
        for e in &events {
            let valued = e.values.iter().filter(|v| value_key(v).is_some());
            match e.kind {
                Kind::Add | Kind::Remove => {
                    for (value, key) in valued.zip(&e.keys) {
                        let have = self.stored_holders(field, key)?;
                        let add = e.kind == Kind::Add;
                        let ids = if add { &e.ids - &have } else { &e.ids & &have };
                        self.store_holders(field, Some(value), key, &ids, add)?;
                    }
                }
                Kind::Replace => {
                    for (key, holders) in self.postings_of(field)? {
                        let gone = &holders & &e.ids;
                        if gone.is_empty() {
                            continue;
                        }
                        self.store_holders(field, None, &key, &gone, false)?;
                        self.hist_bump(field, &key, -(gone.len() as i64))?;
                    }
                    // Whatever the records pointed at, they no longer do.
                    let referrers = [&[REFERRERS][..], &name_key(field)].concat();
                    let mut targets: Vec<Vec<u8>> = Vec::new();
                    for entry in t.sets.prefix_iter(&self.txn.borrow(), &referrers)? {
                        let k = entry?.0;
                        if k.len() == referrers.len() + 6 {
                            targets.push(k[..k.len() - 2].to_vec());
                        }
                    }
                    targets.dedup();
                    for target in targets {
                        self.chunk_apply(SETS, &target, &e.ids, false)?;
                    }
                    for (value, key) in valued.zip(&e.keys) {
                        let have = self.stored_holders(field, key)?;
                        self.store_holders(field, Some(value), key, &(&e.ids - &have), true)?;
                    }
                }
            }
        }
        let prefix = name_key(field);
        let mut w = self.txn.borrow_mut();
        let mut keys = Vec::new();
        for entry in t.events.prefix_iter(&w, &prefix)? {
            keys.push(entry?.0.to_vec());
        }
        for k in keys {
            t.events.delete(&mut w, &k)?;
        }
        drop(w);
        self.events.borrow_mut().insert(field.to_string(), 0);
        Ok(())
    }

    /// Every value of `field` that has a holder as stored, the transaction's
    /// own writes included, with its holders: its postings, whole.
    pub(in super::super) fn postings_of(
        &self,
        field: &str,
    ) -> Result<Vec<(Vec<u8>, RoaringBitmap)>> {
        let posted = [&[POSTINGS][..], &name_key(field)].concat();
        let mut keys = BTreeSet::new();
        {
            let txn = self.txn.borrow();
            for entry in self.t.sets.prefix_iter(&txn, &posted)? {
                let k = entry?.0;
                keys.insert(k[posted.len()..k.len() - 2].to_vec());
            }
        }
        // The postings this transaction made are not in the table yet.
        let whole = [&[SETS][..], &posted[..]].concat();
        for k in self.sets.borrow().keys() {
            if k.starts_with(&whole) && k.len() >= whole.len() + 2 {
                keys.insert(k[whole.len()..k.len() - 2].to_vec());
            }
        }
        let mut out = Vec::new();
        for key in keys {
            let holders = self.read_chunked(SETS, &posting_prefix(field, &key))?;
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
    /// It reads the field's values, not its records: the postings, the events
    /// pending — and one record for each
    /// distinct value found, to give back the value itself (a key is not
    /// always its value: an int and a float of one number share it).
    pub(in super::super) fn values_held(
        &self,
        field: &str,
        ids: &RoaringBitmap,
    ) -> Result<Vec<(Value, RoaringBitmap)>> {
        let events = read_events(&self.t, &self.txn.borrow(), field)?;
        let mut stored: std::collections::BTreeMap<Vec<u8>, RoaringBitmap> = Default::default();
        for key in named(&events) {
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
            let holders = &replay(&events, &key, held) & ids;
            let Some(first) = holders.min() else { continue };
            let rows = self.load_record(first)?.plain_rows();
            let value = rows
                .into_iter()
                .find(|r| r.name == field && value_key(&r.value).as_deref() == Some(&key[..]))
                .with_context(|| format!("a row of '{field}' that its holder lacks"))?
                .value;
            out.push((value, places(&holders)));
        }
        let unknown = &self.read_chunked(SETS, &set_prefix(ABSENT, Some(field)))? & ids;
        if !unknown.is_empty() {
            out.push((Value::Nothing, places(&unknown)));
        }
        Ok(out)
    }

    /// [`Self::fold_events`] for every field with events pending.
    pub(in super::super) fn fold_all_events(&self) -> Result<()> {
        let mut fields = BTreeSet::new();
        for entry in self.t.events.iter(&self.txn.borrow())? {
            let (name, _) = unesc(entry?.0)?;
            fields.insert(String::from_utf8(name).context("a field name")?);
        }
        for field in fields {
            self.fold_events(&field)?;
        }
        Ok(())
    }

    /// Folds the biggest list pending (see `KvStore::fold_step`): size is the
    /// bytes of a part's entries or of a field's events, the ids pending of a
    /// field's trigrams. Returns whether any list was.
    pub(in super::super) fn fold_biggest(&self) -> Result<bool> {
        // A deletion first, whatever the size of the others: its records are
        // read with the entries of their parts still applied.
        if self.fold_one_delete()? {
            return Ok(true);
        }
        enum List {
            Part(u32),
            Events(String),
            Grams(String),
        }
        let mut best: Option<(u64, List)> = None;
        let mut consider = |size: u64, list: List| {
            if size > 0 && best.as_ref().is_none_or(|(b, _)| size > *b) {
                best = Some((size, list));
            }
        };
        let mut parts: std::collections::BTreeMap<u32, u64> = Default::default();
        for entry in self.t.mass.iter(&self.txn.borrow())? {
            let (k, v) = entry?;
            let part = u32::from_be_bytes(k[..4].try_into().expect("a part"));
            // Records given back wait for a write, not for a fold.
            if part != crate::kvstore::mass::THAWED_PART {
                *parts.entry(part).or_default() += (k.len() + v.len()) as u64;
            }
        }
        for (part, size) in parts {
            consider(size, List::Part(part));
        }
        let mut fields: std::collections::BTreeMap<String, u64> = Default::default();
        for entry in self.t.events.iter(&self.txn.borrow())? {
            let (k, v) = entry?;
            let (name, _) = unesc(k)?;
            let name = String::from_utf8(name).context("a field name")?;
            *fields.entry(name).or_default() += (k.len() + v.len()) as u64;
        }
        for (field, size) in fields {
            consider(size, List::Events(field));
        }
        for field in self.gram_fields()? {
            consider(self.gram_fold_size(&field)?, List::Grams(field));
        }
        match best {
            None => Ok(false),
            Some((_, List::Part(part))) => self.fold_part(part).map(|()| true),
            Some((_, List::Events(field))) => self.fold_events(&field).map(|()| true),
            Some((_, List::Grams(field))) => self.fold_gram_field(&field).map(|()| true),
        }
    }
}
