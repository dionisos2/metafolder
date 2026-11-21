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
//! keeps — while an entry names it — when it was written to by hand, under the
//! number of the write (doc "Mass writes in the store"):
//!
//! | in a record                  | says                                         |
//! |------------------------------|----------------------------------------------|
//! | a fixed field (f, n)         | an entry older than `n` does not touch `f`   |
//! | a fixed value (f, v, n)      | an entry older than `n` does not touch `v` of `f` |
//!
//! | table | key           | value                                |
//! |-------|---------------|--------------------------------------|
//! | mass  | part · number | field · kind · values · ids of part  |
//!
//! A record's value is its rows, as ever, then — only when an entry names it —
//! its fixed fields and values; one without reads as before.

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
    /// Fields set or emptied by hand, after the entries older than the number.
    pub(crate) fixed_fields: Vec<(String, u64)>,
    /// Values added or taken off by hand, after the entries older than the
    /// number: whether the value is there, the rows say.
    pub(crate) fixed_values: Vec<(String, Value, u64)>,
}

impl Record {
    /// Rows no entry can override.
    pub(crate) fn plain(rows: Vec<FieldRow>) -> Record {
        Record { rows, fixed_fields: Vec::new(), fixed_values: Vec::new() }
    }

    /// `rows` ([`codec::put_rows`]), then — only when it has any — `varint
    /// count · { varint len · field · varint number } · varint count · {
    /// varint len · field · value · varint number }`, the value with its type
    /// (a value taken off may have no row left to say it).
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Enc::default();
        codec::put_rows(&mut out, &self.rows);
        if !self.fixed_fields.is_empty() || !self.fixed_values.is_empty() {
            out.varint(self.fixed_fields.len() as u64);
            for (field, n) in &self.fixed_fields {
                out.bytes(field.as_bytes()).varint(*n);
            }
            out.varint(self.fixed_values.len() as u64);
            for (field, value, n) in &self.fixed_values {
                out.bytes(field.as_bytes()).value(value).varint(*n);
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
            let field = r.string()?;
            record.fixed_fields.push((field, r.varint()?));
        }
        for _ in 0..r.varint()? {
            let field = r.string()?;
            let value = r.value()?;
            record.fixed_values.push((field, value, r.varint()?));
        }
        if !r.is_empty() {
            bail!("a record of trailing bytes");
        }
        Ok(record)
    }

    /// A field set or emptied by hand at `n`: what was fixed of its values
    /// before says nothing more.
    pub(crate) fn fix_field(&mut self, field: &str, n: u64) {
        self.fixed_values.retain(|(f, _, _)| f != field);
        self.fixed_fields.retain(|(f, _)| f != field);
        self.fixed_fields.push((field.to_string(), n));
    }

    /// A value added or taken off by hand at `n`.
    pub(crate) fn fix_value(&mut self, field: &str, value: &Value, n: u64) {
        self.fixed_values.retain(|(f, v, _)| !(f == field && v == value));
        self.fixed_values.push((field.to_string(), value.clone(), n));
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
        let before = self.rows.len();
        self.rows.retain(|r| !(matches!(r.value, Value::Nothing) && valued.contains(&r.name)));
        self.rows.len() != before
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
/// are applied: an entry does not touch a field, or a value, the record says
/// was written to by hand after it.
pub(crate) fn effective(record: &Record, entries: &[&Listed]) -> Vec<FieldRow> {
    let mut rows = record.rows.clone();
    for e in entries {
        if record.fixed_fields.iter().any(|(f, n)| *f == e.field && *n > e.op) {
            continue;
        }
        let fixed = |value: &Value| {
            record.fixed_values.iter().any(|(f, v, n)| *f == e.field && v == value && *n > e.op)
        };
        let holds = |rows: &[FieldRow], value: &Value| {
            rows.iter().any(|r| r.name == e.field && &r.value == value)
        };
        match e.kind {
            Kind::Add | Kind::Replace => {
                if e.kind == Kind::Replace {
                    rows.retain(|r| r.name != e.field || fixed(&r.value));
                }
                for v in &e.values {
                    if !fixed(v) && !holds(&rows, v) {
                        rows.push(FieldRow { name: e.field.clone(), value: v.clone() });
                    }
                }
            }
            Kind::Remove => {
                for v in e.values.iter().filter(|v| !fixed(v)) {
                    rows.retain(|r| !(r.name == e.field && &r.value == v));
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
    Ok(effective(&record, &named))
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
        Ok(effective(&record, &named))
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
    pub(super) rows: Vec<FieldRow>,
}

impl Loaded {
    /// The rows it holds.
    pub(super) fn plain_rows(&self) -> Vec<FieldRow> {
        self.rows.clone()
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
            raw.rows.clone()
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

    /// Writes a loaded record's rows back, `fixed` being what the write
    /// wrote by hand under its number: whole fields (no value), or values. A
    /// record no entry names is written plain.
    pub(super) fn save_record(
        &self,
        loaded: &Loaded,
        rows: Vec<FieldRow>,
        n: u64,
        fixed: &[(&str, Option<&Value>)],
    ) -> Result<()> {
        // A record a deletion gave back keeps the rows it took, in the log,
        // before the first write that changes them.
        self.freeze_given_back(&RoaringBitmap::from_iter([loaded.id]))?;
        let record = if loaded.entries.is_empty() {
            Record::plain(rows)
        } else {
            let mut record = Record { rows, ..loaded.raw.clone() };
            for (field, value) in fixed {
                match value {
                    Some(v) => record.fix_value(field, v, n),
                    None => record.fix_field(field, n),
                }
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
            self.put_record(id, &effective(&raw, &named))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, v: i64) -> FieldRow {
        FieldRow { name: name.into(), value: Value::Int(v) }
    }

    fn entry(op: u64, kind: Kind, field: &str, values: &[i64]) -> Listed {
        Listed {
            op,
            kind,
            field: field.into(),
            values: values.iter().map(|v| Value::Int(*v)).collect(),
            ids: [1].into_iter().collect(),
        }
    }

    fn applied(record: &Record, entries: &[Listed]) -> Vec<FieldRow> {
        let mut rows = effective(record, &entries.iter().collect::<Vec<_>>());
        rows.sort_by_key(|r| format!("{r:?}"));
        rows
    }

    #[test]
    fn a_record_comes_back_with_its_fixed_fields_and_values() {
        let plain = Record::plain(vec![row("a", 1), row("b", 2)]);
        assert_eq!(Record::decode(&plain.encode()).unwrap(), plain);
        let fixed = Record {
            rows: vec![row("a", 1)],
            fixed_fields: vec![("b".into(), 7)],
            fixed_values: vec![("a".into(), Value::Int(1), 9), ("c".into(), Value::Int(3), 4)],
        };
        assert_eq!(Record::decode(&fixed.encode()).unwrap(), fixed);
        // A record no entry names is its rows alone.
        let mut rows = Enc::default();
        codec::put_rows(&mut rows, &plain.rows);
        assert_eq!(plain.encode(), rows.0);
    }

    #[test]
    fn an_entry_older_than_a_fixed_field_does_not_touch_it() {
        let record = Record {
            rows: vec![row("a", 1)],
            fixed_fields: vec![("a".into(), 5)],
            fixed_values: Vec::new(),
        };
        let older = [entry(3, Kind::Add, "a", &[2]), entry(4, Kind::Replace, "a", &[])];
        assert_eq!(applied(&record, &older), vec![row("a", 1)]);
        let newer = [entry(6, Kind::Add, "a", &[2])];
        assert_eq!(applied(&record, &newer), vec![row("a", 1), row("a", 2)]);
    }

    #[test]
    fn an_entry_older_than_a_fixed_value_does_not_touch_that_value() {
        // 1 added by hand at 5, 2 removed by hand at 5.
        let record = Record {
            rows: vec![row("a", 1), row("a", 3)],
            fixed_fields: Vec::new(),
            fixed_values: vec![("a".into(), Value::Int(1), 5), ("a".into(), Value::Int(2), 5)],
        };
        let older = [entry(2, Kind::Add, "a", &[2]), entry(3, Kind::Remove, "a", &[1])];
        assert_eq!(applied(&record, &older), vec![row("a", 1), row("a", 3)]);
        // A replacement older than both takes the rest, not what was fixed.
        let set = [entry(4, Kind::Replace, "a", &[2, 9])];
        assert_eq!(applied(&record, &set), vec![row("a", 1), row("a", 9)]);
        // Newer: everything applies.
        let set = [entry(6, Kind::Replace, "a", &[2])];
        assert_eq!(applied(&record, &set), vec![row("a", 2)]);
    }
}
