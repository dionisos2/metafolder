//! The rows of a record, and the writes to many records not yet written into
//! it (doc "Mass writes in the store").
//!
//! A write to many records at once is not written into each record: it is one
//! *entry* in the list of every part — `PART` ids, the high bits of the dense
//! id — it touches, naming the operation and the ids of that part. A record
//! is read as the rows it holds with the entries of its part applied, oldest
//! first. A fold writes them into the records and empties the list.
//!
//! The most recent write wins, a write to one record included, so a record
//! carries the number of the operation that wrote each of its rows (when
//! some entry could be older than it), and what was taken off it by hand
//! after an entry:
//!
//! | in a record         | says                                              |
//! |---------------------|---------------------------------------------------|
//! | a row's number      | an entry older than it does not remove the row    |
//! | a *tomb* (f, v, n)  | an entry older than `n` does not add `v` to `f`   |
//! | *cleared* (f, n)    | an entry older than `n` adds nothing to `f`       |
//!
//! | table | key           | value                                |
//! |-------|---------------|--------------------------------------|
//! | mass  | part · number | field · kind · values · ids of part  |
//!
//! A record's value is its rows, as ever, then — only when it has any — the
//! numbers, tombs and cleared fields; one without reads as before.

use std::collections::BTreeSet;

use anyhow::{bail, Result};
use heed::RoTxn;
use metafolder_core::metarecord::Value;
use roaring::RoaringBitmap;

use super::codec::{self, Dec, Enc};
use super::derived::decode_set;
use super::{get_value, put_value, In, KvStore, KvTxn, Out, State, Tables};
use crate::rows::FieldRow;
pub(crate) use crate::store::MassKind as Kind;

/// How many bits of a dense id a part leaves out: a part is 64 k ids, whose
/// bitmap in an entry takes 8 KiB at most.
pub(crate) const PART_BITS: u32 = 16;

pub(crate) fn part_of(id: u32) -> u32 {
    id >> PART_BITS
}

/// The part under which the deletions of many records wait for the fold: no id
/// has it, and the entries of the parts, which an id's high bits name, are
/// never this far.
pub(crate) const DELETES_PART: u32 = u32::MAX;

/// The part under which the records a deletion gave back wait for their rows to
/// be copied: while one is not written, the log reads the deletion's rows in the
/// record itself, and the mark says where — an entry of the same form as a
/// deletion's, the `k`-th id being the record of the operation `op + k`.
pub(crate) const THAWED_PART: u32 = u32::MAX - 1;

/// A record as stored.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Record {
    pub(crate) rows: Vec<FieldRow>,
    /// The number of the operation that wrote each row; `0` is older than
    /// every entry.
    pub(crate) ops: Vec<u64>,
    /// Values taken off by hand after the entries older than the number.
    pub(crate) tombs: Vec<(String, Value, u64)>,
    /// Fields emptied by hand after the entries older than the number.
    pub(crate) cleared: Vec<(String, u64)>,
}

impl Record {
    /// Rows no entry can override.
    pub(crate) fn plain(rows: Vec<FieldRow>) -> Record {
        let ops = vec![0; rows.len()];
        Record { rows, ops, tombs: Vec::new(), cleared: Vec::new() }
    }

    /// `rows` ([`codec::put_rows`]), then — only when it has any — `varint
    /// count · { kind · … }`: a row's number (`0` · varint row · varint
    /// number), a tomb (`1` · name · value · varint number), a cleared field
    /// (`2` · name · varint number).
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Enc::default();
        let order = codec::put_rows(&mut out, &self.rows);
        let numbered: Vec<_> = self.ops.iter().enumerate().filter(|(_, op)| **op != 0).collect();
        let n = numbered.len() + self.tombs.len() + self.cleared.len();
        if n > 0 {
            out.varint(n as u64);
            for (i, op) in numbered {
                out.raw(&[0]).varint(order[i] as u64).varint(*op);
            }
            for (name, value, op) in &self.tombs {
                out.raw(&[1]).bytes(name.as_bytes()).value(value).varint(*op);
            }
            for (name, op) in &self.cleared {
                out.raw(&[2]).bytes(name.as_bytes()).varint(*op);
            }
        }
        out.0
    }

    pub(crate) fn decode(b: &[u8]) -> Result<Record> {
        let mut r = Dec(b);
        let mut record = Record::plain(codec::get_rows(&mut r)?);
        if r.is_empty() {
            return Ok(record);
        }
        for _ in 0..r.varint()? {
            match r.take(1)?[0] {
                0 => {
                    let i = r.varint()? as usize;
                    let op = r.varint()?;
                    match record.ops.get_mut(i) {
                        Some(slot) => *slot = op,
                        None => bail!("a row number for a row a record does not have"),
                    }
                }
                1 => {
                    let name = r.string()?;
                    let value = r.value()?;
                    record.tombs.push((name, value, r.varint()?));
                }
                2 => {
                    let name = r.string()?;
                    record.cleared.push((name, r.varint()?));
                }
                other => bail!("an extra of kind {other} in a record"),
            }
        }
        Ok(record)
    }

    /// Takes the `nothing` out of a field that also holds values: a field
    /// holds one or the other (doc "Nothing versus unknown"), which records
    /// written before November 2026 did not keep. Whether it changed.
    pub(crate) fn drop_mixed_nothing(&mut self) -> bool {
        let valued: BTreeSet<String> = self
            .rows
            .iter()
            .filter(|r| !matches!(r.value, Value::Nothing))
            .map(|r| r.name.clone())
            .collect();
        let keep: Vec<bool> = self
            .rows
            .iter()
            .map(|r| !(matches!(r.value, Value::Nothing) && valued.contains(&r.name)))
            .collect();
        if keep.iter().all(|k| *k) {
            return false;
        }
        let mut k = keep.iter();
        self.rows.retain(|_| *k.next().expect("one per row"));
        let mut k = keep.iter();
        self.ops.retain(|_| *k.next().expect("one per row"));
        true
    }
}

/// One entry of a part's list.
#[derive(Clone, Debug)]
pub(crate) struct Listed {
    pub(crate) op: u64,
    pub(crate) kind: Kind,
    pub(crate) field: String,
    pub(crate) values: Vec<Value>,
    pub(crate) ids: RoaringBitmap,
}

impl Listed {
    pub(super) fn encode(&self) -> Vec<u8> {
        let mut out = Out::default();
        out.int(match self.kind {
            Kind::Add => 0,
            Kind::Remove => 1,
            Kind::Replace => 2,
        })
        .bytes(self.field.as_bytes())
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

    pub(super) fn decode(op: u64, bytes: &[u8]) -> Result<Listed> {
        let mut r = In(bytes);
        let kind = match r.int()? {
            0 => Kind::Add,
            1 => Kind::Remove,
            2 => Kind::Replace,
            other => bail!("an entry of kind {other}"),
        };
        let field = r.string()?;
        let n = r.int()?;
        let mut values = Vec::new();
        for _ in 0..n {
            values.push(get_value(&mut r)?);
        }
        Ok(Listed { op, kind, field, values, ids: decode_set(r.bytes()?)? })
    }
}

pub(super) fn entry_key(part: u32, op: u64) -> Vec<u8> {
    [&part.to_be_bytes()[..], &op.to_be_bytes()[..]].concat()
}

/// The entries of `part`, oldest first.
pub(crate) fn read_part(t: &Tables, r: &RoTxn<'_>, part: u32) -> Result<Vec<Listed>> {
    let mut out = Vec::new();
    for entry in t.mass.prefix_iter(r, &part.to_be_bytes())? {
        let (k, v) = entry?;
        out.push(Listed::decode(u64::from_be_bytes(k[4..12].try_into().expect("a number")), v)?);
    }
    Ok(out)
}

/// What `record` holds once `entries` — those naming its id, oldest first —
/// are applied, each row with the number of the operation that wrote it.
pub(crate) fn effective(record: &Record, entries: &[&Listed]) -> Vec<(FieldRow, u64)> {
    let mut rows: Vec<(FieldRow, u64)> =
        record.rows.iter().cloned().zip(record.ops.iter().copied()).collect();
    for e in entries {
        // Taken off, or emptied, by hand after the entry: it does not come back.
        let overridden = |value: &Value| {
            record.cleared.iter().any(|(n, t)| *n == e.field && *t > e.op)
                || record.tombs.iter().any(|(n, v, t)| *n == e.field && v == value && *t > e.op)
        };
        let holds = |rows: &[(FieldRow, u64)], value: &Value| {
            rows.iter().any(|(r, _)| r.name == e.field && &r.value == value)
        };
        match e.kind {
            Kind::Add | Kind::Replace => {
                if e.kind == Kind::Replace {
                    rows.retain(|(r, op)| !(r.name == e.field && *op < e.op));
                }
                for v in &e.values {
                    if !overridden(v) && !holds(&rows, v) {
                        rows.push((FieldRow { name: e.field.clone(), value: v.clone() }, e.op));
                    }
                }
            }
            Kind::Remove => {
                for v in &e.values {
                    rows.retain(|(r, op)| !(r.name == e.field && &r.value == v && *op < e.op));
                }
            }
        }
    }
    rows
}

/// The entries of `entries` naming `id`, still oldest first.
pub(crate) fn naming(entries: &[Listed], id: u32) -> Vec<&Listed> {
    entries.iter().filter(|e| e.ids.contains(id)).collect()
}

/// A record read as a query or a read sees it: what is stored of it, the
/// entries of its part applied. `read` counts the keys it reads.
pub(crate) fn read_record(
    t: &Tables,
    r: &RoTxn<'_>,
    id: u32,
    mass: bool,
    read: &mut dyn FnMut(u64) -> Result<()>,
) -> Result<Vec<FieldRow>> {
    read(1)?;
    let record = match t.metarecords.get(r, &id.to_be_bytes())? {
        Some(v) => Record::decode(v)?,
        None => Record::default(),
    };
    if !mass {
        return Ok(record.rows);
    }
    let entries = read_part(t, r, part_of(id))?;
    read(1 + entries.len() as u64)?;
    let named = naming(&entries, id);
    if named.is_empty() {
        return Ok(record.rows);
    }
    Ok(effective(&record, &named).into_iter().map(|(row, _)| row).collect())
}

/// Every dense id that has a record or is named by an entry (one that is
/// only named has no record yet).
pub(crate) fn record_ids(t: &Tables, r: &RoTxn<'_>, mass: bool) -> Result<RoaringBitmap> {
    let mut ids = RoaringBitmap::new();
    for entry in t.metarecords.iter(r)? {
        ids.insert(super::derived::dense(entry?.0));
    }
    if mass {
        for entry in t.mass.iter(r)? {
            ids |= Listed::decode(0, entry?.1)?.ids;
        }
    }
    Ok(ids)
}

/// Records read in id order, a part's list read once for all its records.
#[derive(Default)]
pub(crate) struct Parts {
    part: Option<u32>,
    entries: Vec<Listed>,
}

impl Parts {
    pub(crate) fn rows(
        &mut self,
        t: &Tables,
        r: &RoTxn<'_>,
        id: u32,
        mass: bool,
    ) -> Result<Vec<FieldRow>> {
        let record = match t.metarecords.get(r, &id.to_be_bytes())? {
            Some(v) => Record::decode(v)?,
            None => Record::default(),
        };
        if !mass {
            return Ok(record.rows);
        }
        if self.part != Some(part_of(id)) {
            self.entries = read_part(t, r, part_of(id))?;
            self.part = Some(part_of(id));
        }
        let named = naming(&self.entries, id);
        if named.is_empty() {
            return Ok(record.rows);
        }
        Ok(effective(&record, &named).into_iter().map(|(row, _)| row).collect())
    }
}

impl KvStore {
    /// How many entries are pending, in every part.
    pub fn pending_mass(&self) -> Result<usize> {
        let r = self.env.read_txn()?;
        // What a fold would write: the marks of records given back wait for a
        // write, not for a fold.
        let given_back = self.t.mass.prefix_iter(&r, &THAWED_PART.to_be_bytes())?.count();
        Ok((self.t.mass.len(&r)? as usize).saturating_sub(given_back))
    }

    /// Whether any part has entries: a read looks for them only if so.
    pub(super) fn mass_pending(&self) -> bool {
        self.mass.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// A record the transaction loaded: what is stored of it, the entries that
/// name it, and the rows it holds once they are applied.
pub(super) struct Loaded {
    pub(super) id: u32,
    pub(super) raw: Record,
    pub(super) entries: Vec<Listed>,
    pub(super) rows: Vec<(FieldRow, u64)>,
}

impl Loaded {
    /// The rows alone.
    pub(super) fn plain_rows(&self) -> Vec<FieldRow> {
        self.rows.iter().map(|(r, _)| r.clone()).collect()
    }
}

impl KvTxn<'_> {
    pub(super) fn mass_pending(&self) -> bool {
        self.mass_lists.get()
    }

    /// Whether records given back wait for their rows to be copied.
    pub(super) fn thawed_pending(&self) -> bool {
        self.thawed.get()
    }

    pub(super) fn deletes_pending(&self) -> bool {
        self.deletes.get()
    }

    /// The entries of `part`, as the transaction has them.
    fn part_entries(&self, part: u32) -> Result<Vec<Listed>> {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        read_part(&self.t, &self.txn.borrow(), part)
    }

    /// The record of dense id `id`, with the entries naming it applied.
    pub(super) fn load_record(&self, id: u32) -> Result<Loaded> {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let raw = match self.t.metarecords.get(&self.txn.borrow(), &id.to_be_bytes())? {
            Some(v) => Record::decode(v)?,
            None => Record::default(),
        };
        let entries = if self.mass_pending() {
            let part = self.part_entries(part_of(id))?;
            part.into_iter().filter(|e| e.ids.contains(id)).collect()
        } else {
            Vec::new()
        };
        let rows = if entries.is_empty() {
            raw.rows.iter().cloned().zip(raw.ops.iter().copied()).collect()
        } else {
            effective(&raw, &entries.iter().collect::<Vec<_>>())
        };
        Ok(Loaded { id, raw, entries, rows })
    }

    /// The number of a write to this record that an entry could be older
    /// than: a fresh one when an entry names the record, `0` when none can.
    pub(super) fn write_number(&self, loaded: &Loaded) -> Result<u64> {
        if loaded.entries.is_empty() {
            Ok(0)
        } else {
            Ok(self.next(State::NextWriteNumber)? as u64)
        }
    }

    /// Writes a loaded record's rows back, `tombs` and `cleared` being what
    /// the write adds to what it carried. A record no entry names is written
    /// plain.
    pub(super) fn save_record(
        &self,
        loaded: &Loaded,
        rows: Vec<(FieldRow, u64)>,
        tombs: Vec<(String, Value, u64)>,
        cleared: Vec<(String, u64)>,
    ) -> Result<()> {
        // A record a deletion gave back keeps the rows it took, in the log,
        // before the first write that changes them.
        self.freeze_given_back(&RoaringBitmap::from_iter([loaded.id]))?;
        let record = if loaded.entries.is_empty() {
            Record::plain(rows.into_iter().map(|(r, _)| r).collect())
        } else {
            let mut record = Record {
                ops: rows.iter().map(|(_, op)| *op).collect(),
                rows: rows.into_iter().map(|(r, _)| r).collect(),
                tombs: loaded.raw.tombs.clone(),
                cleared: loaded.raw.cleared.clone(),
            };
            for t in tombs {
                record.tombs.retain(|(n, v, _)| !(*n == t.0 && *v == t.1));
                record.tombs.push(t);
            }
            for c in cleared {
                record.tombs.retain(|(n, _, _)| *n != c.0);
                record.cleared.retain(|(n, _)| *n != c.0);
                record.cleared.push(c);
            }
            record
        };
        let mut w = self.txn.borrow_mut();
        self.t.metarecords.put(&mut w, &loaded.id.to_be_bytes(), &record.encode())?;
        Ok(())
    }

    /// Appends the entries of a write to many records: one per part its ids
    /// are in. There is no limit to a list: the idle fold writes it in (doc
    /// "Folding in idle time").
    pub(super) fn append_entries(
        &self,
        kind: Kind,
        field: &str,
        values: &[Value],
        ids: &RoaringBitmap,
    ) -> Result<()> {
        // An entry changes the rows of the records it names: what the log
        // reads of a deletion in a record given back is copied first.
        self.freeze_given_back(ids)?;
        let op = self.next(State::NextWriteNumber)? as u64;
        let mut parts = BTreeSet::new();
        for id in ids {
            parts.insert(part_of(id));
        }
        for &part in &parts {
            let lo = part << PART_BITS;
            let hi = lo | ((1 << PART_BITS) - 1);
            let mut in_part = RoaringBitmap::new();
            in_part.insert_range(lo..=hi);
            in_part &= ids;
            let entry = Listed {
                op,
                kind,
                field: field.to_string(),
                values: values.to_vec(),
                ids: in_part,
            };
            self.t.mass.put(&mut self.txn.borrow_mut(), &entry_key(part, op), &entry.encode())?;
        }
        self.mass_lists.set(true);
        Ok(())
    }

    /// Writes a part's entries into its records, and empties the list. A
    /// metarecord that is gone is not written back.
    pub(super) fn fold_part(&self, part: u32) -> Result<()> {
        if part == DELETES_PART || part == THAWED_PART {
            return Ok(());
        }
        let entries = self.part_entries(part)?;
        if entries.is_empty() {
            return Ok(());
        }
        let mut ids = RoaringBitmap::new();
        for e in &entries {
            ids |= &e.ids;
        }
        // A deleted record is folded too: its record says what the entries it
        // keeps in the indexes describe, which a restoration relies on.
        for id in ids {
            let raw = match self.t.metarecords.get(&self.txn.borrow(), &id.to_be_bytes())? {
                Some(v) => Record::decode(v)?,
                None => Record::default(),
            };
            let named = naming(&entries, id);
            let rows: Vec<FieldRow> =
                effective(&raw, &named).into_iter().map(|(row, _)| row).collect();
            self.put_record(id, &rows)?;
        }
        let mut w = self.txn.borrow_mut();
        for e in &entries {
            self.t.mass.delete(&mut w, &entry_key(part, e.op))?;
        }
        Ok(())
    }

    /// [`Self::fold_part`] for every part with entries.
    pub(super) fn fold_all_mass(&self) -> Result<()> {
        // The deletions first: they read the records of the metarecords they
        // took out with the entries still applied, which a part's fold would
        // leave behind for a record that is gone.
        while self.fold_one_delete()? {}
        let mut parts = BTreeSet::new();
        for entry in self.t.mass.iter(&self.txn.borrow())? {
            let (k, _) = entry?;
            parts.insert(u32::from_be_bytes(k[..4].try_into().expect("a part")));
        }
        for part in parts {
            self.fold_part(part)?;
        }
        Ok(())
    }
}
