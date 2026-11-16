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

use super::derived::{
    count_prefix, decode_set, dense, referrers_prefix, set_prefix, COUNT_BITS, NO_TARGET, PARENTS,
    REFERRERS, SETS, UNIVERSE,
};
use super::mass::{entry_key, Listed, DELETES_PART};
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
const DELETE: &str = "mass_delete";

/// The records of a stub.
#[derive(Clone)]
pub(super) enum Who {
    /// Their dense ids, as a write names them: nothing was read to write
    /// them. A reindex, which numbers the ids again, turns them into uuids.
    Ids(RoaringBitmap),
    /// Their uuids, in the order their ids had.
    Uuids(Vec<Uuid>),
}

/// A record of a folded deletion as it is put back: its dense id, its uuid, and
/// the rows the log froze.
type Folded = (u32, Uuid, Vec<FieldRow>);

impl Who {
    fn len(&self) -> usize {
        match self {
            Who::Ids(ids) => ids.len() as usize,
            Who::Uuids(uuids) => uuids.len(),
        }
    }
}

/// A stub, decoded.
#[derive(Clone)]
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
    /// A deletion: the operations are `delete_metarecord`s, which have no field
    /// and no value, and the rows each had before are the record's own while
    /// it is still stored, then the snapshots of its operation.
    pub(super) delete: bool,
    /// A deletion whose records the fold wrote into the snapshots.
    pub(super) in_snaps: bool,
    /// A stub that reverts operations of another: operation `first + k`
    /// reverts operation `reverts + k` (kept where an ordinary operation keeps
    /// the operation it reverts; an older stub has none).
    pub(super) reverts: Option<i64>,
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
            .opt_bytes(Some(self.field.as_bytes()))
            .opt_int(self.reverts)
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
        let folded = if self.in_snaps { 2 } else { 0 };
        match &self.who {
            Who::Uuids(uuids) => {
                out.0.push(folded);
                for u in uuids {
                    out.0.extend_from_slice(u.as_bytes());
                }
            }
            Who::Ids(ids) => {
                out.0.push(1 + folded);
                ids.serialize_into(&mut out.0).expect("a bitmap into a vector");
            }
        }
        out.0
    }

    fn type_name(&self) -> &'static str {
        if self.delete {
            return DELETE;
        }
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
            DELETE => Kind::Remove,
            _ => return Ok(None),
        };
        let delete = head.op_type == DELETE;
        // Past the header: what `dec_op` read.
        let mut r = In(b);
        r.int()?;
        r.int()?;
        r.opt_bytes()?;
        r.bytes()?;
        r.bytes()?;
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
        let who = match tag & 1 {
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
            delete,
            in_snaps: tag & 2 != 0,
            reverts: head.reverts_op_id,
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
                _ if self.delete => "delete_metarecord",
                (Kind::Add, _) => "append_field",
                (Kind::Replace, false) => "set_field",
                _ => "delete_field",
            }
            .into(),
            entity_uuid: self.entity(read, (id - self.first) as usize)?,
            field_name: (!self.delete).then(|| self.field.clone()),
            reverts_op_id: self.reverts.map(|r| r + (id - self.first)),
            origin: self.origin.clone(),
        })
    }

    /// The rows operation `id` of the stub has after it, or before.
    pub(super) fn snapshot(&self, id: i64, after: bool) -> Vec<FieldRow> {
        let row = |value: &Value| FieldRow { name: self.field.clone(), value: value.clone() };
        if self.delete {
            // Read from the record, or the snapshots: not a stub's to say.
            return Vec::new();
        }
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

    /// The rows `append_field` operation `op_id` added, a stub's included:
    /// none for any other operation.
    pub(super) fn added_rows(&self, op_id: i64) -> Result<Vec<FieldRow>> {
        if let Some(stub) = self.stub_at(op_id)? {
            return Ok(if stub.delete { Vec::new() } else { stub.snapshot(op_id, true) });
        }
        let mut out = Vec::new();
        let prefix = key(&[&be(op_id), &[1u8]]);
        for e in self.t.snaps.prefix_iter(self.r, &prefix)? {
            out.push(super::dec_row(e?.1)?);
        }
        self.count(out.len() as u64 + 1)?;
        Ok(out)
    }

    /// What operation `op_id` added to its entity's version: the number an
    /// ordinary operation keeps, and for a stub's the one its own rows give.
    pub(super) fn version_delta(&self, op_id: i64) -> Result<u64> {
        if let Some(stub) = self.stub_at(op_id)? {
            let before = self.snapshot_rows(op_id)?;
            return Ok(crate::version::net(&before, &stub.snapshot(op_id, true)));
        }
        self.count(1)?;
        let raw = self.t.ops.get(self.r, &be(op_id))?.context("an operation that is not there")?;
        let delta = super::op_delta(raw)?.context("an operation with no version delta")?;
        Ok(delta as u64)
    }

    /// The rows operation `op_id` took off its record, a stub's included.
    pub(super) fn snapshot_rows(&self, op_id: i64) -> Result<Vec<FieldRow>> {
        let stub = self.stub_at(op_id)?;
        if let Some(stub) = &stub {
            if !stub.delete {
                return Ok(stub.snapshot(op_id, false));
            }
        }
        let mut out = Vec::new();
        let prefix = key(&[&be(op_id), &[0u8]]);
        for e in self.t.snaps.prefix_iter(self.r, &prefix)? {
            out.push(super::dec_row(e?.1)?);
        }
        self.count(out.len() as u64 + 1)?;
        // A deletion's rows are the snapshots the fold wrote, and for a record
        // it has not folded — still pending, or given back — the record's own.
        if out.is_empty() {
            if let Some(stub) = stub.filter(|s| s.delete) {
                return self.deleted_rows(&stub, op_id);
            }
        }
        Ok(out)
    }

    /// The rows the record of operation `op_id` of a deletion held, which are
    /// still in its key: the deletion wrote nothing of them.
    fn deleted_rows(&self, stub: &Stub, op_id: i64) -> Result<Vec<FieldRow>> {
        let Who::Ids(ids) = &stub.who else { return Ok(Vec::new()) };
        let id =
            ids.select((op_id - stub.first) as u32).context("a deletion shorter than its ids")?;
        let rows = self.record_raw(id)?;
        if !rows.is_empty() {
            return Ok(rows);
        }
        // The record is gone: it was given back and deleted again — a redo —
        // by an operation of the chain of reverts, whose own before snapshot
        // holds what it deleted, which is what this deletion took.
        let uuid = self.uuid_of_id(id)?;
        if let Some(chain) = self.revert_chain(uuid, op_id)? {
            for member in chain.into_iter().rev() {
                if self.op(member)?.is_some_and(|o| o.op_type == "delete_metarecord") {
                    return self.snapshot_rows(member);
                }
            }
        }
        Ok(Vec::new())
    }

    /// The operations after `op_id` on the record of a pending deletion that
    /// are its revert, the revert of that, and so on (a chain, oldest first);
    /// `None` when something else was written to the record since.
    fn revert_chain(&self, uuid: Uuid, op_id: i64) -> Result<Option<Vec<i64>>> {
        let mut chain = vec![op_id];
        for later in self.entity_op_ids_after(uuid, op_id)? {
            let Some(op) = self.op(later)? else { continue };
            match op.reverts_op_id {
                Some(r) if chain.contains(&r) => chain.push(later),
                _ => return Ok(None),
            }
        }
        chain.remove(0);
        Ok(Some(chain))
    }

    /// The record of operation `op_id` of a pending deletion, as `(id, uuid)`.
    fn deleted_record(&self, stub: &Stub, op_id: i64) -> Result<Option<(u32, Uuid)>> {
        let Who::Ids(ids) = &stub.who else { return Ok(None) };
        let Some(id) = ids.select((op_id - stub.first) as u32) else { return Ok(None) };
        Ok(Some((id, self.uuid_of_id(id)?)))
    }

    /// Whether the rows operation `op_id` took off its record are known: not
    /// for a deletion still pending whose record was written to since by
    /// anything but a revert of that deletion, or of its revert — the record is
    /// no longer what the deletion took, and the log, which reads it there,
    /// would say today's rows.
    pub(super) fn before_known(&self, op_id: i64) -> Result<bool> {
        let Some(stub) = self.stub_at(op_id)? else { return Ok(true) };
        if !stub.delete {
            return Ok(true);
        }
        // Rows the fold froze are known; the others are the record's own.
        let prefix = key(&[&be(op_id), &[0u8]]);
        if self.t.snaps.prefix_iter(self.r, &prefix)?.next().is_some() {
            return Ok(true);
        }
        let Some((_, uuid)) = self.deleted_record(&stub, op_id)? else { return Ok(true) };
        Ok(self.revert_chain(uuid, op_id)?.is_some())
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
        reverts: Option<i64>,
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
                // A revert names the operation each of its own reverts, in the
                // order of the records: it must change exactly those.
                if reverts.is_some() && changed != *ids {
                    return Ok(None);
                }
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
            return Ok(Some(MassApplied { changed, last_op: 0, touches_watch: false }));
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
            delete: false,
            in_snaps: false,
            reverts,
        };
        let last = stub.last();
        self.put_stub(&stub)?;
        Ok(Some(MassApplied { changed, last_op: last, touches_watch: false }))
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
        self.count_rows_gone(field, changed)?;
        let kept: Vec<&Value> = values.iter().filter(|v| !matches!(v, Value::Nothing)).collect();
        if let Some(first) = kept.first() {
            let delta = kept.len() as i64 * changed.len() as i64;
            self.count_type(field, first, delta)?;
        }
        Ok(())
    }

    /// The type counts of `field` after every row of `changed` goes: as many
    /// rows as the counters say, of the one type the field has.
    fn count_rows_gone(&self, field: &str, changed: &RoaringBitmap) -> Result<()> {
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
        Ok(())
    }

    /// The fields the store has rows of, and those among them that hold
    /// positions (`tree_ref`, `hierarchy_ref`).
    fn fields_held(&self) -> Result<(Vec<String>, Vec<String>)> {
        let txn = self.txn.borrow();
        let (mut all, mut positional) = (Vec::<String>::new(), Vec::<String>::new());
        for e in self.t.field_types.iter(&txn)? {
            let (name, ty) = super::derived::unesc(e?.0)?;
            let name = String::from_utf8(name).context("a field name")?;
            if matches!(ty, b"tree_ref" | b"hierarchy_ref") {
                positional.push(name.clone());
            }
            if all.last() != Some(&name) {
                all.push(name);
            }
        }
        positional.dedup();
        Ok((all, positional))
    }

    /// Deletes the records `ids` as one stub (doc "Mass operations in the
    /// log"): their ids leave the universe, which is all any answer needs,
    /// and the records stay stored until the fold. Kept exact now, by set
    /// operations and without reading a record: the type counts, which sum
    /// the counters, and the parents that lost their last child. The
    /// histograms count rows no universe can sift, so they are given back by
    /// the fold. `None` — nothing written — when a record has children
    /// (which leave their ancestors' descendants, one record at a time) or
    /// the write has changed sets this one would read stale.
    pub(super) fn mass_delete_log(
        &self,
        ids: &RoaringBitmap,
        label: i64,
        timestamp: i64,
        origin: Option<&str>,
    ) -> Result<Option<MassApplied>> {
        let universe = self.read_chunked(SETS, &set_prefix(UNIVERSE, None))?;
        let changed = ids & &universe;
        if changed.is_empty() {
            return Ok(Some(MassApplied { changed, last_op: 0, touches_watch: false }));
        }
        if let Some(root) = crate::store::Rows::child_by_bytes(self, "mfr_path", None, b"")? {
            if let Some(id) = self.dense_id(root.as_bytes())? {
                if changed.contains(id) {
                    return Err(crate::error::DomainError::BadRequest(
                        "cannot delete the repository root metarecord".to_string(),
                    )
                    .into());
                }
            }
        }
        let (fields, positional) = self.fields_held()?;
        // A folder deleted here has all its children deleted here too, or the
        // children are unpicked one by one.
        let left = &universe - &changed;
        for field in &positional {
            let parents = self.read_chunked(SETS, &set_prefix(PARENTS, Some(field)))?;
            for parent in &parents & &changed {
                let kids = self.read_chunked(SETS, &referrers_prefix(field, parent))?;
                if !(&kids & &left).is_empty() {
                    return Ok(None);
                }
            }
        }
        for field in &fields {
            self.count_rows_gone(field, &changed)?;
        }
        for field in &positional {
            self.parents_after(field, &changed, &left)?;
        }
        let mut touches_watch = false;
        for field in ["mf_watch", "mf_ignore", crate::eligibility::WATCH_EXCEEDED] {
            touches_watch |= !(&self.holder_ids_now(field)? & &changed).is_empty();
        }
        self.chunk_apply(SETS, &set_prefix(UNIVERSE, None), &changed, false)?;
        let stub = Stub {
            first: self.next_op_id()?,
            label,
            timestamp,
            origin: origin.map(str::to_string),
            kind: Kind::Remove,
            field: String::new(),
            values: Vec::new(),
            before: Vec::new(),
            who: Who::Ids(changed.clone()),
            delete: true,
            in_snaps: false,
            reverts: None,
        };
        let last = stub.last();
        self.put_stub(&stub)?;
        let entry = Listed {
            op: stub.first as u64,
            kind: Kind::Remove,
            field: String::new(),
            values: Vec::new(),
            ids: changed.clone(),
        };
        self.t.mass.put(
            &mut self.txn.borrow_mut(),
            &entry_key(DELETES_PART, entry.op),
            &entry.encode(),
        )?;
        self.mass_lists.store(true, std::sync::atomic::Ordering::Relaxed);
        self.deletes.store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(Some(MassApplied { changed, last_op: last, touches_watch }))
    }

    /// The fields with data under set kind `kind` (counts, referrers): the
    /// names the keys of that kind begin with.
    fn fields_with_sets(&self, kind: u8) -> Result<Vec<String>> {
        let txn = self.txn.borrow();
        let mut out: Vec<String> = Vec::new();
        for e in self.t.sets.prefix_iter(&txn, &[kind])? {
            let (k, _) = e?;
            let (name, _) = super::derived::unesc(&k[1..])?;
            let name = String::from_utf8(name).context("a field name")?;
            if out.last() != Some(&name) {
                out.push(name);
            }
        }
        Ok(out)
    }

    /// Reverts a run of operations of one mass add or remove at once: the
    /// operations `first_op..=last_op`, which are the stub's, are taken back
    /// by a stub of the opposite kind over the same records, each of whose
    /// operations names the one it reverts. `None` — nothing written — when
    /// they are not so (not a stub's, a replacement, several values) or when a
    /// record no longer holds what the revert would change.
    pub(super) fn mass_revert_log(
        &self,
        first_op: i64,
        last_op: i64,
        label: i64,
        timestamp: i64,
        origin: Option<&str>,
    ) -> Result<Option<MassApplied>> {
        let stub = {
            let txn = self.txn.borrow();
            self.read_in(&txn, &mut |read: &Read| read.stub_at(first_op))?
        };
        let Some(stub) = stub.filter(|s| {
            !s.delete && s.kind != Kind::Replace && s.values.len() == 1 && s.covers(last_op)
        }) else {
            return Ok(None);
        };
        let Who::Ids(all) = &stub.who else { return Ok(None) };
        let (a, n) = ((first_op - stub.first) as usize, (last_op - first_op + 1) as usize);
        let ids: RoaringBitmap = all.iter().skip(a).take(n).collect();
        if ids.len() != n as u64 {
            return Ok(None);
        }
        // A record deleted since, then given back, is alive under another id
        // (the id it had stays in the holders, which only the universe
        // sifts): the revert by ids would miss it, and say it did it.
        let universe = self.read_chunked(SETS, &set_prefix(UNIVERSE, None))?;
        if !ids.is_subset(&universe) {
            return Ok(None);
        }
        let opposite = if stub.kind == Kind::Add { Kind::Remove } else { Kind::Add };
        self.mass_log(
            opposite,
            &stub.field,
            &stub.values,
            &ids,
            label,
            timestamp,
            origin,
            Some(first_op),
        )
    }

    /// Gives back a deletion still pending (doc "Mass operations in the log"):
    /// the operations `first_op..=last_op`, which are a deletion's, become
    /// those of records alive again. The records never left the store, so
    /// their ids go back into the universe, the type counts and the parents
    /// the deletion took off are given back, and the deletion's mark no longer
    /// names them; nothing is read or written per record. `None` — nothing
    /// written — when the deletion is not pending as a whole for these
    /// operations (folded, partly given back, cut by a prune), when a record is
    /// alive already, or when a field would have to be told its type again.
    pub(super) fn mass_undelete_log(
        &self,
        first_op: i64,
        last_op: i64,
    ) -> Result<Option<MassApplied>> {
        let stub = {
            let txn = self.txn.borrow();
            self.read_in(&txn, &mut |read: &Read| read.stub_at(first_op))?
        };
        let Some(stub) = stub.filter(|s| s.delete && s.covers(last_op)) else {
            return Ok(None);
        };
        let Who::Ids(all) = &stub.who else { return Ok(None) };
        // The fold wrote the records out of the store when the stub holds
        // their rows (`in_snaps`): they are put back from there.
        let folded = stub.in_snaps;
        // The mark that names every one of these operations, which a fold
        // took away.
        let mark = if folded {
            None
        } else {
            let marks = {
                let txn = self.txn.borrow();
                super::mass::read_part(&self.t, &txn, DELETES_PART)?
            };
            let found = marks.into_iter().find(|m| {
                let (from, to) = (m.op as i64, m.op as i64 + m.ids.len() as i64 - 1);
                from <= first_op && last_op <= to
            });
            if found.is_none() {
                return Ok(None);
            }
            found
        };
        let (a, n) = ((first_op - stub.first) as usize, (last_op - first_op + 1) as usize);
        let ids: RoaringBitmap = all.iter().skip(a).take(n).collect();
        let universe = self.read_chunked(SETS, &set_prefix(UNIVERSE, None))?;
        if ids.len() != n as u64 || !(&ids & &universe).is_empty() {
            return Ok(None);
        }
        // Everything is read before anything is written: a `None` leaves the
        // transaction as it was.
        let mut counts: Vec<(String, Vec<Vec<u8>>, u64)> = Vec::new();
        for field in self.fields_with_sets(super::derived::COUNT)? {
            let mut back = 0u64;
            for bit in 0..COUNT_BITS {
                let plane = self.read_chunked(SETS, &count_prefix(&field, bit))?;
                back += (&plane & &ids).len() << bit;
            }
            if back == 0 {
                continue;
            }
            let types: Vec<Vec<u8>> = {
                let txn = self.txn.borrow();
                let prefix = name_key(&field);
                let mut out = Vec::new();
                for e in self.t.field_types.prefix_iter(&txn, &prefix)? {
                    out.push(e?.0[prefix.len()..].to_vec());
                }
                out
            };
            if types.is_empty() {
                return Ok(None);
            }
            counts.push((field, types, back));
        }
        // What the fold took out of the store: the records, from the rows the
        // log froze. Each must still be the one it was — its id its uuid's, no
        // record under either — and a position it held not taken since.
        let revived =
            if folded { self.folded_records(&stub, all, first_op, last_op)? } else { None };
        if folded && revived.is_none() {
            return Ok(None);
        }
        let alive = &universe | &ids;
        let mut parents: Vec<(String, u32)> = Vec::new();
        for field in self.fields_with_sets(REFERRERS)? {
            let prefix = set_prefix(REFERRERS, Some(&field));
            let mut groups: BTreeMap<u32, RoaringBitmap> = BTreeMap::new();
            {
                let txn = self.txn.borrow();
                for e in self.t.sets.prefix_iter(&txn, &prefix)? {
                    let (k, v) = e?;
                    if k.len() == prefix.len() + 6 {
                        *groups.entry(dense(&k[prefix.len()..])).or_default() |= decode_set(v)?;
                    }
                }
            }
            for (parent, kids) in groups {
                if parent == NO_TARGET || (&kids & &ids).is_empty() {
                    continue;
                }
                // A parent that is gone would leave its children without one.
                if !alive.contains(parent) {
                    return Ok(None);
                }
                parents.push((field.clone(), parent));
            }
        }
        let mut touches_watch = false;
        for field in ["mf_watch", "mf_ignore", crate::eligibility::WATCH_EXCEEDED] {
            touches_watch |= !(&self.holder_ids_now(field)? & &ids).is_empty();
        }
        // What is read is settled: the writes.
        for (id, uuid, rows) in revived.iter().flatten() {
            self.put_record(*id, rows)?;
            self.t.metarecords.put(&mut self.txn.borrow_mut(), uuid.as_bytes(), &[])?;
            for row in rows {
                self.add_hist(&row.name, &row.value)?;
                if let Some(k) = Self::forest_key(&row.name, &row.value) {
                    let v = key(&[uuid.as_bytes(), &id.to_be_bytes()]);
                    self.t.forest.put(&mut self.txn.borrow_mut(), &k, &v)?;
                }
            }
        }
        self.chunk_apply(SETS, &set_prefix(UNIVERSE, None), &ids, true)?;
        for (field, types, back) in counts {
            for ty in types {
                self.count_type_named(&field, &ty, back as i64)?;
            }
        }
        for (field, parent) in parents {
            self.set_member(PARENTS, Some(&field), parent, true)?;
        }
        let Some(mark) = mark else {
            return Ok(Some(MassApplied { changed: ids, last_op, touches_watch }));
        };
        // The mark keeps the operations it still names, each at its own number.
        let mut w = self.txn.borrow_mut();
        self.t.mass.delete(&mut w, &entry_key(DELETES_PART, mark.op))?;
        let ranks: Vec<u32> = mark.ids.iter().collect();
        let (lo, hi) = ((first_op - mark.op as i64) as usize, (last_op - mark.op as i64) as usize);
        for (from, part) in [(0, &ranks[..lo]), (hi + 1, &ranks[hi + 1..])] {
            if part.is_empty() {
                continue;
            }
            let entry = Listed {
                op: mark.op + from as u64,
                kind: Kind::Remove,
                field: String::new(),
                values: Vec::new(),
                ids: part.iter().copied().collect(),
            };
            self.t.mass.put(&mut w, &entry_key(DELETES_PART, entry.op), &entry.encode())?;
        }
        Ok(Some(MassApplied { changed: ids, last_op, touches_watch }))
    }

    /// The records the fold of a deletion wrote out of the store, for the
    /// operations `first_op..=last_op` of `stub` (whose ids are `all`): each
    /// `(id, uuid, rows)`, the rows being those the log froze. `None` when one
    /// is not as the fold left it: a record stored under its id or its uuid
    /// (given back, or not folded yet), an id that is no longer its uuid's, or
    /// a position a live record took since.
    fn folded_records(
        &self,
        stub: &Stub,
        all: &RoaringBitmap,
        first_op: i64,
        last_op: i64,
    ) -> Result<Option<Vec<Folded>>> {
        let at = (first_op - stub.first) as usize;
        let n = (last_op - first_op + 1) as usize;
        let mut out = Vec::with_capacity(n);
        let txn = self.txn.borrow();
        for (rank, id) in all.iter().skip(at).take(n).enumerate() {
            let op = first_op + rank as i64;
            let Some(uuid) = self.t.uuids.get(&txn, &id.to_be_bytes())? else { return Ok(None) };
            let Ok(uuid) = Uuid::from_slice(uuid) else { return Ok(None) };
            let named = self.t.ids.get(&txn, uuid.as_bytes())?;
            if named != Some(&id.to_be_bytes()[..])
                || self.t.metarecords.get(&txn, uuid.as_bytes())?.is_some()
                || self.t.records.get(&txn, &id.to_be_bytes())?.is_some()
            {
                return Ok(None);
            }
            let prefix = key(&[&be(op), &[0u8]]);
            let mut rows = Vec::new();
            for e in self.t.snaps.prefix_iter(&txn, &prefix)? {
                rows.push(dec_row(e?.1)?);
            }
            for row in &rows {
                let Some(k) = Self::forest_key(&row.name, &row.value) else { continue };
                if let Some(v) = self.t.forest.get(&txn, &k)? {
                    let holder = super::derived::dense(&v[super::FOREST_ID..]);
                    if holder != id && self.live_in(&txn, holder)? {
                        return Ok(None);
                    }
                }
            }
            out.push((id, uuid, rows));
        }
        Ok(Some(out))
    }

    /// The parents of `field` that had a child among `gone` and have none
    /// left in `left` (the universe after the deletion) are no parents. One
    /// sequential pass over the field's referrers.
    fn parents_after(&self, field: &str, gone: &RoaringBitmap, left: &RoaringBitmap) -> Result<()> {
        let prefix = set_prefix(REFERRERS, Some(field));
        // The stored chunks, the transaction's own in their stead: its earlier
        // writes are in the order they were made.
        let mut chunks: BTreeMap<(u32, u16), RoaringBitmap> = BTreeMap::new();
        {
            let txn = self.txn.borrow();
            for e in self.t.sets.prefix_iter(&txn, &prefix)? {
                let (k, v) = e?;
                if k.len() == prefix.len() + 6 {
                    let at = (
                        dense(&k[prefix.len()..]),
                        u16::from_be_bytes([k[k.len() - 2], k[k.len() - 1]]),
                    );
                    chunks.insert(at, decode_set(v)?);
                }
            }
        }
        let own = [&[SETS][..], &prefix].concat();
        for (k, bm) in self.sets.borrow().iter() {
            if k.len() == own.len() + 6 && k.starts_with(&own) {
                let at =
                    (dense(&k[own.len()..]), u16::from_be_bytes([k[k.len() - 2], k[k.len() - 1]]));
                chunks.insert(at, bm.clone());
            }
        }
        let mut groups: Vec<(u32, RoaringBitmap)> = Vec::new();
        for ((parent, _), kids) in chunks {
            match groups.last_mut() {
                Some((p, all)) if *p == parent => *all |= kids,
                _ => groups.push((parent, kids)),
            }
        }
        for (parent, kids) in groups {
            if parent == NO_TARGET || (&kids & gone).is_empty() {
                continue;
            }
            if (&(&kids - gone) & left).is_empty() {
                self.set_member(PARENTS, Some(field), parent, false)?;
            }
        }
        Ok(())
    }

    /// Folds the oldest deletion pending: the records it deleted are written
    /// out of the store, after their rows went to the snapshots of its
    /// operations (which read them from the records until then) and to the
    /// histograms. Returns whether there was one.
    pub(super) fn fold_one_delete(&self) -> Result<bool> {
        let prefix = DELETES_PART.to_be_bytes();
        let found = {
            let txn = self.txn.borrow();
            let mut entries = self.t.mass.prefix_iter(&txn, &prefix)?;
            match entries.next() {
                Some(e) => {
                    let (k, v) = e?;
                    let op = u64::from_be_bytes(k[4..12].try_into().expect("a number"));
                    Some((k.to_vec(), Listed::decode(op, v)?))
                }
                None => None,
            }
        };
        let Some((entry_key_bytes, listed)) = found else {
            self.deletes.store(false, std::sync::atomic::Ordering::Relaxed);
            return Ok(false);
        };
        let first = listed.op as i64;
        // The stubs holding its operations: the one it wrote, or what a prune
        // left of it (the first operations of a stub go first).
        let mut covering: Option<Stub> = None;
        let mut marked: BTreeMap<i64, Stub> = BTreeMap::new();
        for (rank, id) in listed.ids.iter().enumerate() {
            let op_id = first + rank as i64;
            let rows = self.load_record(id)?.plain_rows();
            for row in &rows {
                self.drop_hist(&row.name, &row.value)?;
            }
            if !covering.as_ref().is_some_and(|s| s.covers(op_id)) {
                let txn = self.txn.borrow();
                covering = self
                    .read_in(&txn, &mut |read: &Read| read.stub_at(op_id))?
                    .filter(|s| s.delete);
            }
            let uuid = self.t.uuids.get(&self.txn.borrow(), &id.to_be_bytes())?.map(<[u8]>::to_vec);
            let mut w = self.txn.borrow_mut();
            if let Some(stub) = &covering {
                for (n, row) in rows.iter().enumerate() {
                    let k = key(&[&be(op_id), &[0u8], &(n as u32).to_be_bytes()]);
                    self.t.snaps.put(&mut w, &k, &enc_row(row))?;
                }
                marked.entry(stub.first).or_insert_with(|| stub.clone());
            }
            self.t.records.delete(&mut w, &id.to_be_bytes())?;
            // The key of a uuid is the metarecord's, if it is back by now.
            if let Some(uuid) = uuid {
                if super::derived::id_of(&self.t, &w, &uuid)? == Some(id) {
                    self.t.metarecords.delete(&mut w, &uuid)?;
                }
            }
        }
        for (at, mut stub) in marked {
            stub.in_snaps = true;
            self.t.ops.put(&mut self.txn.borrow_mut(), &be(at), &stub.encode())?;
        }
        self.t.mass.delete(&mut self.txn.borrow_mut(), &entry_key_bytes)?;
        Ok(true)
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
        if let Some(target) = stub.reverts {
            for k in 0..stub.who.len() as i64 {
                t.reverted_by.put(&mut w, &key(&[&be(target + k), &be(stub.first + k)]), &[])?;
            }
        }
        Ok(())
    }

    /// Takes the first `k` operations out of a stub, all of them when it
    /// holds no more: pruning and retention take the oldest operations.
    fn cut_stub(&self, stub: &Stub, k: usize) -> Result<()> {
        let mut w = self.txn.borrow_mut();
        let t = self.t;
        if stub.delete && stub.in_snaps {
            for id in stub.first..stub.first + k as i64 {
                let doomed: Vec<Vec<u8>> = t
                    .snaps
                    .prefix_iter(&w, &be(id))?
                    .map(|e| e.map(|(key, _)| key.to_vec()))
                    .collect::<std::result::Result<_, _>>()?;
                for key in doomed {
                    t.snaps.delete(&mut w, &key)?;
                }
            }
        }
        if let Some(target) = stub.reverts {
            for j in 0..k as i64 {
                t.reverted_by.delete(&mut w, &key(&[&be(target + j), &be(stub.first + j)]))?;
            }
        }
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
            delete: stub.delete,
            in_snaps: stub.in_snaps,
            reverts: stub.reverts.map(|r| r + k as i64),
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
