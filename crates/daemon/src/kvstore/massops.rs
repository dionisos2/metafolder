//! The operations of a write to many records, one entry in the log (doc
//! "Mass operations in the log").
//!
//! A write of one value to, or off, a field of N records is logged as one
//! *stub*: the field, the value, and the uuids of the records that changed,
//! sorted. It holds the N operation ids that follow its key — `first`, which
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

use super::{be, dec_op, dec_row, enc_row, from_be, key, uuid_of, In, KvTxn, Out, Read, ROOT};
use crate::log::OpRow;
use crate::rows::FieldRow;
use crate::store::{MassApplied, MassKind as Kind};

const ADD: &str = "mass_add";
const REMOVE: &str = "mass_remove";

/// A stub, decoded.
pub(super) struct Stub {
    pub(super) first: i64,
    pub(super) label: i64,
    pub(super) timestamp: i64,
    pub(super) origin: Option<String>,
    pub(super) kind: Kind,
    pub(super) row: FieldRow,
    /// Sorted: the record of operation `first + k` is `uuids[k]`.
    pub(super) uuids: Vec<Uuid>,
}

impl Stub {
    pub(super) fn last(&self) -> i64 {
        self.first + self.uuids.len() as i64 - 1
    }

    pub(super) fn covers(&self, id: i64) -> bool {
        (self.first..=self.last()).contains(&id)
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Out::default();
        out.int(self.label)
            .int(self.timestamp)
            .opt_bytes(self.origin.as_deref().map(str::as_bytes))
            .bytes(if self.kind == Kind::Add { ADD } else { REMOVE }.as_bytes())
            .bytes(ROOT.as_slice())
            .opt_int(None)
            .opt_int(None)
            .opt_bytes(Some(self.row.name.as_bytes()))
            .opt_int(None)
            .bytes(&enc_row(&self.row));
        for u in &self.uuids {
            out.0.extend_from_slice(u.as_bytes());
        }
        out.0
    }

    /// The stub a value holds, `None` when it is an ordinary operation's.
    pub(super) fn decode(first: i64, b: &[u8]) -> Result<Option<Self>> {
        let head = dec_op(first, b)?;
        let kind = match head.op_type.as_str() {
            ADD => Kind::Add,
            REMOVE => Kind::Remove,
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
        let row = dec_row(r.bytes()?)?;
        let uuids = r.0.chunks_exact(16).map(uuid_of).collect();
        Ok(Some(Stub {
            first,
            label: head.label_id,
            timestamp: head.timestamp,
            origin: head.origin,
            kind,
            row,
            uuids,
        }))
    }

    /// The operation `id` is, one of the stub's.
    pub(super) fn op(&self, id: i64) -> OpRow {
        OpRow {
            id,
            label_id: self.label,
            timestamp: self.timestamp,
            op_type: if self.kind == Kind::Add { "append_field" } else { "delete_field" }.into(),
            entity_uuid: self.uuids[(id - self.first) as usize],
            field_name: Some(self.row.name.clone()),
            reverts_op_id: None,
            origin: self.origin.clone(),
        }
    }

    /// The rows an operation of the stub has after it, or before.
    pub(super) fn snapshot(&self, after: bool) -> Vec<FieldRow> {
        if after == (self.kind == Kind::Add) {
            vec![self.row.clone()]
        } else {
            Vec::new()
        }
    }

    /// The place of a record among the stub's.
    pub(super) fn position(&self, uuid: Uuid) -> Option<usize> {
        self.uuids.binary_search(&uuid).ok()
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
                out.push(s.op(id));
                continue;
            }
            self.count(1)?;
            let b = self.t.ops.get(self.r, &be(id))?.map(<[u8]>::to_vec);
            match b {
                Some(b) => match Stub::decode(id, &b)? {
                    Some(s) => {
                        out.push(s.op(id));
                        stub = Some(s);
                    }
                    None => out.push(dec_op(id, &b)?),
                },
                None => {
                    let s =
                        self.stub_at(id)?.with_context(|| format!("operation {id} vanished"))?;
                    out.push(s.op(id));
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
            if let Some(k) = stub.position(entity) {
                let id = first + k as i64;
                if id > after {
                    out.push(id);
                }
            }
        }
        out.reverse();
        Ok(out)
    }

    /// How many operations the stubs hold beyond one entry each.
    pub(super) fn mass_extra(&self) -> Result<i64> {
        self.count(1)?;
        Ok(self.meta("mass_extra")?.unwrap_or(0))
    }
}

impl KvTxn<'_> {
    /// Logs a write of `value` to, or off, `field` of `uuids` as one stub,
    /// and derives what it changes (doc "Mass operations in the log").
    #[allow(clippy::too_many_arguments)]
    pub(super) fn mass_log(
        &self,
        kind: Kind,
        field: &str,
        value: &Value,
        uuids: &[Uuid],
        label: i64,
        timestamp: i64,
        origin: Option<&str>,
    ) -> Result<Option<MassApplied>> {
        if kind == Kind::Replace || !self.mass_eligible(field, kind, std::slice::from_ref(value))? {
            return Ok(None);
        }
        let key = super::derived::value_key(value).context("a value with no key")?;
        let mut ids = RoaringBitmap::new();
        let mut named: BTreeMap<u32, Uuid> = BTreeMap::new();
        let mut requested: Vec<(u32, Uuid)> = Vec::new();
        for &u in uuids {
            if !crate::store::Rows::exists(self, u)? {
                if kind == Kind::Add {
                    return Err(crate::error::DomainError::NotFound(format!(
                        "Metarecord not found: {u}"
                    ))
                    .into());
                }
                continue;
            }
            let id =
                self.dense_id(u.as_bytes())?.with_context(|| format!("no dense id for {u}"))?;
            ids.insert(id);
            named.insert(id, u);
            requested.push((id, u));
        }
        let have = self.effective(field, value, &key)?;
        let changed = if kind == Kind::Add { &ids - &have } else { &ids & &have };
        if changed.is_empty() {
            return Ok(Some(MassApplied { changed: Vec::new(), last_op: 0 }));
        }
        let n = changed.len() as i64;
        let delta = if kind == Kind::Add { n } else { -n };
        self.count_type(field, value, delta)?;
        self.mass_end_ids(field, kind, vec![value.clone()], changed.clone())?;
        let mut uuids: Vec<Uuid> = changed.iter().map(|id| named[&id]).collect();
        uuids.sort();
        // Told to the caller in the order it asked.
        let mut seen = HashSet::new();
        let reported: Vec<Uuid> = requested
            .into_iter()
            .filter(|(id, _)| changed.contains(*id))
            .filter(|(_, u)| seen.insert(*u))
            .map(|(_, u)| u)
            .collect();
        let first = self.next_op_id()?;
        let stub = Stub {
            first,
            label,
            timestamp,
            origin: origin.map(str::to_string),
            kind,
            row: FieldRow { name: field.to_string(), value: value.clone() },
            uuids,
        };
        let last = stub.last();
        self.put_stub(&stub)?;
        Ok(Some(MassApplied { changed: reported, last_op: last }))
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
        t.meta.put(&mut w, b"mass_extra", &be(extra + stub.uuids.len() as i64 - 1))?;
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
        let kept = stub.uuids.len() - k;
        if kept == 0 {
            t.meta.put(&mut w, b"mass_extra", &be(extra - (stub.uuids.len() as i64 - 1)))?;
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
            row: stub.row.clone(),
            uuids: stub.uuids[k..].to_vec(),
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
            ensure!(k <= stub.uuids.len(), "more operations than a stub holds");
            labels.insert(stub.label);
            if prefix {
                self.cut_stub(&stub, k)?;
            }
            handled.extend(named);
        }
        Ok((handled, labels))
    }
}
