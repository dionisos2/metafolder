//! An entry of the log in its one form (doc "Log storage"): a write on a list
//! of metarecords, its targets — an individual operation when there is one, a
//! mass operation when there are many —, one operation per target. The header is the value of its first
//! operation id in `log_entries`; the targets and their before are in
//! `log_targets`, in chunks of [`CHUNK`] targets.

use anyhow::{bail, ensure, Context, Result};
use metafolder_core::metarecord::Value;
use roaring::RoaringBitmap;
use uuid::Uuid;

use super::codec::{self, Dec, Enc};
use super::{KvTxn, Read};
use crate::log::OpRow;
use crate::rows::FieldRow;
use crate::store::NewOp;

/// The targets a chunk of `log_targets` holds, the last one excepted.
pub(crate) const CHUNK: u64 = 4096;

/// About how many targets a step of the idle fold indexes in
/// `log_entries_by_metarecord`.
pub(crate) const INDEX_BUDGET: u64 = 64 * 1024;

/// What an entry does to each of its targets (doc "Log storage").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Effect {
    Create,
    Delete,
    SetRecord,
    Add,
    Remove,
    Set,
    Unset,
    /// A write no client asked for and none can undo.
    Unknown,
}

/// What an undo of the entry does to the files (doc "Filesystem
/// coordination").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FileAction {
    None,
    Deleted,
    Moved,
    Modified,
}

/// The parameters of an effect: what it wrote, the same for every target.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Params {
    /// `create`, `delete`, `unknown`.
    None,
    /// `set_record`: every row.
    Rows(Vec<FieldRow>),
    /// `add`, `remove`: a field and a value.
    Value(String, Value),
    /// `set`: a field and its values, one or more.
    Values(String, Vec<Value>),
    /// `unset`: a field.
    Field(String),
}

/// The header of an entry, the value of its first operation id in
/// `log_entries`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Header {
    /// The innermost log context of the write.
    pub(crate) context: i64,
    /// Unix milliseconds.
    pub(crate) timestamp: i64,
    pub(crate) origin: Option<String>,
    pub(crate) effect: Effect,
    pub(crate) file: FileAction,
    /// For an entry a revert wrote: operation `first + k` reverts `reverts + k`.
    pub(crate) reverts: Option<i64>,
    pub(crate) params: Params,
    /// How many targets, the operation ids `first..first + targets`.
    pub(crate) targets: u64,
}

/// A chunk of `log_targets`: its targets, and what each held before the
/// operation. The before of a target of a deletion of many records may still
/// be in its record, which the pending deletion keeps as it was: its rank is
/// then `pending`, and its before here empty (doc "Mass writes in the store").
///
/// A write to many records names its targets by dense id (`ids`), in their
/// order, and reads no uuid: the idle fold names them by uuid later, and a
/// reindex first (doc "Log storage"). A chunk read so has its `targets` read
/// from the ids; written, it keeps the ids while they are there.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Chunk {
    pub(crate) targets: Vec<Uuid>,
    pub(crate) before: Vec<Vec<FieldRow>>,
    pub(crate) pending: RoaringBitmap,
    pub(crate) ids: Option<RoaringBitmap>,
}

/// Added to the count of targets at the head of a chunk that names them by
/// dense id: a count of uuids is at most [`CHUNK`].
const BY_ID: u64 = 1 << 20;

const EFFECTS: [(Effect, &str); 8] = [
    (Effect::Create, "create_metarecord"),
    (Effect::Delete, "delete_metarecord"),
    (Effect::SetRecord, "set_metarecord"),
    (Effect::Add, "append_field"),
    (Effect::Remove, "delete_field"),
    (Effect::Set, "set_field"),
    (Effect::Unset, "delete_field"),
    (Effect::Unknown, "unknown"),
];

const FILE_ACTIONS: [(FileAction, &str); 4] = [
    (FileAction::None, ""),
    (FileAction::Deleted, "file_deleted"),
    (FileAction::Moved, "file_moved"),
    (FileAction::Modified, "file_modified"),
];

/// A one-byte code: its place in its table.
fn code_of<T: PartialEq>(table: &[(T, &str)], v: &T) -> u8 {
    table.iter().position(|(t, _)| t == v).expect("every variant in its table") as u8
}

fn of_code<T: Copy>(table: &[(T, &str)], code: u8, what: &str) -> Result<T> {
    match table.get(code as usize) {
        Some((t, _)) => Ok(*t),
        None => bail!("a log entry of {what} code {code}"),
    }
}

/// A non-negative number as a varint.
fn natural(out: &mut Enc, n: i64, what: &str) -> Result<()> {
    ensure!(n >= 0, "a log entry of {what} {n}");
    out.varint(n as u64);
    Ok(())
}

fn natural_of(r: &mut Dec, what: &str) -> Result<i64> {
    let n = r.varint()?;
    i64::try_from(n).map_err(|_| anyhow::anyhow!("a log entry of {what} {n}"))
}

impl Header {
    /// `varint context · zigzag timestamp · origin · effect · file action ·
    /// varint (first reverted + 1, 0 for none) · parameters · varint targets`.
    pub(crate) fn encode(&self) -> Result<Vec<u8>> {
        let mut out = Enc::default();
        natural(&mut out, self.context, "context")?;
        out.zigzag(self.timestamp);
        out.raw(&[
            codec::origin_byte(self.origin.as_deref())?,
            code_of(&EFFECTS, &self.effect),
            code_of(&FILE_ACTIONS, &self.file),
        ]);
        match self.reverts {
            Some(r) => {
                ensure!(r >= 0, "a log entry reverting op {r}");
                out.varint(r as u64 + 1);
            }
            None => {
                out.varint(0);
            }
        }
        match (self.effect, &self.params) {
            (Effect::Create | Effect::Delete | Effect::Unknown, Params::None) => {}
            (Effect::SetRecord, Params::Rows(rows)) => {
                codec::put_rows(&mut out, rows);
            }
            (Effect::Add | Effect::Remove, Params::Value(field, value)) => {
                out.bytes(field.as_bytes()).value(value);
            }
            (Effect::Set, Params::Values(field, values)) if !values.is_empty() => {
                out.bytes(field.as_bytes()).varint(values.len() as u64);
                for v in values {
                    out.value(v);
                }
            }
            (Effect::Unset, Params::Field(field)) => {
                out.bytes(field.as_bytes());
            }
            (effect, params) => bail!("a log entry {effect:?} of parameters {params:?}"),
        }
        out.varint(self.targets);
        Ok(out.0)
    }

    pub(crate) fn decode(b: &[u8]) -> Result<Header> {
        let mut r = Dec(b);
        let context = natural_of(&mut r, "context")?;
        let timestamp = r.zigzag()?;
        let [origin, effect, file] = r.take(3)? else { unreachable!("three bytes") };
        let origin = codec::origin_of(*origin)?;
        let effect = of_code(&EFFECTS, *effect, "effect")?;
        let file = of_code(&FILE_ACTIONS, *file, "file action")?;
        let reverts = match natural_of(&mut r, "reverted op")? {
            0 => None,
            n => Some(n - 1),
        };
        let params = match effect {
            Effect::Create | Effect::Delete | Effect::Unknown => Params::None,
            Effect::SetRecord => Params::Rows(codec::get_rows(&mut r)?),
            Effect::Add | Effect::Remove => Params::Value(r.string()?, r.value()?),
            Effect::Set => {
                let field = r.string()?;
                let n = r.varint()?;
                ensure!(n > 0, "a log entry setting no value");
                let values = (0..n).map(|_| r.value()).collect::<Result<_>>()?;
                Params::Values(field, values)
            }
            Effect::Unset => Params::Field(r.string()?),
        };
        let targets = r.varint()?;
        ensure!(r.is_empty(), "a log entry of trailing bytes");
        Ok(Header { context, timestamp, origin, effect, file, reverts, params, targets })
    }

    /// The type the wire names each operation of the entry by: the file
    /// action when there is one, else the effect's.
    pub(crate) fn wire_type(&self) -> &'static str {
        match self.file {
            FileAction::None => EFFECTS[code_of(&EFFECTS, &self.effect) as usize].1,
            file => FILE_ACTIONS[code_of(&FILE_ACTIONS, &file) as usize].1,
        }
    }

    /// The field the entry writes, when it writes one.
    pub(crate) fn field(&self) -> Option<&str> {
        match &self.params {
            Params::None | Params::Rows(_) => None,
            Params::Value(f, _) | Params::Values(f, _) | Params::Field(f) => Some(f),
        }
    }
}

impl Chunk {
    /// `varint n · n × uuid · varint len · [the pending ranks, a roaring
    /// bitmap] · varint groups · { varint len · field · value · varint count
    /// · { varint rank } }`, a group being a row the targets of its ranks held.
    /// Named by dense id: `varint (BY_ID + n) · varint len · [the ids, a
    /// roaring bitmap]` in place of the uuids.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Enc::default();
        match &self.ids {
            Some(ids) => {
                // Records written together are runs of ids: a few bytes.
                let mut ids = ids.clone();
                ids.optimize();
                let mut bytes = Vec::new();
                ids.serialize_into(&mut bytes).expect("a bitmap into a vector");
                out.varint(BY_ID + ids.len()).bytes(&bytes);
            }
            None => {
                out.varint(self.targets.len() as u64);
                for t in &self.targets {
                    out.raw(t.as_bytes());
                }
            }
        }
        if self.pending.is_empty() {
            out.varint(0);
        } else {
            // A deletion pending over a chunk is a run of ranks: a few bytes.
            let mut pending = self.pending.clone();
            pending.optimize();
            let mut bytes = Vec::new();
            pending.serialize_into(&mut bytes).expect("a bitmap into a vector");
            out.bytes(&bytes);
        }
        let mut groups: Vec<(&FieldRow, Vec<u64>)> = Vec::new();
        for (rank, rows) in self.before.iter().enumerate() {
            for row in rows {
                match groups.iter_mut().find(|(r, _)| *r == row) {
                    Some((_, ranks)) => ranks.push(rank as u64),
                    None => groups.push((row, vec![rank as u64])),
                }
            }
        }
        out.varint(groups.len() as u64);
        for (row, ranks) in groups {
            out.bytes(row.name.as_bytes()).value(&row.value).varint(ranks.len() as u64);
            for rank in ranks {
                out.varint(rank);
            }
        }
        out.0
    }

    pub(crate) fn decode(b: &[u8]) -> Result<Chunk> {
        let mut r = Dec(b);
        let head = r.varint()?;
        let (n, targets, ids) = if head >= BY_ID {
            let n = usize::try_from(head - BY_ID)?;
            let ids = RoaringBitmap::deserialize_from(r.bytes()?)?;
            ensure!(ids.len() == n as u64, "a chunk of {n} targets naming {} ids", ids.len());
            (n, Vec::new(), Some(ids))
        } else {
            let n = usize::try_from(head)?;
            (n, (0..n).map(|_| r.uuid()).collect::<Result<Vec<_>>>()?, None)
        };
        ensure!(n as u64 <= CHUNK, "a chunk of {n} targets");
        let pending = match r.bytes()? {
            [] => RoaringBitmap::new(),
            bytes => RoaringBitmap::deserialize_from(bytes)?,
        };
        ensure!(pending.max().is_none_or(|m| (m as usize) < n), "a chunk pending past its end");
        let mut before = vec![Vec::new(); n];
        for _ in 0..r.varint()? {
            let row = FieldRow { name: r.string()?, value: r.value()? };
            for _ in 0..r.varint()? {
                let rank = r.varint()?;
                match before.get_mut(rank as usize) {
                    Some(rows) => rows.push(row.clone()),
                    None => bail!("a chunk of {n} targets naming rank {rank}"),
                }
            }
        }
        ensure!(r.is_empty(), "a chunk of trailing bytes");
        Ok(Chunk { targets, before, pending, ids })
    }

    /// How many targets.
    pub(crate) fn len(&self) -> usize {
        self.ids.as_ref().map_or(self.targets.len(), |ids| ids.len() as usize)
    }
}

/// The key of chunk `n` of the entry `first` in `log_targets`.
pub(super) fn chunk_key(first: i64, n: u64) -> Vec<u8> {
    super::key(&[&super::be(first), &(n as u32).to_be_bytes()])
}

/// How many chunks an entry of `targets` targets has.
pub(super) fn chunks_of(targets: u64) -> u64 {
    targets.div_ceil(CHUNK)
}

/// An entry of the log: its first operation id and its header.
#[derive(Clone, Debug)]
pub(super) struct Entry {
    pub(super) first: i64,
    pub(super) h: Header,
}

impl Entry {
    pub(super) fn last(&self) -> i64 {
        self.first + self.h.targets as i64 - 1
    }

    pub(super) fn covers(&self, id: i64) -> bool {
        (self.first..=self.last()).contains(&id)
    }

    /// The chunk operation `id` is in, and its rank there.
    pub(super) fn place(&self, id: i64) -> (u64, usize) {
        let k = (id - self.first) as u64;
        (k / CHUNK, (k % CHUNK) as usize)
    }

    /// Operation `id`, whose target is `entity`: what a write to that target
    /// alone would have logged.
    pub(super) fn op_of(&self, id: i64, entity: Uuid) -> OpRow {
        OpRow {
            id,
            log_context_id: self.h.context,
            timestamp: self.h.timestamp,
            op_type: self.h.wire_type().to_string(),
            entity_uuid: entity,
            field_name: self.h.field().map(str::to_string),
            reverts_op_id: self.h.reverts.map(|r| r + (id - self.first)),
            origin: self.h.origin.clone(),
        }
    }

    /// What an operation took off its target, from what its chunk keeps of
    /// it: a removal keeps nothing, its value being its parameter.
    pub(super) fn before(&self, kept: Vec<FieldRow>) -> Vec<FieldRow> {
        match (&self.h.effect, &self.h.params) {
            (Effect::Remove, Params::Value(field, value)) => {
                vec![FieldRow { name: field.clone(), value: value.clone() }]
            }
            _ => kept,
        }
    }

    /// The rows an add added: its parameter. None for another effect.
    pub(super) fn added(&self) -> Vec<FieldRow> {
        match (&self.h.effect, &self.h.params) {
            (Effect::Add, Params::Value(field, value)) => {
                vec![FieldRow { name: field.clone(), value: value.clone() }]
            }
            _ => Vec::new(),
        }
    }

    /// Whether the entry is a deletion.
    pub(super) fn deletes(&self) -> bool {
        self.h.effect == Effect::Delete
    }
}

/// The header of an individual operation, and what its chunk
/// keeps as its before (doc "Log storage").
pub(super) fn of_op(op: &NewOp, context: i64, timestamp: i64) -> Result<(Header, Vec<FieldRow>)> {
    use crate::log::OpType as T;
    let file = match op.op_type {
        T::FileDeleted => FileAction::Deleted,
        T::FileMoved => FileAction::Moved,
        T::FileModified => FileAction::Modified,
        _ => FileAction::None,
    };
    let field = || op.field_name.clone().context("an operation on a field names none");
    let mut before = op.before.clone();
    let (effect, params) = match op.op_type {
        T::CreateRecord => (Effect::Create, Params::None),
        T::DeleteRecord => (Effect::Delete, Params::None),
        T::Unknown => (Effect::Unknown, Params::None),
        T::SetRecord => (Effect::SetRecord, Params::Rows(op.after.clone())),
        T::AppendField => match &op.after[..] {
            [added] => (Effect::Add, Params::Value(field()?, added.value.clone())),
            other => bail!("an append of {} rows", other.len()),
        },
        // One row: a removal, whose value is its parameter. Every row of the
        // field (`Writer::delete_field_rows`): the field unset.
        T::DeleteField => match &op.before[..] {
            [one] => {
                let value = one.value.clone();
                before.clear();
                (Effect::Remove, Params::Value(field()?, value))
            }
            _ => (Effect::Unset, Params::Field(field()?)),
        },
        T::SetField | T::FileDeleted | T::FileMoved | T::FileModified => {
            if op.after.is_empty() {
                (Effect::Unset, Params::Field(field()?))
            } else {
                (
                    Effect::Set,
                    Params::Values(field()?, op.after.iter().map(|r| r.value.clone()).collect()),
                )
            }
        }
    };
    let h = Header {
        context,
        timestamp,
        origin: op.origin.clone(),
        effect,
        file,
        reverts: op.reverts_op_id,
        params,
        targets: 1,
    };
    Ok((h, before))
}

/// The chunks of a write to many records, its targets named by dense id,
/// `ids` in their order — no uuid is read (doc "Log storage") —, in chunks
/// of [`CHUNK`], the before of the target at rank `k` being `before(k)`, and
/// every rank pending when `pending`.
pub(super) fn chunks_by_id(
    ids: &RoaringBitmap,
    pending: bool,
    mut before: impl FnMut(usize) -> Vec<FieldRow>,
) -> Vec<Chunk> {
    let all: Vec<u32> = ids.iter().collect();
    all.chunks(CHUNK as usize)
        .enumerate()
        .map(|(n, part)| {
            let at = n * CHUNK as usize;
            Chunk {
                targets: Vec::new(),
                before: (0..part.len()).map(|k| before(at + k)).collect(),
                pending: if pending {
                    (0..part.len() as u32).collect()
                } else {
                    RoaringBitmap::new()
                },
                ids: Some(part.iter().copied().collect()),
            }
        })
        .collect()
}

/// The chunks a read loaded, kept while it reads the operations of an
/// entry one after the other.
#[derive(Default)]
pub(super) struct Loaded {
    at: Option<(i64, u64)>,
    chunk: Option<Chunk>,
}

impl Read<'_> {
    /// The entry that holds operation `id`: the last that begins at or before
    /// it, if it reaches it. An entry the store's cache holds that reaches it
    /// is that one: the entries are disjoint, and change only with a write.
    pub(super) fn entry_at(&self, id: i64) -> Result<Option<Entry>> {
        if let Some((cache, generation)) = self.entries {
            let cache = cache.lock().unwrap_or_else(|e| e.into_inner());
            if cache.generation == generation {
                if let Some((first, h)) = cache.entries.range(..=id).next_back() {
                    let entry = Entry { first: *first, h: h.clone() };
                    if entry.covers(id) {
                        return Ok(Some(entry));
                    }
                }
            }
        }
        let end = super::be(id);
        let range = (std::ops::Bound::Unbounded, std::ops::Bound::Included(&end[..]));
        self.count(1)?;
        let found = match self.t.log_entries.rev_range(self.r, &range)?.next() {
            Some(e) => {
                let (k, v) = e?;
                let entry = Entry { first: super::from_be(k), h: Header::decode(v)? };
                entry.covers(id).then_some(entry)
            }
            None => None,
        };
        if let (Some((cache, generation)), Some(entry)) = (self.entries, &found) {
            let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
            if cache.generation != generation || cache.entries.len() >= ENTRY_CACHE {
                cache.entries.clear();
                cache.generation = generation;
            }
            cache.entries.insert(entry.first, entry.h.clone());
        }
        Ok(found)
    }

    /// Chunk `n` of the entry `first`: the one a write transaction holds, if
    /// it is that one.
    /// A chunk that names its targets by dense id has their uuids read.
    pub(super) fn chunk(&self, first: i64, n: u64) -> Result<Chunk> {
        let mut chunk = match self.log_chunk.and_then(|held| held(first, n)) {
            Some(chunk) => chunk,
            None => {
                self.count(1)?;
                let v = self
                    .t
                    .log_targets
                    .get(self.r, &chunk_key(first, n))?
                    .with_context(|| format!("chunk {n} of log entry {first} is missing"))?;
                Chunk::decode(v)?
            }
        };
        if let Some(ids) = &chunk.ids {
            if chunk.targets.len() != ids.len() as usize {
                chunk.targets = self.uuids_of_ids_iter(&mut ids.iter())?;
            }
        }
        Ok(chunk)
    }

    /// The chunk of operation `id` of `entry`, from `loaded` when it holds it.
    pub(super) fn chunk_of<'l>(
        &self,
        entry: &Entry,
        id: i64,
        loaded: &'l mut Loaded,
    ) -> Result<(&'l Chunk, usize)> {
        let (n, rank) = entry.place(id);
        if loaded.at != Some((entry.first, n)) {
            loaded.chunk = Some(self.chunk(entry.first, n)?);
            loaded.at = Some((entry.first, n));
        }
        Ok((loaded.chunk.as_ref().expect("just loaded"), rank))
    }

    /// Operation `id` of `entry`.
    pub(super) fn op_in(&self, entry: &Entry, id: i64, loaded: &mut Loaded) -> Result<OpRow> {
        let (chunk, rank) = self.chunk_of(entry, id, loaded)?;
        Ok(entry.op_of(id, chunk.targets[rank]))
    }

    /// The rows operation `id` took off its target. A deletion of many records
    /// still pending for it has them in the record (doc "Mass writes in the
    /// store").
    pub(super) fn before_rows(&self, id: i64) -> Result<Vec<FieldRow>> {
        let Some(entry) = self.entry_at(id)? else { return Ok(Vec::new()) };
        if entry.h.effect == Effect::Remove {
            return Ok(entry.before(Vec::new()));
        }
        let mut loaded = Loaded::default();
        let (chunk, rank) = self.chunk_of(&entry, id, &mut loaded)?;
        if chunk.pending.contains(rank as u32) {
            let dense = match chunk.ids.as_ref().and_then(|ids| ids.select(rank as u32)) {
                Some(id) => id,
                None => {
                    let uuid = chunk.targets[rank];
                    self.count(1)?;
                    super::derived::id_of(self.t, self.r, uuid.as_bytes())?
                        .with_context(|| format!("deleted metarecord {uuid} has no dense id"))?
                }
            };
            return self.record_raw(dense);
        }
        Ok(chunk.before[rank].clone())
    }

    /// The rows operation `id` added: an add's value, none for another effect.
    pub(super) fn added_rows(&self, id: i64) -> Result<Vec<FieldRow>> {
        Ok(self.entry_at(id)?.map(|e| e.added()).unwrap_or_default())
    }

    /// The operations of `ids` (ids of the log), in the order given: the entry
    /// and the chunk read last serve the ids that follow. For operations all
    /// of one target, when it is known (`entity`), no chunk is read.
    pub(super) fn ops_resolved_of(
        &self,
        ids: impl Iterator<Item = i64>,
        entity: Option<Uuid>,
    ) -> Result<Vec<OpRow>> {
        let mut out = Vec::new();
        let mut entry: Option<Entry> = None;
        let mut loaded = Loaded::default();
        for id in ids {
            if !entry.as_ref().is_some_and(|e| e.covers(id)) {
                entry =
                    Some(self.entry_at(id)?.with_context(|| format!("operation {id} vanished"))?);
            }
            let e = entry.as_ref().expect("just found");
            out.push(match entity {
                Some(u) => e.op_of(id, u),
                None => self.op_in(e, id, &mut loaded)?,
            });
        }
        Ok(out)
    }

    /// The ids of the operations from `from` on, at most `cap` of them.
    pub(super) fn ids_from(&self, from: i64, cap: usize) -> Result<Vec<i64>> {
        let mut out = Vec::new();
        let mut start = from;
        // An entry that began before `from` still holds ids from it on.
        if let Some(e) = self.entry_at(from)? {
            out.extend((from..=e.last()).take(cap));
            start = e.last() + 1;
        }
        let from_key = super::be(start);
        let range = (std::ops::Bound::Included(&from_key[..]), std::ops::Bound::Unbounded);
        let mut read = 0;
        for e in self.t.log_entries.range(self.r, &range)? {
            if out.len() >= cap {
                break;
            }
            let (k, v) = e?;
            read += 1;
            let entry = Entry { first: super::from_be(k), h: Header::decode(v)? };
            out.extend((entry.first..=entry.last()).take(cap - out.len()));
        }
        self.count(read)?;
        Ok(out)
    }

    /// The operations at or before `from`, at most `max`, newest first: a
    /// backward range of `log_entries`, and the chunks of those it reaches.
    pub(super) fn back(&self, from: i64, max: usize) -> Result<Vec<OpRow>> {
        let mut out = Vec::new();
        let end = super::be(from);
        let range = (std::ops::Bound::Unbounded, std::ops::Bound::Included(&end[..]));
        let mut loaded = Loaded::default();
        for e in self.t.log_entries.rev_range(self.r, &range)? {
            if out.len() >= max {
                break;
            }
            self.count(1)?;
            let (k, v) = e?;
            let entry = Entry { first: super::from_be(k), h: Header::decode(v)? };
            for id in (entry.first..=entry.last().min(from)).rev().take(max - out.len()) {
                out.push(self.op_in(&entry, id, &mut loaded)?);
            }
        }
        Ok(out)
    }

    /// The last operation of the entry `first` is.
    pub(super) fn entry_last(&self, first: i64, v: &[u8]) -> Result<i64> {
        Ok(Entry { first, h: Header::decode(v)? }.last())
    }

    /// The targets of `entry`, read from its chunks.
    pub(super) fn targets(&self, entry: &Entry) -> Result<Vec<Uuid>> {
        let mut out = Vec::with_capacity(entry.h.targets as usize);
        for n in 0..chunks_of(entry.h.targets) {
            out.extend(self.chunk(entry.first, n)?.targets);
        }
        Ok(out)
    }

    /// The last operation `log_entries_by_metarecord` indexes: every one up to
    /// it, none after (doc "Log storage").
    pub(super) fn indexed_up_to(&self) -> Result<i64> {
        self.count(1)?;
        Ok(super::State::LogIndexedUpTo.get(self.t, self.r)?.unwrap_or(0))
    }

    /// The entries holding an operation after `after`, oldest first.
    pub(super) fn entries_after(&self, after: i64) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        let mut start = after + 1;
        if let Some(e) = self.entry_at(start)? {
            start = e.last() + 1;
            out.push(e);
        }
        let from = super::be(start);
        let range = (std::ops::Bound::Included(&from[..]), std::ops::Bound::Unbounded);
        for e in self.t.log_entries.range(self.r, &range)? {
            self.count(1)?;
            let (k, v) = e?;
            out.push(Entry { first: super::from_be(k), h: Header::decode(v)? });
        }
        Ok(out)
    }

    /// A record's operations after `after`, ascending: those
    /// `log_entries_by_metarecord` indexes, then those of the entries the
    /// idle fold has not indexed yet, found among their targets.
    pub(super) fn entity_op_ids_after(&self, entity: Uuid, after: i64) -> Result<Vec<i64>> {
        let mut ids = Vec::new();
        for e in self.t.log_entries_by_metarecord.prefix_iter(self.r, entity.as_bytes())? {
            self.count(1)?;
            let id = super::from_be(&e?.0[16..]);
            if id > after {
                ids.push(id);
            }
        }
        let indexed = self.indexed_up_to()?;
        let from = indexed.max(after);
        for entry in self.entries_after(from)? {
            for (k, u) in self.targets(&entry)?.into_iter().enumerate() {
                let id = entry.first + k as i64;
                if u == entity && id > from {
                    ids.push(id);
                }
            }
        }
        ids.sort_unstable();
        ids.dedup();
        Ok(ids)
    }

    /// The operations that revert operation `op`: the revert of `first ·
    /// last` reverts each of them, in order (doc "Log storage").
    pub(super) fn reverters_of(&self, op: i64) -> Result<Vec<i64>> {
        let Some(entry) = self.entry_at(op)? else { return Ok(Vec::new()) };
        let mut out = Vec::new();
        let mut read = 1;
        for e in self.t.log_reverted_by.prefix_iter(self.r, &super::be(entry.first))? {
            read += 1;
            let (first, last, revert) = reverted_key(e?.0);
            if (first..=last).contains(&op) {
                out.push(revert + (op - first));
            }
        }
        self.count(read)?;
        Ok(out)
    }
}

/// `entry's first op · first reverted · last reverted · the op that reverts
/// the first`, a key of `log_reverted_by`.
fn reverted_by_key(entry: i64, first: i64, last: i64, revert: i64) -> Vec<u8> {
    super::key(&[&super::be(entry), &super::be(first), &super::be(last), &super::be(revert)])
}

/// The first and last reverted, and the revert of the first, of a key of
/// `log_reverted_by`.
fn reverted_key(k: &[u8]) -> (i64, i64, i64) {
    (super::from_be(&k[8..16]), super::from_be(&k[16..24]), super::from_be(&k[24..32]))
}

/// How many entries the store's cache keeps.
const ENTRY_CACHE: usize = 100_000;

/// The entries of the log reads looked up, by first operation id, for the
/// generation of the store they were read at.
#[derive(Default)]
pub(super) struct EntryCache {
    generation: u64,
    entries: std::collections::BTreeMap<i64, Header>,
}

/// The chunk of the log a write transaction extends an entry in, decoded
/// (doc "Log storage"): chunk `n` of the entry `first`.
pub(super) struct HeldChunk {
    first: i64,
    n: u64,
    chunk: Chunk,
}

/// The targets of the last entry of the log, while writes extend it: kept by
/// the store between them, so that an extension reads no chunk to know it
/// does not name its target already.
pub(super) struct LastTargets {
    first: i64,
    targets: u64,
    set: std::collections::HashSet<Uuid>,
}

impl KvTxn<'_> {
    /// Counts a chunk of the log decoded (`0`) or encoded (`1`).
    fn chunk_coded(&self, which: usize) {
        self.log_chunk_codings[which].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// A copy of the chunk the transaction holds, if it is chunk `n` of the
    /// entry `first`.
    pub(super) fn held_chunk(&self, first: i64, n: u64) -> Option<Chunk> {
        let held = self.log_chunk.borrow();
        held.as_ref().filter(|h| h.first == first && h.n == n).map(|h| h.chunk.clone())
    }

    /// Encodes and puts the chunk the transaction holds, and lets it go: at
    /// the commit, and before what writes the chunks of the log otherwise.
    pub(super) fn flush_log_chunk(&self) -> Result<()> {
        let Some(held) = self.log_chunk.borrow_mut().take() else { return Ok(()) };
        self.chunk_coded(1);
        let key = chunk_key(held.first, held.n);
        self.t.log_targets.put(&mut self.txn.borrow_mut(), &key, &held.chunk.encode())?;
        Ok(())
    }

    /// Reads through the transaction.
    pub(super) fn read_log<T>(&self, f: impl FnOnce(&Read) -> Result<T>) -> Result<T> {
        let txn = self.txn.borrow();
        let mut f = Some(f);
        self.read_in(&txn, &mut |read: &Read| (f.take().expect("called once"))(read))
    }

    /// Appends an entry of `chunks` (doc "Log storage"). Its first operation
    /// id.
    pub(super) fn put_entry(&self, h: &Header, chunks: &[Chunk]) -> Result<i64> {
        // The entry is the log's last now: its last chunk is the one held.
        self.flush_log_chunk()?;
        let first = {
            let mut w = self.txn.borrow_mut();
            let t = self.t;
            let first = super::State::NextLogOp.get(&t, &w)?.unwrap_or(1);
            debug_assert_eq!(chunks.iter().map(|c| c.len() as u64).sum::<u64>(), h.targets);
            let at = super::be(first);
            t.log_entries.put(&mut w, &at, &h.encode()?)?;
            if let Some((last, before)) = chunks.split_last() {
                for (n, chunk) in before.iter().enumerate() {
                    self.chunk_coded(1);
                    t.log_targets.put(&mut w, &chunk_key(first, n as u64), &chunk.encode())?;
                }
                *self.log_chunk.borrow_mut() =
                    Some(HeldChunk { first, n: before.len() as u64, chunk: last.clone() });
            }
            let mut count = Enc::default();
            count.varint(h.targets);
            let key = super::key(&[&super::be(h.context), &at]);
            t.log_entries_by_context.put(&mut w, &key, &count.0)?;
            self.class(h.origin.as_deref()).put(&mut w, &at, &[])?;
            super::State::NextLogOp.put(&t, &mut w, first + h.targets as i64)?;
            first
        };
        if let Some(reverted) = h.reverts {
            for (entry, from, to) in self.reverted_spans(reverted, h.targets as i64)? {
                let key = reverted_by_key(entry, from, to, first + (from - reverted));
                self.t.log_reverted_by.put(&mut self.txn.borrow_mut(), &key, &[])?;
            }
        }
        Ok(first)
    }

    /// The entries the operations `from..from + n` are in, and the span of
    /// them each holds: `(entry, first, last)`. An operation the log no longer
    /// holds is passed over.
    fn reverted_spans(&self, from: i64, n: i64) -> Result<Vec<(i64, i64, i64)>> {
        let end = from + n - 1;
        self.read_log(|r| {
            let mut out = Vec::new();
            let mut id = from;
            while id <= end {
                match r.entry_at(id)? {
                    Some(e) => {
                        let last = e.last().min(end);
                        out.push((e.first, id, last));
                        id = last + 1;
                    }
                    None => {
                        let at = super::be(id);
                        let range =
                            (std::ops::Bound::Excluded(&at[..]), std::ops::Bound::Unbounded);
                        r.count(1)?;
                        match r.t.log_entries.range(r.r, &range)?.next() {
                            Some(e) => id = super::from_be(e?.0),
                            None => break,
                        }
                    }
                }
            }
            Ok(out)
        })
    }

    /// Adds the operation `h` on `target` to the last entry of the log, when it
    /// differs from it only by its target (doc "Log storage"): the same
    /// context, origin, effect, file action and parameters, a revert of the
    /// operation after the last one the entry reverts, and a target the entry
    /// does not name. A deletion is not extended: the undo of a deletion of
    /// many records knows it by its entry. The operation's id, `None` when it
    /// is not added.
    pub(super) fn extend_last(
        &self,
        h: &Header,
        target: Uuid,
        before: Vec<FieldRow>,
    ) -> Result<Option<i64>> {
        let t = self.t;
        let next = {
            let w = self.txn.borrow();
            super::State::NextLogOp.get(&t, &w)?.unwrap_or(1)
        };
        if h.effect == Effect::Delete {
            return Ok(None);
        }
        let Some(mut entry) = self.read_log(|r| r.entry_at(next - 1))? else { return Ok(None) };
        let e = &entry.h;
        let reverts_next = match (e.reverts, h.reverts) {
            (None, None) => true,
            (Some(r), Some(n)) => n == r + e.targets as i64,
            _ => false,
        };
        let same = e.context == h.context
            && e.origin == h.origin
            && e.effect == h.effect
            && e.file == h.file
            && e.params == h.params
            && reverts_next;
        if !same || entry.last() != next - 1 {
            return Ok(None);
        }
        let (n, rank) = (entry.h.targets / CHUNK, (entry.h.targets % CHUNK) as usize);
        // A chunk naming its targets by dense id is not extended: an entry
        // of a write in bulk, which a write to one record rarely follows.
        if rank != 0 {
            let by_id = match self.log_chunk.borrow().as_ref() {
                Some(h) if h.first == entry.first && h.n == n => Some(h.chunk.ids.is_some()),
                _ => None,
            };
            let by_id = match by_id {
                Some(b) => b,
                None => {
                    let t = self.t;
                    let w = self.txn.borrow();
                    let v = t.log_targets.get(&w, &chunk_key(entry.first, n))?;
                    v.is_some_and(|v| Chunk::decode(v).is_ok_and(|c| c.ids.is_some()))
                }
            };
            if by_id {
                return Ok(None);
            }
        }
        // The entry's targets: those the store kept since the last write that
        // extended it, else its chunks.
        let mut last = self.last_targets.borrow_mut();
        if !last.as_ref().is_some_and(|l| l.first == entry.first && l.targets == entry.h.targets) {
            let kept = self.last_targets_kept.lock().unwrap_or_else(|e| e.into_inner()).take();
            *last = match kept.filter(|l| l.first == entry.first && l.targets == entry.h.targets) {
                Some(l) => Some(l),
                None => {
                    let set = self.read_log(|r| r.targets(&entry))?.into_iter().collect();
                    Some(LastTargets { first: entry.first, targets: entry.h.targets, set })
                }
            };
        }
        let known = last.as_mut().expect("just loaded");
        if !known.set.insert(target) {
            return Ok(None);
        }
        known.targets += 1;
        drop(last);
        let at = super::be(entry.first);
        // The chunk extended: the one held, else read once and held.
        let holds =
            self.log_chunk.borrow().as_ref().is_some_and(|h| h.first == entry.first && h.n == n);
        if !holds {
            self.flush_log_chunk()?;
            let chunk = if rank == 0 {
                Chunk {
                    targets: Vec::new(),
                    before: Vec::new(),
                    pending: RoaringBitmap::new(),
                    ids: None,
                }
            } else {
                self.chunk_coded(0);
                self.read_log(|r| r.chunk(entry.first, n))?
            };
            *self.log_chunk.borrow_mut() = Some(HeldChunk { first: entry.first, n, chunk });
        }
        {
            let mut held = self.log_chunk.borrow_mut();
            let chunk = &mut held.as_mut().expect("just held").chunk;
            chunk.targets.push(target);
            chunk.before.push(before);
        }
        entry.h.targets += 1;
        // The revert's key for the entry the previous operation reverted grows
        // when that entry holds the next one too.
        let revert_key = match h.reverts {
            Some(reverted) => Some(self.grown_revert_key(reverted, next)?),
            None => None,
        };
        let mut w = self.txn.borrow_mut();
        t.log_entries.put(&mut w, &at, &entry.h.encode()?)?;
        let mut count = Enc::default();
        count.varint(entry.h.targets);
        t.log_entries_by_context.put(
            &mut w,
            &super::key(&[&super::be(entry.h.context), &at]),
            &count.0,
        )?;
        if let Some((gone, key)) = revert_key {
            if let Some(gone) = gone {
                t.log_reverted_by.delete(&mut w, &gone)?;
            }
            if let Some(key) = key {
                t.log_reverted_by.put(&mut w, &key, &[])?;
            }
        }
        super::State::NextLogOp.put(&t, &mut w, next + 1)?;
        Ok(Some(next))
    }

    /// The key of `log_reverted_by` operation `revert` adds when it reverts
    /// `reverted`, the previous one of its entry reverting `reverted - 1`:
    /// the key of the previous one grown, which it replaces, when one entry
    /// holds both; a key of its own else. Nothing when the log no longer holds
    /// `reverted`.
    #[allow(clippy::type_complexity)]
    fn grown_revert_key(
        &self,
        reverted: i64,
        revert: i64,
    ) -> Result<(Option<Vec<u8>>, Option<Vec<u8>>)> {
        let Some(e) = self.read_log(|r| r.entry_at(reverted))? else { return Ok((None, None)) };
        let previous = {
            let w = self.txn.borrow();
            let mut found = None;
            for k in self.t.log_reverted_by.prefix_iter(&w, &super::be(e.first))? {
                let k = k?.0;
                let (first, last, by) = reverted_key(k);
                if last == reverted - 1 && by + (last - first) == revert - 1 {
                    found = Some((k.to_vec(), first, by));
                }
            }
            found
        };
        Ok(match previous {
            Some((gone, first, by)) => {
                (Some(gone), Some(reverted_by_key(e.first, first, reverted, by)))
            }
            None => (None, Some(reverted_by_key(e.first, reverted, reverted, revert))),
        })
    }

    /// Removes an entry and every key of it: those it has as an entry, as an
    /// entry reverted, and as a revert.
    pub(super) fn remove_entry(&self, entry: &Entry) -> Result<()> {
        self.flush_log_chunk()?;
        let t = self.t;
        let at = super::be(entry.first);
        let indexed = self.read_log(|r| r.indexed_up_to())?;
        let targets: Vec<Uuid> =
            if entry.first <= indexed { self.read_log(|r| r.targets(entry))? } else { Vec::new() };
        let reverts = match entry.h.reverts {
            Some(reverted) => self.reverted_spans(reverted, entry.h.targets as i64)?,
            None => Vec::new(),
        };
        let reverted: Vec<Vec<u8>> = {
            let w = self.txn.borrow();
            let mut out = Vec::new();
            for k in t.log_reverted_by.prefix_iter(&w, &at)? {
                out.push(k?.0.to_vec());
            }
            out
        };
        let mut w = self.txn.borrow_mut();
        t.log_entries.delete(&mut w, &at)?;
        for n in 0..chunks_of(entry.h.targets) {
            t.log_targets.delete(&mut w, &chunk_key(entry.first, n))?;
        }
        t.log_entries_by_context
            .delete(&mut w, &super::key(&[&super::be(entry.h.context), &at]))?;
        for (k, u) in targets.iter().enumerate() {
            let key = super::key(&[u.as_bytes(), &super::be(entry.first + k as i64)]);
            t.log_entries_by_metarecord.delete(&mut w, &key)?;
        }
        for k in reverted {
            t.log_reverted_by.delete(&mut w, &k)?;
        }
        if let Some(r) = entry.h.reverts {
            for (e, from, to) in reverts {
                let key = reverted_by_key(e, from, to, entry.first + (from - r));
                t.log_reverted_by.delete(&mut w, &key)?;
            }
        }
        t.log_watcher_entries.delete(&mut w, &at)?;
        t.log_user_entries.delete(&mut w, &at)?;
        Ok(())
    }

    /// Indexes the targets of the operations after the last one
    /// `log_entries_by_metarecord` indexes, about `budget` of them: one key
    /// `uuid · op id` each, and the pointer moved (doc "Log storage").
    /// Whether there were any.
    pub(super) fn index_log_step(&self, budget: u64) -> Result<bool> {
        let indexed = self.read_log(|r| r.indexed_up_to())?;
        let entries = self.read_log(|r| r.entries_after(indexed))?;
        if entries.is_empty() {
            return Ok(false);
        }
        // The chunks named by dense id are rewritten by uuid: the held one
        // among them.
        self.flush_log_chunk()?;
        let mut done = 0u64;
        let mut up_to = indexed;
        for entry in entries {
            let mut targets = Vec::with_capacity(entry.h.targets as usize);
            for n in 0..chunks_of(entry.h.targets) {
                let mut chunk = self.read_log(|r| r.chunk(entry.first, n))?;
                if chunk.ids.take().is_some() {
                    self.chunk_coded(1);
                    let key = chunk_key(entry.first, n);
                    self.t.log_targets.put(&mut self.txn.borrow_mut(), &key, &chunk.encode())?;
                }
                targets.extend(chunk.targets);
            }
            let mut w = self.txn.borrow_mut();
            for (k, u) in targets.iter().enumerate() {
                let id = entry.first + k as i64;
                if id > indexed {
                    let key = super::key(&[u.as_bytes(), &super::be(id)]);
                    self.t.log_entries_by_metarecord.put(&mut w, &key, &[])?;
                }
            }
            done += targets.len() as u64;
            up_to = entry.last();
            if done >= budget {
                break;
            }
        }
        super::State::LogIndexedUpTo.put(&self.t, &mut self.txn.borrow_mut(), up_to)?;
        Ok(true)
    }

    /// Writes into the chunks of the log the before of deleted records the
    /// log read in the records until now: each `(operation id, rows)`. An
    /// operation that is no deletion's any more (pruned) is passed over.
    pub(super) fn write_befores(&self, befores: Vec<(i64, Vec<FieldRow>)>) -> Result<()> {
        if befores.is_empty() {
            return Ok(());
        }
        // The chunks it changes are read from the table, the held one too.
        self.flush_log_chunk()?;
        let mut changed: std::collections::BTreeMap<(i64, u64), Chunk> = Default::default();
        let mut entry: Option<Entry> = None;
        for (id, rows) in befores {
            if !entry.as_ref().is_some_and(|e| e.covers(id)) {
                entry = self.read_log(|r| r.entry_at(id))?;
            }
            let Some(e) = entry.as_ref().filter(|e| e.deletes()) else { continue };
            let (n, rank) = e.place(id);
            let chunk = match changed.entry((e.first, n)) {
                std::collections::btree_map::Entry::Occupied(o) => o.into_mut(),
                std::collections::btree_map::Entry::Vacant(v) => {
                    v.insert(self.read_log(|r| r.chunk(e.first, n))?)
                }
            };
            if chunk.pending.remove(rank as u32) {
                chunk.before[rank] = rows;
            }
        }
        let mut w = self.txn.borrow_mut();
        for ((first, n), chunk) in changed {
            self.t.log_targets.put(&mut w, &chunk_key(first, n), &chunk.encode())?;
        }
        Ok(())
    }

    /// The last id of the entry `id` is the first of, `id` itself when none is.
    pub(super) fn entry_end(&self, id: i64) -> Result<i64> {
        let w = self.txn.borrow();
        let Some(v) = self.t.log_entries.get(&w, &super::be(id))? else { return Ok(id) };
        Ok(Entry { first: id, h: Header::decode(v)? }.last())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, value: Value) -> FieldRow {
        FieldRow { name: name.into(), value }
    }

    fn header(effect: Effect, params: Params) -> Header {
        Header {
            context: 12,
            timestamp: 1_763_700_000_000,
            origin: None,
            effect,
            file: FileAction::None,
            reverts: None,
            params,
            targets: 1,
        }
    }

    fn every_header() -> Vec<Header> {
        let tags = || "tags".to_string();
        let mut all = vec![
            header(Effect::Create, Params::None),
            header(Effect::Delete, Params::None),
            header(Effect::Unknown, Params::None),
            header(
                Effect::SetRecord,
                Params::Rows(vec![row("a", Value::Int(1)), row("b", Value::Nothing)]),
            ),
            header(Effect::Add, Params::Value(tags(), Value::String("x".into()))),
            header(Effect::Remove, Params::Value(tags(), Value::Int(-3))),
            header(Effect::Set, Params::Values(tags(), vec![Value::Nothing])),
            header(Effect::Set, Params::Values(tags(), vec![Value::Int(1), Value::Int(2)])),
            header(Effect::Unset, Params::Field(tags())),
        ];
        let mut full = header(Effect::Set, Params::Values("mfr_path".into(), vec![Value::Nothing]));
        full.origin = Some(crate::log::ORIGIN_WATCHER.into());
        full.file = FileAction::Deleted;
        full.reverts = Some(1 << 40);
        full.targets = 1 << 33;
        all.push(full);
        let mut trash = header(Effect::Delete, Params::None);
        trash.origin = Some(crate::log::ORIGIN_TRASH.into());
        trash.timestamp = -5;
        all.push(trash);
        all
    }

    #[test]
    fn every_header_comes_back() {
        for h in every_header() {
            let bytes = h.encode().unwrap();
            assert_eq!(Header::decode(&bytes).unwrap(), h);
        }
    }

    #[test]
    fn a_header_whose_params_are_not_its_effects_is_refused() {
        assert!(header(Effect::Add, Params::None).encode().is_err());
        assert!(header(Effect::Create, Params::Field("a".into())).encode().is_err());
        assert!(header(Effect::Set, Params::Values("a".into(), vec![])).encode().is_err());
    }

    #[test]
    fn a_truncated_header_is_an_error_not_a_panic() {
        for h in every_header() {
            let bytes = h.encode().unwrap();
            for cut in 0..bytes.len() {
                assert!(Header::decode(&bytes[..cut]).is_err(), "{h:?} cut at {cut}");
            }
            let mut longer = bytes.clone();
            longer.push(0);
            assert!(Header::decode(&longer).is_err(), "a trailing byte");
        }
    }

    #[test]
    fn an_add_of_a_short_tag_is_twenty_two_bytes() {
        let h = header(Effect::Add, Params::Value("tags".into(), Value::String("foo".into())));
        // context 1 · timestamp 6 · origin, effect, file 3 · reverts 1 ·
        // field 5 · value 5 · targets 1.
        assert_eq!(h.encode().unwrap().len(), 22);
    }

    #[test]
    fn the_wire_names_the_file_action_else_the_effect() {
        let cases = [
            (Effect::Create, FileAction::None, "create_metarecord"),
            (Effect::Delete, FileAction::None, "delete_metarecord"),
            (Effect::SetRecord, FileAction::None, "set_metarecord"),
            (Effect::Add, FileAction::None, "append_field"),
            (Effect::Remove, FileAction::None, "delete_field"),
            (Effect::Unset, FileAction::None, "delete_field"),
            (Effect::Set, FileAction::None, "set_field"),
            (Effect::Unknown, FileAction::None, "unknown"),
            (Effect::Set, FileAction::Deleted, "file_deleted"),
            (Effect::Unset, FileAction::Moved, "file_moved"),
            (Effect::Set, FileAction::Modified, "file_modified"),
        ];
        for (effect, file, wire) in cases {
            let mut h = header(effect, Params::None);
            h.file = file;
            assert_eq!(h.wire_type(), wire, "{effect:?} {file:?}");
        }
        let h = header(Effect::Unset, Params::Field("tags".into()));
        assert_eq!(h.field(), Some("tags"));
        assert_eq!(header(Effect::Create, Params::None).field(), None);
    }

    #[test]
    fn a_chunk_comes_back_with_each_targets_before() {
        let targets: Vec<Uuid> = (1..=4).map(Uuid::from_u128).collect();
        let before = vec![
            vec![row("tags", Value::String("a".into())), row("tags", Value::String("b".into()))],
            vec![],
            vec![row("tags", Value::String("a".into()))],
            vec![row("n", Value::Int(3)), row("tags", Value::Nothing)],
        ];
        let chunk = Chunk {
            targets: targets.clone(),
            before: before.clone(),
            pending: Default::default(),
            ids: None,
        };
        let back = Chunk::decode(&chunk.encode()).unwrap();
        assert_eq!(back.targets, targets);
        assert!(back.pending.is_empty());
        let back = back.before;
        for (k, rows) in before.iter().enumerate() {
            let (mut want, mut got) = (rows.clone(), back[k].clone());
            let key = |r: &FieldRow| format!("{r:?}");
            want.sort_by_key(key);
            got.sort_by_key(key);
            assert_eq!(got, want, "target {k}");
        }
        // A deletion pending: every before still in the records, then all but
        // the target given back and written to.
        let mut pending =
            Chunk { targets, before: vec![vec![]; 4], pending: (0..4).collect(), ids: None };
        assert_eq!(Chunk::decode(&pending.encode()).unwrap(), pending);
        pending.pending.remove(2);
        pending.before[2] = vec![row("n", Value::Int(1))];
        assert_eq!(Chunk::decode(&pending.encode()).unwrap(), pending);
    }

    #[test]
    fn a_shared_value_is_written_once_in_a_chunk() {
        let targets: Vec<Uuid> = (0..CHUNK as u128).map(Uuid::from_u128).collect();
        let tag = || vec![row("tags", Value::String("foo".into()))];
        let chunk = Chunk {
            targets: targets.clone(),
            before: targets.iter().map(|_| tag()).collect(),
            pending: Default::default(),
            ids: None,
        };
        let bytes = chunk.encode();
        // n · uuids · no pending · one group: field, value, rank count, ranks
        // (128 of one byte, the rest of two).
        let ranks = 128 + 2 * (CHUNK as usize - 128);
        assert_eq!(bytes.len(), 2 + 16 * CHUNK as usize + 1 + 1 + 5 + 5 + 2 + ranks);
        // A deletion pending over the whole chunk: a run, a few bytes.
        let deleted = Chunk {
            targets,
            before: vec![vec![]; CHUNK as usize],
            pending: (0..CHUNK as u32).collect(),
            ids: None,
        };
        assert!(deleted.encode().len() < 16 * CHUNK as usize + 32);
        assert_eq!(Chunk::decode(&bytes).unwrap(), chunk);
    }

    #[test]
    fn a_truncated_chunk_is_an_error_not_a_panic() {
        let chunk = Chunk {
            targets: vec![Uuid::from_u128(1), Uuid::from_u128(2)],
            before: vec![vec![row("a", Value::Int(1))], vec![]],
            pending: [1].into_iter().collect(),
            ids: None,
        };
        let bytes = chunk.encode();
        for cut in 0..bytes.len() {
            assert!(Chunk::decode(&bytes[..cut]).is_err(), "cut at {cut}");
        }
        let bad_rank = Chunk {
            targets: vec![Uuid::from_u128(1)],
            before: vec![vec![]],
            pending: Default::default(),
            ids: None,
        };
        let mut bytes = bad_rank.encode();
        // One group naming rank 5 of a chunk of one target.
        bytes.pop();
        bytes.extend_from_slice(&[1, 1, b'a', 2, 2, 1, 5]);
        assert!(Chunk::decode(&bytes).is_err());
    }

    #[test]
    fn a_chunk_named_by_dense_id_comes_back_with_its_ids() {
        let ids: RoaringBitmap = [3, 70, 71, 9000].into_iter().collect();
        let chunk = Chunk {
            targets: Vec::new(),
            before: vec![vec![row("tag", Value::Int(1))], vec![], vec![], vec![]],
            pending: [2].into_iter().collect(),
            ids: Some(ids.clone()),
        };
        assert_eq!(chunk.len(), 4);
        let back = Chunk::decode(&chunk.encode()).unwrap();
        assert_eq!(back, chunk);
        // Read, its uuids filled in: written again, it keeps the ids.
        let read = Chunk { targets: vec![Uuid::from_u128(1); 4], ..back };
        assert_eq!(Chunk::decode(&read.encode()).unwrap(), chunk);
    }

    #[test]
    fn a_chunk_named_by_dense_id_is_refused_when_its_count_lies() {
        let ids: RoaringBitmap = [3, 70].into_iter().collect();
        let chunk = Chunk {
            targets: Vec::new(),
            before: vec![vec![], vec![]],
            pending: Default::default(),
            ids: Some(ids),
        };
        let mut bytes = chunk.encode();
        // The count, one byte past BY_ID's varint, said three.
        let mut head = Enc::default();
        head.varint(BY_ID + 3);
        let mut real = Enc::default();
        real.varint(BY_ID + 2);
        bytes.splice(..real.0.len(), head.0);
        assert!(Chunk::decode(&bytes).is_err());
        for cut in 0..chunk.encode().len() {
            assert!(Chunk::decode(&chunk.encode()[..cut]).is_err(), "cut at {cut}");
        }
    }
}
