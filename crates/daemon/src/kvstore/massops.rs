//! The operations of a write to many records, one entry in the log (doc
//! "Mass operations in the log").
//!
//! A write of one value to, or off, a field of N records is logged as one
//! *stub*: the field, the value, and the records that changed — their dense ids
//! as the write named them, their uuids once a reindex numbers the ids again. It holds the N operation ids that follow its key — `first`, which
//! is its key in `ops`, to `first + N - 1` — and what reads the log is given
//! the operations a write to each record alone would have logged: the
//! record `k` places from the start holds operation `first + k`, an
//! `append_field` with the row after it, or a `delete_field` with the row
//! before it. Nothing of them is stored.
//!
//! | table         | key                | value                       |
//! |---------------|--------------------|-----------------------------|
//! | ops           | first id           | an operation's header, then the row and the uuids |
//! | ops_by_label  | label · first id   | the last id (an operation's is empty) |
//! | ops_by_entity | nil uuid · first id| —                           |
//! | user_ops, watcher_ops | first id   | — (one entry in the retention count) |
//!
//! The stub is told from an operation by its type, `mass_add` or
//! `mass_remove`; the header is an operation's own, so what reads an
//! operation's timestamp reads a stub's. The nil uuid, which no metarecord
//! has, lists the stubs in `ops_by_entity`: a record's operations after an id
//! are its own and those of the stubs that name it. The `mass_extra` counter
//! of `meta` is the operations the stubs hold beyond one each.

use std::collections::{BTreeMap, HashSet};

use anyhow::{ensure, Context, Result};
use metafolder_core::metarecord::Value;
use roaring::RoaringBitmap;
use uuid::Uuid;

use super::derived::{count_prefix, COUNT_BITS, SETS};
use super::{
    be, dec_op, dec_row, enc_row, from_be, key, name_key, uuid_of, In, KvTxn, Out, Read, ROOT,
};
use crate::log::OpRow;
use crate::rows::FieldRow;
use crate::store::{MassApplied, MassKind as Kind};

const ADD: &str = "mass_add";
const REMOVE: &str = "mass_remove";
const SET: &str = "mass_set";
const UNSET: &str = "mass_unset";

/// The records of a stub.
#[derive(Clone)]
pub(super) enum Who {
    /// Their dense ids, as a write names them: nothing was read to write
    /// them. A reindex, which numbers the ids again, turns them into uuids.
    Ids(RoaringBitmap),
    /// Their uuids, in the order their ids had.
    Uuids(Vec<Uuid>),
}

impl Who {
    fn len(&self) -> usize {
        match self {
            Who::Ids(ids) => ids.len() as usize,
            Who::Uuids(uuids) => uuids.len(),
        }
    }
}

/// A stub, decoded.
pub(super) struct Stub {
    pub(super) first: i64,
    pub(super) label: i64,
    pub(super) timestamp: i64,
    pub(super) origin: Option<String>,
    pub(super) kind: Kind,
    pub(super) field: String,
    /// What was added or removed (one value); what replaced the rows (none
    /// for a field taken off).
    pub(super) values: Vec<Value>,
    /// For a replacement: what it took off, each value with the places —
    /// among the stub's records — of those that held it. A replacement
    /// forgets what it replaces, so the log is told.
    pub(super) before: Vec<(Value, RoaringBitmap)>,
    /// The record of operation `first + k` is the `k`-th of them, in the
    /// order of their ids or their uuids.
    pub(super) who: Who,
}

impl Stub {
    pub(super) fn last(&self) -> i64 {
        self.first + self.who.len() as i64 - 1
    }

    pub(super) fn covers(&self, id: i64) -> bool {
        (self.first..=self.last()).contains(&id)
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Out::default();
        out.int(self.label)
            .int(self.timestamp)
            .opt_bytes(self.origin.as_deref().map(str::as_bytes))
            .bytes(self.type_name().as_bytes())
            .bytes(ROOT.as_slice())
            .opt_int(None)
            .opt_int(None)
            .opt_bytes(Some(self.field.as_bytes()))
            .opt_int(None)
            .int(self.values.len() as i64);
        for row in self.rows() {
            out.bytes(&enc_row(&row));
        }
        out.int(self.before.len() as i64);
        for (value, places) in &self.before {
            out.bytes(&enc_row(&FieldRow { name: self.field.clone(), value: value.clone() }));
            let mut bytes = Vec::new();
            places.serialize_into(&mut bytes).expect("a bitmap into a vector");
            out.bytes(&bytes);
        }
        match &self.who {
            Who::Uuids(uuids) => {
                out.0.push(0);
                for u in uuids {
                    out.0.extend_from_slice(u.as_bytes());
                }
            }
            Who::Ids(ids) => {
                out.0.push(1);
                ids.serialize_into(&mut out.0).expect("a bitmap into a vector");
            }
        }
        out.0
    }

    fn type_name(&self) -> &'static str {
        match (self.kind, self.values.is_empty()) {
            (Kind::Add, _) => ADD,
            (Kind::Remove, _) => REMOVE,
            (Kind::Replace, false) => SET,
            (Kind::Replace, true) => UNSET,
        }
    }

    /// The values as rows of the field.
    fn rows(&self) -> Vec<FieldRow> {
        self.values
            .iter()
            .map(|v| FieldRow { name: self.field.clone(), value: v.clone() })
            .collect()
    }

    /// The stub a value holds, `None` when it is an ordinary operation's.
    pub(super) fn decode(first: i64, b: &[u8]) -> Result<Option<Self>> {
        let head = dec_op(first, b)?;
        let kind = match head.op_type.as_str() {
            ADD => Kind::Add,
            REMOVE => Kind::Remove,
            SET | UNSET => Kind::Replace,
            _ => return Ok(None),
        };
        // Past the header: what `dec_op` read.
        let mut r = In(b);
        r.int()?;
        r.int()?;
        r.opt_bytes()?;
        r.bytes()?;
        r.bytes()?;
        r.opt_int()?;
        r.opt_int()?;
        r.opt_bytes()?;
        r.opt_int()?;
        let mut values = Vec::new();
        for _ in 0..r.int()? {
            values.push(dec_row(r.bytes()?)?.value);
        }
        let mut before = Vec::new();
        for _ in 0..r.int()? {
            let value = dec_row(r.bytes()?)?.value;
            before.push((value, RoaringBitmap::deserialize_from(r.bytes()?)?));
        }
        let (tag, rest) = r.0.split_first().context("a stub with no records")?;
        let who = match tag {
            0 => Who::Uuids(rest.chunks_exact(16).map(uuid_of).collect()),
            _ => Who::Ids(RoaringBitmap::deserialize_from(rest)?),
        };
        Ok(Some(Stub {
            first,
            label: head.label_id,
            timestamp: head.timestamp,
            origin: head.origin,
            kind,
            field: head.field_name.context("a stub with no field")?,
            values,
            before,
            who,
        }))
    }

    /// The record of the stub's `k`-th operation.
    pub(super) fn entity(&self, read: &Read, k: usize) -> Result<Uuid> {
        match &self.who {
            Who::Uuids(uuids) => Ok(uuids[k]),
            Who::Ids(ids) => {
                let id = ids.select(k as u32).context("a stub shorter than its ids")?;
                read.uuid_of_id(id)
            }
        }
    }

    /// The operation `id` is, one of the stub's.
    pub(super) fn op(&self, read: &Read, id: i64) -> Result<OpRow> {
        Ok(OpRow {
            id,
            label_id: self.label,
            timestamp: self.timestamp,
            op_type: match (self.kind, self.values.is_empty()) {
                (Kind::Add, _) => "append_field",
                (Kind::Replace, false) => "set_field",
                _ => "delete_field",
            }
            .into(),
            entity_uuid: self.entity(read, (id - self.first) as usize)?,
            field_name: Some(self.field.clone()),
            reverts_op_id: None,
            origin: self.origin.clone(),
        })
    }

    /// The rows operation `id` of the stub has after it, or before.
    pub(super) fn snapshot(&self, id: i64, after: bool) -> Vec<FieldRow> {
        let row = |value: &Value| FieldRow { name: self.field.clone(), value: value.clone() };
        match self.kind {
            Kind::Add | Kind::Remove if after == (self.kind == Kind::Add) => self.rows(),
            Kind::Add | Kind::Remove => Vec::new(),
            Kind::Replace if after => self.rows(),
            Kind::Replace => {
                let k = (id - self.first) as u32;
                self.before.iter().filter(|(_, at)| at.contains(k)).map(|(v, _)| row(v)).collect()
            }
        }
    }

    /// The place of a record among the stub's.
    pub(super) fn position(&self, read: &Read, uuid: Uuid) -> Result<Option<usize>> {
        Ok(match &self.who {
            Who::Uuids(uuids) => uuids.iter().position(|u| *u == uuid),
            Who::Ids(ids) => {
                read.count(1)?;
                match super::derived::id_of(read.t, read.r, uuid.as_bytes())? {
                    Some(id) if ids.contains(id) => Some(ids.rank(id) as usize - 1),
                    _ => None,
                }
            }
        })
    }
}

impl Read<'_> {
    /// The stub holding operation `id`, when it is one of a stub's.
    pub(super) fn stub_at(&self, id: i64) -> Result<Option<Stub>> {
        self.count(1)?;
        if let Some(v) = self.t.ops.get(self.r, &be(id))? {
            return Stub::decode(id, v);
        }
        let end = be(id);
        let range = (std::ops::Bound::Unbounded, std::ops::Bound::Included(&end[..]));
        self.count(1)?;
        match self.t.ops.rev_range(self.r, &range)?.next() {
            Some(e) => {
                let (k, v) = e?;
                Ok(Stub::decode(from_be(k), v)?.filter(|s| s.covers(id)))
            }
            None => Ok(None),
        }
    }

    /// The operations of `ids` (ids of the log), in the order given; the
    /// stub read last serves the ids that follow it.
    pub(super) fn ops_resolved(&self, ids: impl Iterator<Item = i64>) -> Result<Vec<OpRow>> {
        let mut out = Vec::new();
        let mut stub: Option<Stub> = None;
        for id in ids {
            if let Some(s) = stub.as_ref().filter(|s| s.covers(id)) {
                out.push(s.op(self, id)?);
                continue;
            }
            self.count(1)?;
            let b = self.t.ops.get(self.r, &be(id))?.map(<[u8]>::to_vec);
            match b {
                Some(b) => match Stub::decode(id, &b)? {
                    Some(s) => {
                        out.push(s.op(self, id)?);
                        stub = Some(s);
                    }
                    None => out.push(dec_op(id, &b)?),
                },
                None => {
                    let s =
                        self.stub_at(id)?.with_context(|| format!("operation {id} vanished"))?;
                    out.push(s.op(self, id)?);
                    stub = Some(s);
                }
            }
        }
        Ok(out)
    }

    /// The ids of the operations from `from` on, at most `cap` of them.
    pub(super) fn ids_from(&self, from: i64, cap: usize) -> Result<Vec<i64>> {
        let mut out = Vec::new();
        let mut start = from;
        // A stub that began before `from` still holds ids from it on.
        if let Some(s) = self.stub_at(from)? {
            out.extend((from..=s.last()).take(cap));
            start = s.last() + 1;
        }
        let from_key = be(start);
        let range = (std::ops::Bound::Included(&from_key[..]), std::ops::Bound::Unbounded);
        let mut read = 0;
        for e in self.t.ops.range(self.r, &range)? {
            if out.len() >= cap {
                break;
            }
            let (k, v) = e?;
            read += 1;
            let id = from_be(k);
            match Stub::decode(id, v)? {
                Some(s) => out.extend((id..=s.last()).take(cap - out.len())),
                None => out.push(id),
            }
        }
        self.count(read)?;
        Ok(out)
    }

    /// An entity's operations after `after` that are a stub's, ids ascending.
    pub(super) fn stub_ops_of(&self, entity: Uuid, after: i64) -> Result<Vec<i64>> {
        let mut out = Vec::new();
        let (lo, hi) = (key(&[ROOT.as_slice(), &be(0)]), key(&[ROOT.as_slice(), &be(i64::MAX)]));
        let range = (std::ops::Bound::Included(&lo[..]), std::ops::Bound::Included(&hi[..]));
        for e in self.t.ops_by_entity.rev_range(self.r, &range)? {
            let (k, _) = e?;
            let first = from_be(&k[16..]);
            self.count(2)?;
            let stub = self
                .t
                .ops
                .get(self.r, &be(first))?
                .map(|v| Stub::decode(first, v))
                .transpose()?
                .flatten()
                .context("a stub listed and not there")?;
            if stub.last() <= after {
                break;
            }
            if let Some(k) = stub.position(self, entity)? {
                let id = first + k as i64;
                if id > after {
                    out.push(id);
                }
            }
        }
        out.reverse();
        Ok(out)
    }

    /// The rows operation `op_id` has after it, or before, a stub's included.
    pub(super) fn snapshot_rows(&self, op_id: i64, after: bool) -> Result<Vec<FieldRow>> {
        if let Some(stub) = self.stub_at(op_id)? {
            return Ok(stub.snapshot(op_id, after));
        }
        let mut out = Vec::new();
        let prefix = key(&[&be(op_id), &[after as u8]]);
        for e in self.t.snaps.prefix_iter(self.r, &prefix)? {
            out.push(super::dec_row(e?.1)?);
        }
        self.count(out.len() as u64 + 1)?;
        Ok(out)
    }

    /// A record's operations after `after`, ascending: its own, and those of
    /// the stubs that name it.
    pub(super) fn entity_op_ids_after(&self, entity: Uuid, after: i64) -> Result<Vec<i64>> {
        let mut ids = Vec::new();
        for e in self.t.ops_by_entity.prefix_iter(self.r, entity.as_bytes())? {
            self.count(1)?;
            let id = from_be(&e?.0[16..]);
            if id > after {
                ids.push(id);
            }
        }
        ids.extend(self.stub_ops_of(entity, after)?);
        ids.sort_unstable();
        Ok(ids)
    }

    /// How many operations the stubs hold beyond one entry each.
    pub(super) fn mass_extra(&self) -> Result<i64> {
        self.count(1)?;
        Ok(self.meta("mass_extra")?.unwrap_or(0))
    }
}

impl KvTxn<'_> {
    /// The dense ids of `uuids`, for a write that names records by uuid. A
    /// uuid that is no metarecord is refused when `must_exist`, left out
    /// otherwise.
    pub(super) fn dense_ids_of(&self, uuids: &[Uuid], must_exist: bool) -> Result<RoaringBitmap> {
        let mut ids = RoaringBitmap::new();
        for &u in uuids {
            if !crate::store::Rows::exists(self, u)? {
                if must_exist {
                    return Err(crate::error::DomainError::NotFound(format!(
                        "Metarecord not found: {u}"
                    ))
                    .into());
                }
                continue;
            }
            ids.insert(
                self.dense_id(u.as_bytes())?.with_context(|| format!("no dense id for {u}"))?,
            );
        }
        Ok(ids)
    }

    /// The uuids of dense ids, in the order of the ids.
    pub(super) fn uuids_of_ids(&self, ids: &RoaringBitmap) -> Result<Vec<Uuid>> {
        let txn = self.txn.borrow();
        self.read_in(&txn, &mut |read: &Read| ids.iter().map(|id| read.uuid_of_id(id)).collect())
    }

    /// Logs a write to `field` of the records `ids` (live ones) as one stub,
    /// and derives what it changes (doc "Mass operations in the log"): one
    /// value added to those that lack it or removed from those that hold it,
    /// or the rows replaced by `values` (none: the field taken off). No record
    /// is read, and nothing is read per id: which of them change is a question
    /// to the holders of the value, and how many rows go a sum over the
    /// counters of the field.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn mass_log(
        &self,
        kind: Kind,
        field: &str,
        values: &[Value],
        ids: &RoaringBitmap,
        label: i64,
        timestamp: i64,
        origin: Option<&str>,
    ) -> Result<Option<MassApplied>> {
        if !self.mass_eligible(field, kind, values)? {
            return Ok(None);
        }
        let mut before = Vec::new();
        let changed = match kind {
            Kind::Add | Kind::Remove => {
                let value = &values[0];
                let key = super::derived::value_key(value).context("a value with no key")?;
                let have = self.effective(field, value, &key)?;
                let changed = if kind == Kind::Add { ids - &have } else { ids & &have };
                let n = changed.len() as i64;
                self.count_type(field, value, if kind == Kind::Add { n } else { -n })?;
                changed
            }
            Kind::Replace => {
                // A field taken off changes what holds it.
                let changed = if values.is_empty() {
                    ids & &self.holder_ids_now(field)?
                } else {
                    ids.clone()
                };
                if !changed.is_empty() {
                    before = self.values_held(field, &changed)?;
                    self.count_rows_replaced(field, values, &changed)?;
                }
                changed
            }
        };
        if changed.is_empty() {
            return Ok(Some(MassApplied { changed, last_op: 0 }));
        }
        self.mass_end_ids(field, kind, values.to_vec(), changed.clone())?;
        let stub = Stub {
            first: self.next_op_id()?,
            label,
            timestamp,
            origin: origin.map(str::to_string),
            kind,
            field: field.to_string(),
            values: values.to_vec(),
            before,
            who: Who::Ids(changed.clone()),
        };
        let last = stub.last();
        self.put_stub(&stub)?;
        Ok(Some(MassApplied { changed, last_op: last }))
    }

    /// The type counts of `field` after its rows on `changed` are replaced by
    /// `values`: the rows that go are as many as the counters say, and of the
    /// one type the field has.
    fn count_rows_replaced(
        &self,
        field: &str,
        values: &[Value],
        changed: &RoaringBitmap,
    ) -> Result<()> {
        let mut gone = 0u64;
        for bit in 0..COUNT_BITS {
            let plane = self.read_chunked(SETS, &count_prefix(field, bit))?;
            gone += (&plane & changed).len() << bit;
        }
        if gone > 0 {
            let types: Vec<Vec<u8>> = {
                let txn = self.txn.borrow();
                let prefix = name_key(field);
                let mut out = Vec::new();
                for e in self.t.field_types.prefix_iter(&txn, &prefix)? {
                    out.push(e?.0[prefix.len()..].to_vec());
                }
                out
            };
            for ty in types {
                // One type a field has, `Nothing` aside; a few types only
                // while a retype is under way, which is not a mass write.
                self.count_type_named(field, &ty, -(gone as i64))?;
            }
        }
        let kept: Vec<&Value> = values.iter().filter(|v| !matches!(v, Value::Nothing)).collect();
        if let Some(first) = kept.first() {
            let delta = kept.len() as i64 * changed.len() as i64;
            self.count_type(field, first, delta)?;
        }
        Ok(())
    }

    /// The stubs that hold ids become stubs that hold uuids: a reindex is
    /// about to number the ids again. Dead records included, which the ids
    /// still name until then.
    pub(super) fn stubs_to_uuids(&self) -> Result<()> {
        let mut found: Vec<(i64, Stub)> = Vec::new();
        {
            let txn = self.txn.borrow();
            let r: &heed::RoTxn<'_> = &txn;
            for e in self.t.ops_by_entity.prefix_iter(r, ROOT.as_slice())? {
                let first = from_be(&e?.0[16..]);
                if let Some(v) = self.t.ops.get(r, &be(first))? {
                    if let Some(stub) = Stub::decode(first, v)? {
                        if matches!(stub.who, Who::Ids(_)) {
                            found.push((first, stub));
                        }
                    }
                }
            }
        }
        for (first, mut stub) in found {
            let Who::Ids(ids) = &stub.who else { continue };
            // In the order of the ids: an operation keeps its record.
            stub.who = Who::Uuids(self.uuids_of_ids(ids)?);
            let mut w = self.txn.borrow_mut();
            self.t.ops.put(&mut w, &be(first), &stub.encode())?;
        }
        Ok(())
    }

    /// The last id of the stub that `id` is the first of, `id` itself when it
    /// is an ordinary operation's.
    pub(super) fn entry_end(&self, id: i64) -> Result<i64> {
        let w = self.txn.borrow();
        let Some(v) = self.t.ops.get(&w, &be(id))? else { return Ok(id) };
        Ok(Stub::decode(id, v)?.map_or(id, |s| s.last()))
    }

    fn next_op_id(&self) -> Result<i64> {
        let w = self.txn.borrow();
        Ok(self.t.meta.get(&w, b"next_op")?.map_or(1, from_be))
    }

    fn put_stub(&self, stub: &Stub) -> Result<()> {
        let mut w = self.txn.borrow_mut();
        let t = self.t;
        let first = be(stub.first);
        t.ops.put(&mut w, &first, &stub.encode())?;
        t.ops_by_label.put(&mut w, &key(&[&be(stub.label), &first]), &be(stub.last()))?;
        t.ops_by_entity.put(&mut w, &key(&[ROOT.as_slice(), &first]), &[])?;
        self.class(stub.origin.as_deref()).put(&mut w, &first, &[])?;
        t.meta.put(&mut w, b"next_op", &be(stub.last() + 1))?;
        let extra = t.meta.get(&w, b"mass_extra")?.map_or(0, from_be);
        t.meta.put(&mut w, b"mass_extra", &be(extra + stub.who.len() as i64 - 1))?;
        Ok(())
    }

    /// Takes the first `k` operations out of a stub, all of them when it
    /// holds no more: pruning and retention take the oldest operations.
    fn cut_stub(&self, stub: &Stub, k: usize) -> Result<()> {
        let mut w = self.txn.borrow_mut();
        let t = self.t;
        let first = be(stub.first);
        t.ops.delete(&mut w, &first)?;
        t.ops_by_label.delete(&mut w, &key(&[&be(stub.label), &first]))?;
        t.ops_by_entity.delete(&mut w, &key(&[ROOT.as_slice(), &first]))?;
        t.watcher_ops.delete(&mut w, &first)?;
        t.user_ops.delete(&mut w, &first)?;
        let extra = t.meta.get(&w, b"mass_extra")?.map_or(0, from_be);
        let kept = stub.who.len() - k;
        if kept == 0 {
            t.meta.put(&mut w, b"mass_extra", &be(extra - (stub.who.len() as i64 - 1)))?;
            return Ok(());
        }
        t.meta.put(&mut w, b"mass_extra", &be(extra - k as i64))?;
        drop(w);
        let rest = Stub {
            first: stub.first + k as i64,
            label: stub.label,
            timestamp: stub.timestamp,
            origin: stub.origin.clone(),
            kind: stub.kind,
            field: stub.field.clone(),
            values: stub.values.clone(),
            before: stub
                .before
                .iter()
                .map(|(v, at)| {
                    (
                        v.clone(),
                        at.iter().filter(|p| *p as usize >= k).map(|p| p - k as u32).collect(),
                    )
                })
                .collect(),
            who: match &stub.who {
                Who::Uuids(uuids) => Who::Uuids(uuids[k..].to_vec()),
                Who::Ids(ids) => Who::Ids(ids.iter().skip(k).collect()),
            },
        };
        let mut w = self.txn.borrow_mut();
        let first = be(rest.first);
        t.ops.put(&mut w, &first, &rest.encode())?;
        t.ops_by_label.put(&mut w, &key(&[&be(rest.label), &first]), &be(rest.last()))?;
        t.ops_by_entity.put(&mut w, &key(&[ROOT.as_slice(), &first]), &[])?;
        self.class(rest.origin.as_deref()).put(&mut w, &first, &[])?;
        Ok(())
    }

    /// Removes the operations of `ids` that are a stub's: the oldest of a
    /// stub (an id that is not among its first is left, as the rest of the
    /// log keeps what a prune does not name). Every id of `ids` that is a
    /// stub's, and the labels of the stubs it touched.
    pub(super) fn delete_stub_ops(&self, ids: &[i64]) -> Result<(HashSet<i64>, HashSet<i64>)> {
        let mut handled = HashSet::new();
        let mut labels = HashSet::new();
        let mut per: BTreeMap<i64, (Stub, Vec<i64>)> = BTreeMap::new();
        {
            let txn = self.txn.borrow();
            self.read_in(&txn, &mut |read: &Read| {
                for &id in ids {
                    if let Some(s) = read.stub_at(id)? {
                        per.entry(s.first).or_insert_with(|| (s, Vec::new())).1.push(id);
                    }
                }
                Ok(())
            })?;
        }
        for (stub, mut named) in per.into_values() {
            named.sort_unstable();
            let k = named.len();
            let prefix = named.iter().enumerate().all(|(i, id)| *id == stub.first + i as i64);
            ensure!(k <= stub.who.len(), "more operations than a stub holds");
            labels.insert(stub.label);
            if prefix {
                self.cut_stub(&stub, k)?;
            }
            handled.extend(named);
        }
        Ok((handled, labels))
    }
}
