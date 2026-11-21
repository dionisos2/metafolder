//! The writes to many records in the log (doc "Mass operations in the log"):
//! one entry, of the one form every entry has (doc "Log storage").
//!
//! A write of one value to, or off, a field of N records — or of the rows
//! that replace the field's, or a deletion of N records — is logged as one
//! entry of N targets: the field, the value or values, and the records that
//! changed, by uuid, in the order of their dense ids. It holds the N operation
//! ids `first` to `first + N - 1`, and what reads the log is given the
//! operations a write to each record alone would have logged. It is listed
//! under the nil uuid in `log_entries_by_metarecord`, which no metarecord
//! has: a record's operations after an id are its own and those of the
//! entries listed there that name it.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use metafolder_core::metarecord::Value;
use roaring::RoaringBitmap;
use uuid::Uuid;

use super::derived::{
    count_prefix, decode_set, dense, referrers_prefix, set_prefix, COUNT_BITS, NO_TARGET, PARENTS,
    REFERRERS, SETS,
};
use super::entry::{self, Effect, Entry, FileAction, Header, Params, CHUNK};
use super::mass::{entry_key, read_part, Listed, DELETES_PART, THAWED_PART};
use super::{be, key, name_key, KvTxn, Read, ROOT};
use crate::rows::FieldRow;
use crate::store::{MassApplied, MassKind as Kind};

/// A record of a folded deletion as it is put back: its dense id, its uuid, and
/// the rows the log froze.
type Folded = (u32, Uuid, Vec<FieldRow>);

/// An operation of an entry, as a write to many records reads it back: its
/// target, the target's dense id (none when the store has none for
/// it), whether its before is still in the record, and its before otherwise.
struct Target {
    uuid: Uuid,
    id: Option<u32>,
    pending: bool,
    before: Vec<FieldRow>,
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

    /// Logs a write to `field` of the records `ids` (live ones) as one entry,
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
        // Nothing may name a deleted record: what it left in the indexes
        // would come back to life with it (doc "Deleting a metarecord in the
        // store").
        let universe = self.read_chunked(super::derived::LIVE, &[])?;
        let ids = &(ids & &universe);
        let mut before = Vec::new();
        let changed = match kind {
            Kind::Add | Kind::Remove => {
                let value = &values[0];
                // A `Nothing` is held where the field's `absent` set says.
                let have = match super::derived::value_key(value) {
                    Some(key) => self.effective(field, value, &key)?,
                    None => {
                        self.read_chunked(SETS, &set_prefix(super::derived::ABSENT, Some(field)))?
                    }
                };
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
        let (effect, params) = match kind {
            Kind::Add => (Effect::Add, Params::Value(field.to_string(), values[0].clone())),
            Kind::Remove => (Effect::Remove, Params::Value(field.to_string(), values[0].clone())),
            Kind::Replace if values.is_empty() => (Effect::Unset, Params::Field(field.to_string())),
            Kind::Replace => (Effect::Set, Params::Values(field.to_string(), values.to_vec())),
        };
        let h = Header {
            context: label,
            timestamp,
            origin: origin.map(str::to_string),
            effect,
            file: FileAction::None,
            reverts,
            params,
            targets: changed.len(),
        };
        // What a replacement took off, each value with the ranks of the
        // targets that held it: a replacement forgets what it replaces, so
        // the log is told.
        let mut held: Vec<Vec<FieldRow>> = Vec::new();
        if !before.is_empty() {
            held = vec![Vec::new(); changed.len() as usize];
            for (value, ranks) in before {
                for k in ranks {
                    held[k as usize]
                        .push(FieldRow { name: field.to_string(), value: value.clone() });
                }
            }
        }
        let targets = self.uuids_of_ids(&changed)?;
        let chunks = entry::chunks(&targets, false, |k| {
            held.get_mut(k).map(std::mem::take).unwrap_or_default()
        });
        let first = self.put_entry(&h, &chunks, true)?;
        let last = first + changed.len() as i64 - 1;
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
    pub(super) fn fields_held(&self) -> Result<(Vec<String>, Vec<String>)> {
        let txn = self.txn.borrow();
        let (mut all, mut positional) = (Vec::<String>::new(), Vec::<String>::new());
        for e in self.t.field_types.iter(&txn)? {
            let (name, ty) = super::derived::unesc(e?.0)?;
            let name = String::from_utf8(name).context("a field name")?;
            if super::codec::is_position_type(ty) {
                positional.push(name.clone());
            }
            if all.last() != Some(&name) {
                all.push(name);
            }
        }
        positional.dedup();
        Ok((all, positional))
    }

    /// Deletes the records `ids` as one entry (doc "Mass operations in the
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
        let universe = self.read_chunked(super::derived::LIVE, &[])?;
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
        self.chunk_apply(super::derived::LIVE, &[], &changed, false)?;
        let h = Header {
            context: label,
            timestamp,
            origin: origin.map(str::to_string),
            effect: Effect::Delete,
            file: FileAction::None,
            reverts: None,
            params: Params::None,
            targets: changed.len(),
        };
        // The before of every target is in its record, which the deletion
        // keeps as it was until the fold copies it into the chunks.
        let targets = self.uuids_of_ids(&changed)?;
        let first = self.put_entry(&h, &entry::chunks(&targets, true, |_| Vec::new()), true)?;
        let last = first + changed.len() as i64 - 1;
        let entry = Listed {
            op: first as u64,
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
        self.mass_lists.set(true);
        self.deletes.set(true);
        Ok(Some(MassApplied { changed, last_op: last, touches_watch }))
    }

    /// The fields with data under set kind `kind` (counts, referrers): the
    /// names the keys of that kind begin with.
    fn fields_with_sets(&self, kind: u8) -> Result<Vec<String>> {
        let txn = self.txn.borrow();
        let mut out: Vec<String> = Vec::new();
        for e in self.t.sets().prefix_iter(&txn, &[kind])? {
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
    /// operations `first_op..=last_op`, which are a write to many records',
    /// are taken back by an entry of the opposite effect over the same
    /// records, each of whose operations names the one it reverts. `None` —
    /// nothing written — when they are not so (not one write to many records,
    /// a replacement, a record the store no longer numbers as it did) or when
    /// a record no longer holds what the revert would change.
    pub(super) fn mass_revert_log(
        &self,
        first_op: i64,
        last_op: i64,
        label: i64,
        timestamp: i64,
        origin: Option<&str>,
    ) -> Result<Option<MassApplied>> {
        let Some(entry) = self.listed_entry(first_op, last_op)? else { return Ok(None) };
        let (Effect::Add | Effect::Remove, Params::Value(field, value)) =
            (entry.h.effect, &entry.h.params)
        else {
            return Ok(None);
        };
        let Some(ids) = self.ids_in_order(&self.targets_of_ops(&entry, first_op, last_op)?) else {
            return Ok(None);
        };
        // A record deleted since is out of the universe: the revert by ids
        // would miss it, and say it did it.
        let universe = self.read_chunked(super::derived::LIVE, &[])?;
        if !ids.is_subset(&universe) {
            return Ok(None);
        }
        let opposite = if entry.h.effect == Effect::Add { Kind::Remove } else { Kind::Add };
        self.mass_log(
            opposite,
            field,
            std::slice::from_ref(value),
            &ids,
            label,
            timestamp,
            origin,
            Some(first_op),
        )
    }

    /// The write to many records that holds the operations
    /// `first_op..=last_op`, when one does.
    fn listed_entry(&self, first_op: i64, last_op: i64) -> Result<Option<Entry>> {
        let Some(entry) = self.read_log(|r| r.entry_at(first_op))? else { return Ok(None) };
        if !entry.covers(last_op) {
            return Ok(None);
        }
        let listed = {
            let txn = self.txn.borrow();
            let k = key(&[ROOT.as_slice(), &be(entry.first)]);
            self.t.log_entries_by_metarecord.get(&txn, &k)?.is_some()
        };
        Ok(listed.then_some(entry))
    }

    /// The operations `first_op..=last_op` of `entry`, as they are stored.
    fn targets_of_ops(&self, entry: &Entry, first_op: i64, last_op: i64) -> Result<Vec<Target>> {
        let mut out = Vec::with_capacity((last_op - first_op + 1) as usize);
        let mut loaded = entry::Loaded::default();
        for op in first_op..=last_op {
            let (uuid, pending, before) = self.read_log(|r| {
                let (chunk, rank) = r.chunk_of(entry, op, &mut loaded)?;
                Ok((
                    chunk.targets[rank],
                    chunk.pending.contains(rank as u32),
                    chunk.before[rank].clone(),
                ))
            })?;
            let id = self.dense_id(uuid.as_bytes())?;
            out.push(Target { uuid, id, pending, before });
        }
        Ok(out)
    }

    /// The dense ids of `targets`, when the store numbers each of them, in
    /// their order: a write to many records names its targets in the order of
    /// their ids, and an operation of a revert or an undeletion in the place of
    /// the one it takes back.
    fn ids_in_order(&self, targets: &[Target]) -> Option<RoaringBitmap> {
        let mut ids = RoaringBitmap::new();
        for t in targets {
            let id = t.id?;
            if ids.max().is_some_and(|m| m >= id) {
                return None;
            }
            ids.insert(id);
        }
        Some(ids)
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
        let Some(entry) = self.listed_entry(first_op, last_op)?.filter(|e| e.deletes()) else {
            return Ok(None);
        };
        let targets = self.targets_of_ops(&entry, first_op, last_op)?;
        // The fold wrote the records out of the store when the chunks hold
        // their rows: they are put back from there. Some only: not as a whole.
        let folded = !targets.iter().any(|t| t.pending);
        if !folded && !targets.iter().all(|t| t.pending) {
            return Ok(None);
        }
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
        let n = (last_op - first_op + 1) as usize;
        let Some(ids) = self.ids_in_order(&targets) else { return Ok(None) };
        let universe = self.read_chunked(super::derived::LIVE, &[])?;
        if ids.len() != n as u64 || !(&ids & &universe).is_empty() {
            return Ok(None);
        }
        // What the fold took out of the store: the records, from the rows the
        // log froze. Each must still be the one it was — its id its uuid's, no
        // record under either — and a position it held not taken since.
        let revived = if folded { self.folded_records(&targets)? } else { None };
        if folded && revived.is_none() {
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
            // Every row of the field is gone, and its type with it: the rows
            // being given back tell it.
            let types = if types.is_empty() {
                let mut found: std::collections::BTreeSet<Vec<u8>> = Default::default();
                for id in ids.iter() {
                    let rows = match revived.as_ref().and_then(|r| r.iter().find(|f| f.0 == id)) {
                        Some((_, _, rows)) => rows.clone(),
                        None => self.load_record(id)?.plain_rows(),
                    };
                    for row in rows.iter().filter(|r| {
                        r.name == field
                            && !matches!(r.value, metafolder_core::metarecord::Value::Nothing)
                    }) {
                        found.insert(vec![super::codec::type_code(&row.value)]);
                    }
                }
                found.into_iter().collect()
            } else {
                types
            };
            if types.is_empty() {
                return Ok(None);
            }
            counts.push((field, types, back));
        }
        let alive = &universe | &ids;
        // The parents of the positions given back: an ordinary reference is
        // no position, and its target no parent, alive or not.
        let mut parents: Vec<(String, u32)> = Vec::new();
        // A field all of whose rows were deleted has no type left: the rows
        // given back told it above.
        let (typed, mut positional) = self.fields_held()?;
        for (field, types, _) in &counts {
            let placed = types.iter().any(|t| super::codec::is_position_type(t));
            if placed && !typed.contains(field) {
                positional.push(field.clone());
            }
        }
        for field in positional {
            let prefix = set_prefix(REFERRERS, Some(&field));
            let mut groups: BTreeMap<u32, RoaringBitmap> = BTreeMap::new();
            {
                let txn = self.txn.borrow();
                for e in self.t.sets().prefix_iter(&txn, &prefix)? {
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
        for (id, _, rows) in revived.iter().flatten() {
            // Not as plain rows: entries of a write to many records may still
            // name the id, older than the rows the deletion took.
            self.put_given_back(*id, rows)?;
            for row in rows {
                self.add_hist(&row.name, &row.value)?;
                self.file_position(&row.name, &row.value, *id)?;
            }
        }
        self.chunk_apply(super::derived::LIVE, &[], &ids, true)?;
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
        // The records are back with the rows the deletion took, which the log
        // goes on reading in them until the first write that changes them.
        let given_back = Listed {
            op: first_op as u64,
            kind: Kind::Remove,
            field: String::new(),
            values: Vec::new(),
            ids: ids.clone(),
        };
        self.t.mass.put(&mut w, &entry_key(THAWED_PART, given_back.op), &given_back.encode())?;
        self.thawed.set(true);
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
    /// operations `targets` of it: each `(id, uuid, rows)`, the rows being
    /// those the log froze. `None` when one is not as the fold left it: a
    /// record stored under its id (given back), an id that is no longer its
    /// uuid's, or a position a live record took since.
    fn folded_records(&self, targets: &[Target]) -> Result<Option<Vec<Folded>>> {
        let mut out = Vec::with_capacity(targets.len());
        let txn = self.txn.borrow();
        for t in targets {
            let Some(id) = t.id else { return Ok(None) };
            let named = self.t.uuids.get(&txn, &id.to_be_bytes())?;
            if named != Some(t.uuid.as_bytes().as_slice()) || self.live_in(&txn, id)? {
                return Ok(None);
            }
            for row in &t.before {
                let Some(k) = super::derived::raw_key_of(&self.t, &txn, &row.name, &row.value)?
                else {
                    continue;
                };
                if let Some(v) = self.t.index_raw_holder.get(&txn, &k)? {
                    let holder = super::derived::dense(v);
                    if holder != id && self.live_in(&txn, holder)? {
                        return Ok(None);
                    }
                }
            }
            out.push((id, t.uuid, t.before.clone()));
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
            for e in self.t.sets().prefix_iter(&txn, &prefix)? {
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
    /// out of the store, after their rows went to the chunks of its
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
            self.deletes.set(false);
            return Ok(false);
        };
        // A record about to leave the store may be one an earlier deletion
        // gave back: that deletion's rows go to the chunks first.
        self.freeze_given_back(&listed.ids)?;
        let first = listed.op as i64;
        let mut befores = Vec::new();
        for (rank, id) in listed.ids.iter().enumerate() {
            let rows = self.load_record(id)?.plain_rows();
            for row in &rows {
                self.drop_hist(&row.name, &row.value)?;
            }
            // An operation a prune took has nothing to keep.
            befores.push((first + rank as i64, rows));
            if befores.len() as u64 == CHUNK {
                self.write_befores(std::mem::take(&mut befores))?;
            }
        }
        self.write_befores(befores)?;
        self.t.mass.delete(&mut self.txn.borrow_mut(), &entry_key_bytes)?;
        Ok(true)
    }

    /// Copies into the chunks of the log the rows the records of `ids` held
    /// that a deletion took and gave back, which the log reads in the records until
    /// now: the entry, the write, the deletion about to change them. Nothing
    /// is read when no record is waiting.
    pub(super) fn freeze_given_back(&self, ids: &RoaringBitmap) -> Result<()> {
        if ids.is_empty() || !self.thawed_pending() {
            return Ok(());
        }
        let marks = {
            let txn = self.txn.borrow();
            read_part(&self.t, &txn, THAWED_PART)?
        };
        let mut waiting = false;
        for mark in marks {
            let hit = &mark.ids & ids;
            if hit.is_empty() {
                waiting = true;
                continue;
            }
            let mut befores = Vec::new();
            for id in hit.iter() {
                let op = mark.op as i64 + (mark.ids.rank(id) - 1) as i64;
                befores.push((op, self.load_record(id)?.plain_rows()));
            }
            // An operation a prune took has nothing to keep.
            self.write_befores(befores)?;
            // The mark keeps the operations still waiting, each at its number.
            let mut w = self.txn.borrow_mut();
            self.t.mass.delete(&mut w, &entry_key(THAWED_PART, mark.op))?;
            let mut run: Vec<u32> = Vec::new();
            let mut start = 0usize;
            let flush = |run: &mut Vec<u32>, start: usize, w: &mut _| -> Result<bool> {
                if run.is_empty() {
                    return Ok(false);
                }
                let rest = Listed {
                    op: mark.op + start as u64,
                    kind: Kind::Remove,
                    field: String::new(),
                    values: Vec::new(),
                    ids: run.drain(..).collect(),
                };
                self.t.mass.put(w, &entry_key(THAWED_PART, rest.op), &rest.encode())?;
                Ok(true)
            };
            for (rank, id) in mark.ids.iter().enumerate() {
                if hit.contains(id) {
                    waiting |= flush(&mut run, start, &mut w)?;
                } else {
                    if run.is_empty() {
                        start = rank;
                    }
                    run.push(id);
                }
            }
            waiting |= flush(&mut run, start, &mut w)?;
        }
        if !waiting {
            self.thawed.set(false);
        }
        Ok(())
    }

    /// [`Self::freeze_given_back`] for every record waiting: a reindex is
    /// about to number the ids again, which the marks name them by.
    pub(super) fn freeze_all_given_back(&self) -> Result<()> {
        if !self.thawed_pending() {
            return Ok(());
        }
        let mut all = RoaringBitmap::new();
        for mark in read_part(&self.t, &self.txn.borrow(), THAWED_PART)? {
            all |= &mark.ids;
        }
        self.freeze_given_back(&all)
    }
}
