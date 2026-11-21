//! The key-value storage backend (doc "Storage"): the `store` traits
//! over LMDB (through `heed`). The tables below hold the primary data and the
//! event log; the derived key spaces the query source reads are in
//! [`derived`].
//!
//! Every table is an ordered map of byte strings. Integers are big-endian (so
//! byte order is numeric order), uuids their 16 bytes, and a name inside a
//! composite key is escaped (`00` → `00 FF`) and ended by `00 00`, so no name
//! is a prefix of another.
//!
//! | table         | key                              | value                     |
//! |---------------|----------------------------------|---------------------------|
//! | meta          | name                             | counter / HEAD / version / flag |
//! | metarecords   | uuid                             | (empty: it exists)        |
//! | records       | dense id                         | the rows: field name · value |
//! | mass          | part · number                    | a write to many records ([`mass`]) |
//! | field_types   | name · value type                | rows of that type         |
//! | forest        | field · parent · name bytes      | uuid · dense id           |
//! | ops           | op id                            | the operation             |
//! | snaps         | op id · added? · index           | a snapshot row            |
//! | ops_by_label  | label · op id                    | —                         |
//! | ops_by_entity | uuid · op id                     | —                         |
//! | labels        | label id                         | name · description · parent · created |
//! | label_kids    | parent label · child label       | —                         |
//! | label_reverts | 0 · reverter · reverted, 1 · reverted · reverter | 1 if the whole label |
//! | label_keys    | command key                      | label id                  |
//! | reverted_by   | op id · op id of a revert of it  | —                         |
//! | watcher_ops   | op id                            | —                         |
//! | user_ops      | op id                            | —                         |
//!
//! A record's rows are one key of `records`, its dense id (doc "Store tables"):
//! reading a record is one read, and nothing names a row but its record, its
//! name and its value. Older layouts kept them in `cells`, then `field_cells`,
//! one key per row; [`KvStore::open`] converts them ([`LAYOUT`]).
//!
//! The log is a sequence (doc "The operation log"): an operation names no
//! parent, and "the log up to here" is a range of `ops`. `watcher_ops` and
//! `user_ops` split it by origin, for the retention each one has (doc
//! "Automatic retention"): their sizes are B-tree statistics and their first
//! keys the oldest operations of each.
//!
//! Operations are grouped by nested labels (doc "Log contexts"): each
//! carries its innermost one, and `label_kids` holds the nesting. The label
//! the watcher's flushes hang under is `watcher_label` in `meta`.
//!
//! Operation ids and label ids are allocated from counters in `meta` and
//! never reused.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::File;
use std::path::Path;

use anyhow::{bail, Context, Result};
use heed::types::Bytes;
use heed::{CommitOrAbort, Env, EnvOpenOptions, RoTxn, WithoutTls};
use metafolder_core::metarecord::{TreeName, Value};
use uuid::Uuid;

use crate::error::DomainError;
use crate::log::{OpRow, Retention};
use crate::rows::{FieldRow, TreeRow};
use crate::store::{Begin, LabelMeta, Log, MassKind, NewOp, Rows, WriteTxn};

mod codec;
mod derived;
mod mass;
mod massops;
pub mod memory;
mod source;
mod table;
mod v10;

pub use derived::Wear;
pub use source::{KvSource, RangeRows};
use table::{Db, Wtxn};

/// The parent key of a forest root: the zero uuid, which no metarecord has.
const ROOT: [u8; 16] = [0; 16];

/// What a write transaction answers to "which ids hold a row of this field".
type HoldersOf<'a> = dyn Fn(&str) -> Result<roaring::RoaringBitmap> + 'a;
type ValueHoldersOf<'a> = dyn Fn(&str, &Value) -> Result<roaring::RoaringBitmap> + 'a;

#[derive(Clone, Copy)]
struct Tables {
    version: Db,
    global_primary_state: Db,
    metarecords: Db,
    live_metarecords: Db,
    mass: Db,
    field_types: Db,
    log_entries: Db,
    log_snaps: Db,
    log_entries_by_context: Db,
    log_entries_by_metarecord: Db,
    log_contexts: Db,
    log_context_children: Db,
    log_label_keys: Db,
    log_reverted_by: Db,
    log_watcher_entries: Db,
    log_user_entries: Db,
    // Derived (see `derived`).
    global_derived_state: Db,
    ids: Db,
    uuids: Db,
    index_parents: Db,
    index_long_texts: Db,
    index_parent_holders: Db,
    index_descendants: Db,
    index_value_holders: Db,
    index_trigram_pending: Db,
    index_value_counts: Db,
    index_raw_holder: Db,
    index_trigram_texts: Db,
    index_trigram_holders: Db,
    index_trigram_state: Db,
    index_histogram_rows: Db,
    index_histogram_keys: Db,
    index_pending_writes: Db,
    log_context_reverts: Db,
}

/// The tables' names, in `Tables`' order.
const NAMES: [&str; table::TABLES] = [
    "version",
    "global_primary_state",
    "metarecords",
    "live_metarecords",
    "mass",
    "field_types",
    "log_entries",
    "log_snaps",
    "log_entries_by_context",
    "log_entries_by_metarecord",
    "log_contexts",
    "log_context_children",
    "log_label_keys",
    "log_reverted_by",
    "log_watcher_entries",
    "log_user_entries",
    "global_derived_state",
    "ids",
    "uuids",
    "index_parents",
    "index_long_texts",
    "index_parent_holders",
    "index_descendants",
    "index_value_holders",
    "index_trigram_pending",
    "index_value_counts",
    "index_raw_holder",
    "index_trigram_texts",
    "index_trigram_holders",
    "index_trigram_state",
    "index_histogram_rows",
    "index_histogram_keys",
    "index_pending_writes",
    "log_context_reverts",
];

impl Tables {
    /// Opens (creating them if needed) the store's tables in `env`.
    fn open(env: &Env<WithoutTls>) -> Result<Tables> {
        // In `Tables`' order. heed hands a database out for good (`'static`)
        // only once the transaction that created it has committed.
        let w = env.write_txn()?;
        let created = env.create_databases_and_commit(w, |dbo, w| -> heed::Result<_> {
            let mut dbs = Vec::with_capacity(NAMES.len());
            for name in NAMES {
                dbs.push(dbo.create_database::<Bytes, Bytes>(w, Some(name))?);
            }
            let dbs: [_; table::TABLES] = dbs.try_into().expect("one database per name");
            Ok(CommitOrAbort::<_, ()>::Commit(dbs))
        })?;
        let dbs = created.commit().expect("the databases are committed");
        let mut index = 0..;
        let dbs = dbs.map(|db| Db::new(db, index.next().expect("unbounded")));
        let [version, global_primary_state, metarecords, live_metarecords, mass, field_types, log_entries, log_snaps, log_entries_by_context, log_entries_by_metarecord, log_contexts, log_context_children, log_label_keys, log_reverted_by, log_watcher_entries, log_user_entries, global_derived_state, ids, uuids, index_parents, index_long_texts, index_parent_holders, index_descendants, index_value_holders, index_trigram_pending, index_value_counts, index_raw_holder, index_trigram_texts, index_trigram_holders, index_trigram_state, index_histogram_rows, index_histogram_keys, index_pending_writes, log_context_reverts] =
            dbs;
        Ok(Tables {
            version,
            global_primary_state,
            metarecords,
            live_metarecords,
            mass,
            field_types,
            log_entries,
            log_snaps,
            log_entries_by_context,
            log_entries_by_metarecord,
            log_contexts,
            log_context_children,
            log_label_keys,
            log_reverted_by,
            log_watcher_entries,
            log_user_entries,
            global_derived_state,
            ids,
            uuids,
            index_parents,
            index_long_texts,
            index_parent_holders,
            index_descendants,
            index_value_holders,
            index_trigram_pending,
            index_value_counts,
            index_raw_holder,
            index_trigram_texts,
            index_trigram_holders,
            index_trigram_state,
            index_histogram_rows,
            index_histogram_keys,
            index_pending_writes,
            log_context_reverts,
        })
    }

    /// Every table, in the order of their indexes (see [`table::Change`]).
    fn all(&self) -> [Db; table::TABLES] {
        [
            self.version,
            self.global_primary_state,
            self.metarecords,
            self.live_metarecords,
            self.mass,
            self.field_types,
            self.log_entries,
            self.log_snaps,
            self.log_entries_by_context,
            self.log_entries_by_metarecord,
            self.log_contexts,
            self.log_context_children,
            self.log_label_keys,
            self.log_reverted_by,
            self.log_watcher_entries,
            self.log_user_entries,
            self.global_derived_state,
            self.ids,
            self.uuids,
            self.index_parents,
            self.index_long_texts,
            self.index_parent_holders,
            self.index_descendants,
            self.index_value_holders,
            self.index_trigram_pending,
            self.index_value_counts,
            self.index_raw_holder,
            self.index_trigram_texts,
            self.index_trigram_holders,
            self.index_trigram_state,
            self.index_histogram_rows,
            self.index_histogram_keys,
            self.index_pending_writes,
            self.log_context_reverts,
        ]
    }
}

/// A repository database on LMDB.
pub struct KvStore {
    env: Env<WithoutTls>,
    t: Tables,
    /// The store on disk behind a copy in memory, while it is held there (doc
    /// "Holding a repository in memory"): `env` and `t` are then the copy's.
    /// Before `_lock`: the store on disk closes before its lock is let go.
    memory: Option<memory::Memory>,
    /// Says who holds the store, for a borrow that cannot see the lock across
    /// a network mount (doc "Cross-machine sync"). Before `_lock`: removed
    /// before the lock is let go.
    _holder: crate::borrow::HolderGuard,
    /// Held for the store's lifetime: one daemon per repository.
    _lock: File,
    /// Where the store lives: the free space of its filesystem is the room
    /// a write transaction starts with ([`map_to_grant`]).
    dir: std::path::PathBuf,
    /// Keys read so far by the store's reads and its query sources — what
    /// the cost assertions count (`tests/kv_cost.rs`).
    reads: std::sync::atomic::AtomicU64,
    /// Whether any part has entries pending (doc "Mass writes in the store"):
    /// when none does, no read looks for them.
    mass: std::sync::atomic::AtomicBool,
    /// Whether a deletion of many records is pending, whose records are still
    /// stored: when none is, no read sifts what it finds through the universe.
    deletes: std::sync::atomic::AtomicBool,
    /// Whether records given back by the undo of a deletion still hold the
    /// rows the log reads as its before: when none does, no write looks for
    /// them (doc "Mass operations in the log").
    thawed: std::sync::atomic::AtomicBool,
    /// The keys the last committed write transaction wrote, per table — what
    /// the write cost assertions count ([`KvStore::last_write_keys`]).
    written: std::sync::Mutex<[u64; table::TABLES]>,
    /// How many write transactions committed: what a memo of the committed
    /// store is valid for.
    generation: std::sync::atomic::AtomicU64,
    /// The stubs of the log, decoded once for every read of a record's history
    /// until the next write (doc "Mass operations in the log").
    stub_memo: std::sync::Mutex<massops::StubMemo>,
    /// Where the copy is, shared with whoever reports it, so that reading it
    /// never waits on the store.
    resident: memory::ResidencyHandle,
    /// Whether each commit is synced: what the store is opened again with
    /// after a [`KvStore::compact`].
    durable: bool,
}

/// A [`KvStore::compact`] that closed the store and could not open it again:
/// the store now reads an empty stand-in, and its repository must be taken
/// out of service — the file itself is whole, and a load opens it.
#[derive(Debug)]
pub struct StoreClosed(pub String);

impl std::fmt::Display for StoreClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StoreClosed {}

/// What a [`KvStore::compact`] did: the store file's size before and after.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Compacted {
    pub before: u64,
    pub after: u64,
}

/// A directory that removes itself when dropped, for the unit tests' stores.
#[cfg(test)]
pub(crate) struct TestDir(pub std::path::PathBuf);

#[cfg(test)]
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A fresh store for a unit test, in `$TMPDIR/metafolder-tests/`, unsynced.
/// Keep the directory bound as long as the store.
#[cfg(test)]
pub(crate) fn test_store() -> (KvStore, TestDir) {
    let dir =
        std::env::temp_dir().join("metafolder-tests").join(format!("unit_kv_{}", Uuid::new_v4()));
    let kv = KvStore::open_unsynced(&dir).expect("open a test store");
    (kv, TestDir(dir))
}

// ── Encoding ────────────────────────────────────────────────────────────────

fn be(n: i64) -> [u8; 8] {
    (n as u64).to_be_bytes()
}

fn from_be(b: &[u8]) -> i64 {
    u64::from_be_bytes(b[..8].try_into().expect("an 8-byte integer")) as i64
}

fn uuid_of(b: &[u8]) -> Uuid {
    Uuid::from_slice(&b[..16]).expect("a 16-byte uuid")
}

fn key(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

/// A key of the state tables, each value one big-endian `i64` (doc "Store
/// tables"): the format versions in `version`, the counters and pointers of
/// the primary data in `global_primary_state`, those of the indexes in
/// `global_derived_state`. A key that is absent reads as its default.
#[derive(Clone, Copy, Debug)]
enum State {
    /// The format of the primary tables ([`LAYOUT`]).
    PrimaryVersion,
    /// The format of the derived tables (`derived::DERIVED_VERSION`).
    DerivedVersion,
    /// The id of the last operation in the log; absent, an empty log.
    LogHead,
    /// The next operation id, never reused.
    NextLogOp,
    /// The next log context id, from 1, never reused.
    NextLogContext,
    /// The context the watcher's writes hang under.
    WatcherLogContext,
    /// The clock of the writes: what a pending write, a record's numbers and
    /// a pending index write are ordered by (doc "Mass writes in the store").
    NextWriteNumber,
    /// How many times the dense ids were numbered: a page's cursor is bound
    /// to it (doc "Pagination and sorting").
    NextReindex,
    /// The next dense id.
    NextMetarecordId,
}

impl State {
    fn name(self) -> &'static [u8] {
        match self {
            State::PrimaryVersion => b"primary_version",
            State::DerivedVersion => b"derived_version",
            State::LogHead => b"log_head",
            State::NextLogOp => b"next_log_op",
            State::NextLogContext => b"next_log_context",
            State::WatcherLogContext => b"watcher_log_context",
            State::NextWriteNumber => b"next_write_number",
            State::NextReindex => b"next_reindex",
            State::NextMetarecordId => b"next_metarecord_id",
        }
    }

    fn db(self, t: &Tables) -> Db {
        match self {
            State::PrimaryVersion | State::DerivedVersion => t.version,
            State::NextMetarecordId => t.global_derived_state,
            _ => t.global_primary_state,
        }
    }

    fn get(self, t: &Tables, r: &RoTxn<'_>) -> Result<Option<i64>> {
        Ok(self.db(t).get(r, self.name())?.map(from_be))
    }

    fn put(self, t: &Tables, w: &mut table::Wtxn<'_>, n: i64) -> Result<()> {
        self.db(t).put(w, self.name(), &be(n))?;
        Ok(())
    }

    fn delete(self, t: &Tables, w: &mut table::Wtxn<'_>) -> Result<()> {
        self.db(t).delete(w, self.name())?;
        Ok(())
    }
}

/// The version of the primary layout this code writes, `layout` in `meta`
/// (absent: the first one, rows keyed `uuid · row id` in `cells`; 2: the
/// history tree, operations chained to a parent; 3: the sequence, before the
/// operations were split by origin; 4: operations grouped by write; 5:
/// before the reverts of an operation were indexed; 6: a forest entry
/// without its metarecord's dense id; 7: a forest entry holding uuid · row id
/// · dense id, the rows one key each in `field_cells`; 8: a record's rows under
/// one key, its dense id, in `records`; 9: the labels a label reverts, in
/// `label_reverts`; 10: an operation keeps its version delta and not the rows
/// it left; 11: the review of the store's tables, values in the compact
/// encoding — doc "Store tables").
const LAYOUT: i64 = 11;

/// A name inside a composite key: escaped, then terminated.
fn name_key(name: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(name.len() + 2);
    for &b in name.as_bytes() {
        out.push(b);
        if b == 0 {
            out.push(0xFF);
        }
    }
    out.extend_from_slice(&[0, 0]);
    out
}

/// The longest node name a forest key holds whole. LMDB refuses a key over
/// 511 bytes; a longer name is keyed by its first `NODE_MAX` bytes and a hash
/// of the whole — a key `NODE_MAX + 8` bytes long, which no whole name is —
/// and read back from its row.
const NODE_MAX: usize = 300;

fn node_key(name: &[u8]) -> Cow<'_, [u8]> {
    if name.len() <= NODE_MAX {
        Cow::Borrowed(name)
    } else {
        let hash = xxhash_rust::xxh3::xxh3_64(name).to_be_bytes();
        Cow::Owned([&name[..NODE_MAX], &hash[..]].concat())
    }
}

/// A range of a table's entries, either way.
type Entries<'t> = Box<dyn Iterator<Item = heed::Result<(&'t [u8], &'t [u8])>> + 't>;

/// A growable byte buffer with length-prefixed fields.
#[derive(Default)]
struct Out(Vec<u8>);

impl Out {
    fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.0.extend_from_slice(&(b.len() as u32).to_be_bytes());
        self.0.extend_from_slice(b);
        self
    }
    fn opt_bytes(&mut self, b: Option<&[u8]>) -> &mut Self {
        match b {
            None => self.0.push(0),
            Some(b) => {
                self.0.push(1);
                self.bytes(b);
            }
        }
        self
    }
    fn int(&mut self, n: i64) -> &mut Self {
        self.0.extend_from_slice(&n.to_be_bytes());
        self
    }
    fn opt_int(&mut self, n: Option<i64>) -> &mut Self {
        match n {
            None => self.0.push(0),
            Some(n) => {
                self.0.push(1);
                self.int(n);
            }
        }
        self
    }
}

struct In<'a>(&'a [u8]);

impl<'a> In<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.0.len() < n {
            bail!("a truncated record in the key-value store");
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Ok(head)
    }
    fn bytes(&mut self) -> Result<&'a [u8]> {
        let n = u32::from_be_bytes(self.take(4)?.try_into()?) as usize;
        self.take(n)
    }
    fn string(&mut self) -> Result<String> {
        Ok(String::from_utf8(self.bytes()?.to_vec())?)
    }
    fn opt_bytes(&mut self) -> Result<Option<&'a [u8]>> {
        Ok(match self.take(1)?[0] {
            0 => None,
            _ => Some(self.bytes()?),
        })
    }
    fn opt_string(&mut self) -> Result<Option<String>> {
        self.opt_bytes()?.map(|b| Ok(String::from_utf8(b.to_vec())?)).transpose()
    }
    fn int(&mut self) -> Result<i64> {
        Ok(i64::from_be_bytes(self.take(8)?.try_into()?))
    }
    fn opt_int(&mut self) -> Result<Option<i64>> {
        Ok(match self.take(1)?[0] {
            0 => None,
            _ => Some(self.int()?),
        })
    }
}

/// A value with its type, in the compact encoding (doc "Value encoding in
/// the store").
fn put_value(out: &mut Out, v: &Value) {
    let mut e = codec::Enc(std::mem::take(&mut out.0));
    e.value(v);
    out.0 = e.0;
}

fn get_value(r: &mut In) -> Result<Value> {
    let mut d = codec::Dec(r.0);
    let v = d.value()?;
    r.0 = d.0;
    Ok(v)
}

/// A row as stored. `id` is the store's own handle on a cell, until the cells
/// give way to one key per record (doc "Store tables"); a snapshot writes `0`.
/// A snapshot's row: the column an older store wrote a row id in holds `0`.
fn enc_row(row: &FieldRow) -> Vec<u8> {
    let mut out = Out::default();
    out.int(0).bytes(row.name.as_bytes());
    put_value(&mut out, &row.value);
    out.0
}

/// A snapshot's row, whatever id an older store wrote with it.
fn dec_row(b: &[u8]) -> Result<FieldRow> {
    let mut r = In(b);
    r.int()?;
    Ok(FieldRow { name: r.string()?, value: get_value(&mut r)? })
}

/// A record of rows no pending write names, the value of its key in
/// `records` ([`mass::Record`]).
fn enc_rows(rows: &[FieldRow]) -> Vec<u8> {
    mass::Record::plain(rows.to_vec()).encode()
}

fn enc_op(op: &NewOp, label: i64, timestamp: i64) -> Vec<u8> {
    let mut out = Out::default();
    out.int(label)
        .int(timestamp)
        .opt_bytes(op.origin.as_deref().map(str::as_bytes))
        .bytes(op.op_type.as_str().as_bytes())
        .bytes(op.entity.as_bytes())
        .opt_bytes(op.field_name.as_deref().map(str::as_bytes))
        .opt_int(op.reverts_op_id);
    out.0
}

fn dec_op(id: i64, b: &[u8]) -> Result<OpRow> {
    let mut r = In(b);
    let (label_id, timestamp, origin) = (r.int()?, r.int()?, r.opt_string()?);
    let (op_type, entity_uuid) = (r.string()?, uuid_of(r.bytes()?));
    Ok(OpRow {
        id,
        label_id,
        timestamp,
        origin,
        op_type,
        entity_uuid,
        field_name: r.opt_string()?,
        reverts_op_id: r.opt_int()?,
    })
}

/// An operation's timestamp, from its stored form (the second field).
fn op_timestamp(v: &[u8]) -> Result<i64> {
    let mut r = In(v);
    r.int()?;
    r.int()
}

/// A label, and the key of the command that opened it, if one did, in the
/// compact form ([`codec::enc_context`]).
fn enc_label(m: &LabelMeta, key: Option<Uuid>) -> Vec<u8> {
    codec::enc_context(m, key)
}

/// The command key [`enc_label`] stored after the label.
fn dec_label_key(b: &[u8]) -> Result<Option<Uuid>> {
    Ok(codec::dec_context(b)?.1)
}

fn dec_label(b: &[u8]) -> Result<LabelMeta> {
    Ok(codec::dec_context(b)?.0)
}

/// The map a store opens on at the least: an address-space reservation, not
/// a size limit — the first write transaction grows it ([`map_to_grant`]).
const INITIAL_MAP: usize = 1 << 30;

/// The most room (map beyond what the store holds) one store reserves. LMDB
/// grows a map only between transactions, so the room a write transaction
/// starts with is the most it can write: that room is the free space of the
/// disk — a transaction then fails for want of room only when the disk is
/// full — but no more than this. A daemon holds one map per loaded
/// repository and a process has 128 TiB of address space in all (a fixed
/// 1 TiB map ran out of it at ~120 repositories): at 128 GiB, some 900
/// repositories fit, and one transaction may always write at least half of it.
const ROOM_CAP: usize = 128 << 30;

/// The map to grow to before a write transaction, given what the store uses,
/// its map, and the free space of its filesystem — or `None` to keep the
/// map. Room is granted in full (`min(free, ROOM_CAP)`) and renewed once it
/// falls below what must be guaranteed: all of the free space when the disk
/// has less than the cap (as the store grows, free space and room shrink
/// together, so this does not resize at every write), half the cap otherwise.
fn map_to_grant(used: usize, map: usize, free: usize) -> Option<usize> {
    let room = map.saturating_sub(used);
    let (grant, floor) = if free >= ROOM_CAP { (ROOM_CAP, ROOM_CAP / 2) } else { (free, free) };
    let wanted = page_multiple(used.saturating_add(grant));
    (room < floor && wanted > map).then_some(wanted)
}

/// Bytes available to an unprivileged writer on the filesystem holding
/// `dir`; `None` when it cannot be asked.
fn free_space(dir: &Path) -> Option<usize> {
    let st = rustix::fs::statvfs(dir).ok()?;
    Some((st.f_bavail as u128 * st.f_frsize as u128).min(usize::MAX as u128) as usize)
}

/// `n` rounded up to the system's page size, as LMDB requires of a map size.
fn page_multiple(n: usize) -> usize {
    let page = rustix::param::page_size().max(4096);
    n.div_ceil(page) * page
}

// ── Opening ─────────────────────────────────────────────────────────────────

impl KvStore {
    /// Opens (creating it if needed) the store in `dir`, and takes the
    /// repository's lock: a second daemon opening it fails.
    pub fn open(dir: &Path) -> Result<KvStore> {
        KvStore::open_with_map_size(dir, INITIAL_MAP)
    }

    /// [`KvStore::open`] on a map of at least `map_size` bytes (rounded up to
    /// a page) — and at least twice what the store already holds. The map
    /// only reserves address space; [`Begin::begin_write`] doubles it as the
    /// store fills.
    pub fn open_with_map_size(dir: &Path, map_size: usize) -> Result<KvStore> {
        KvStore::open_with(dir, map_size, true)
    }

    /// [`KvStore::open`] without an `fsync` per commit, for a store nobody
    /// needs back after a crash: a test's, a benchmark's while it is being
    /// generated. A commit is then a write to the page cache; a crash may
    /// lose the last ones, or leave the store unreadable.
    pub fn open_unsynced(dir: &Path) -> Result<KvStore> {
        KvStore::open_with(dir, INITIAL_MAP, false)
    }

    fn open_with(dir: &Path, map_size: usize, durable: bool) -> Result<KvStore> {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let lock = File::create(dir.join("daemon.lock"))?;
        {
            // An `flock` belongs to the open file description, which a `fork`
            // shares with the child until its `exec` closes it: a process
            // that spawns anything holds its own lock from another
            // descriptor for that instant, and a store closed then reopened
            // can find it taken. Such a holder lets go within milliseconds;
            // a second daemon does not, and is refused after the wait.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
            loop {
                // `flock(LOCK_EX | LOCK_NB)` on Unix.
                if lock.try_lock().is_ok() {
                    break;
                }
                if std::time::Instant::now() > deadline {
                    bail!("the repository is already open in another daemon ({})", dir.display());
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        let holder = crate::borrow::HolderGuard::write(dir)?;
        let env = open_env(dir, map_size, durable)?;
        let t = Tables::open(&env)?;
        let (derived, layout) = {
            let r = env.read_txn()?;
            (State::DerivedVersion.get(&t, &r)?, State::PrimaryVersion.get(&t, &r)?)
        };
        // A store of layout 10 or before kept its versions and counters in
        // `meta` (doc "Persisted format changes"), still read to recognise it.
        let old_meta = if layout.is_none() { read_old_meta(&env)? } else { Vec::new() };
        let old = |name: &[u8]| old_meta.iter().find(|(k, _)| k == name).map(|(_, v)| *v);
        let (derived, layout) = match old(b"layout") {
            Some(layout) => (old(b"derived"), Some(layout)),
            None => (derived, layout),
        };
        let mut store = KvStore {
            env,
            t,
            _holder: holder,
            _lock: lock,
            dir: dir.to_path_buf(),
            reads: Default::default(),
            mass: Default::default(),
            deletes: Default::default(),
            thawed: Default::default(),
            written: std::sync::Mutex::new([0; table::TABLES]),
            generation: Default::default(),
            stub_memo: Default::default(),
            memory: None,
            resident: Default::default(),
            durable,
        };
        {
            let r = store.env.read_txn()?;
            let given_back =
                store.t.mass.prefix_iter(&r, &mass::THAWED_PART.to_be_bytes())?.count();
            store.thawed.store(given_back > 0, std::sync::atomic::Ordering::Relaxed);
            let pending = store.t.mass.len(&r)? as usize > given_back;
            store.mass.store(pending, std::sync::atomic::Ordering::Relaxed);
            let deleting =
                store.t.mass.prefix_iter(&r, &mass::DELETES_PART.to_be_bytes())?.next().is_some();
            store.deletes.store(deleting, std::sync::atomic::Ordering::Relaxed);
        }
        let mut derived = derived;
        if layout != Some(LAYOUT) {
            let rebuild = store
                .migrate_layout(layout, &old_meta)
                .context("migrate the key-value store's rows")?;
            if rebuild {
                derived = None;
            }
        }
        store.remove_older_tables().context("remove the tables of the older layouts")?;
        // A store from before the derived key spaces (or of another format of
        // them) gets them derived now: their migration.
        if derived != Some(derived::DERIVED_VERSION) {
            store.reindex().context("derive the key-value store's indexes")?;
        }
        Ok(store)
    }
}

/// The keys of the `meta` table of a store of layout 10 or before, and their
/// values; none when the store has no such table.
fn read_old_meta(env: &Env<WithoutTls>) -> Result<Vec<(Vec<u8>, i64)>> {
    let r = env.read_txn()?;
    let meta = env
        .open_databases_and_commit(r, |dbo, r| -> heed::Result<_> {
            Ok(CommitOrAbort::<_, ()>::Commit(dbo.open_database::<Bytes, Bytes>(r, Some("meta"))?))
        })?
        .commit()
        .expect("the database is committed");
    let Some(meta) = meta else { return Ok(Vec::new()) };
    let r = env.read_txn()?;
    let mut out = Vec::new();
    for e in meta.iter(&r)? {
        let (k, v) = e?;
        out.push((k.to_vec(), from_be(v)));
    }
    Ok(out)
}

/// Opens the LMDB environment in `dir`, on a map of at least `map_size`
/// bytes and twice what it holds; `durable`: an `fsync` per commit. The
/// caller holds the repository's lock.
fn open_env(dir: &Path, map_size: usize, durable: bool) -> Result<Env<WithoutTls>> {
    // SAFETY: the repository's lock makes this process the file's only
    // opener; the map is read-only (no WRITEMAP), so a stray write in the
    // process cannot reach it (doc "Storage safety").
    unsafe {
        let held = std::fs::metadata(dir.join("data.mdb")).map_or(0, |m| m.len() as usize);
        let map_size = page_multiple(map_size.max(held.saturating_mul(2)));
        let mut options = EnvOpenOptions::new().read_txn_without_tls();
        options.map_size(map_size).max_dbs(64);
        if !durable {
            // SAFETY: NO_SYNC trades durability for speed, which is the
            // caller's stated choice (`open_unsynced`, or a copy in memory,
            // which a crash loses anyway).
            options.flags(heed::EnvFlags::NO_SYNC);
        }
        options.open(dir)
    }
    .with_context(|| format!("open the key-value store in {}", dir.display()))
}

// ── Reading (shared by the store and its write transactions) ──────────────────

struct Read<'a> {
    t: &'a Tables,
    r: &'a RoTxn<'a>,
    /// The store's read counter; `None` where nothing counts.
    reads: Option<&'a std::sync::atomic::AtomicU64>,
    /// Whether a dense id is in the universe as a write transaction sees it
    /// (its own changes included); `None` for a read of the committed store,
    /// which reads the universe itself ([`Read::live`]).
    live: Option<&'a dyn Fn(u32) -> Result<bool>>,
    /// The ids holding a row of a field as a write transaction sees them (its
    /// own changes included); `None` for a read of the committed store, which
    /// reads the field's bitmaps itself ([`Read::holder_ids`]).
    holders: Option<&'a HoldersOf<'a>>,
    /// The ids holding a value of a field as a write transaction sees them;
    /// `None` for a read of the committed store ([`Read::value_holder_ids`]).
    values: Option<&'a ValueHoldersOf<'a>>,
    /// Whether any part has entries pending, which a record is read with
    /// applied ([`mass`]).
    mass: bool,
    /// The universe's chunks read so far, when the read looks them up itself.
    universe: RefCell<HashMap<u16, roaring::RoaringBitmap>>,
    /// The store's memo of decoded stubs, and the generation the read sees;
    /// `None` in a write transaction, whose writes the memo does not know.
    stubs: Option<(&'a std::sync::Mutex<massops::StubMemo>, u64)>,
}

impl Read<'_> {
    fn meta(&self, key: State) -> Result<Option<i64>> {
        key.get(self.t, self.r)
    }

    /// Whether a metarecord exists: its uuid has a dense id, which is live
    /// (doc "Store tables").
    fn exists(&self, uuid: Uuid) -> Result<bool> {
        self.count(1)?;
        match derived::id_of(self.t, self.r, uuid.as_bytes())? {
            Some(id) => self.live(id),
            None => Ok(false),
        }
    }

    /// Whether the metarecord of a dense id exists: whether the id is in the
    /// universe. What a deletion leaves behind — a forest key, index entries
    /// — names an id that is not (doc "Deleting a metarecord in the store").
    fn live(&self, id: u32) -> Result<bool> {
        if let Some(live) = self.live {
            return live(id);
        }
        let chunk = (id >> 16) as u16;
        let mut seen = self.universe.borrow_mut();
        if let std::collections::hash_map::Entry::Vacant(slot) = seen.entry(chunk) {
            self.count(1)?;
            slot.insert(match self.t.live_metarecords.get(self.r, &chunk.to_be_bytes())? {
                Some(bytes) => derived::decode_set(bytes)?,
                None => roaring::RoaringBitmap::new(),
            });
        }
        Ok(seen[&chunk].contains(id))
    }

    /// Whether an `index_raw_holder` entry (`v`: the child's dense id) is the
    /// position of a metarecord that exists: what tells it from one a deleted
    /// metarecord left behind, without a read (doc "Deleting a metarecord in
    /// the store").
    fn placed(&self, v: &[u8]) -> Result<bool> {
        self.live(derived::dense(v))
    }

    /// The prefix of the positions under `parent` in `field`, `None` when
    /// the parent has no dense id (nothing is under it).
    fn raw_prefix_of(&self, field: &str, parent: &[u8; 16]) -> Result<Option<Vec<u8>>> {
        let pid = if *parent == ROOT {
            Some(derived::NO_TARGET)
        } else {
            self.count(1)?;
            derived::id_of(self.t, self.r, parent)?
        };
        Ok(pid.map(|pid| derived::raw_prefix(field, pid)))
    }

    /// The rows of the record of dense id `id`: its one key. None for a
    /// metarecord a pending deletion took out of the universe.
    fn record(&self, id: u32) -> Result<Vec<FieldRow>> {
        // A deleted metarecord's record stays until a reindex: every read of
        // one is sifted (doc "Deleting a metarecord in the store").
        if !self.live(id)? {
            return Ok(Vec::new());
        }
        self.record_raw(id)
    }

    /// The rows of the record of dense id `id` whether or not it still exists:
    /// what a deletion's operation says its record held.
    pub(super) fn record_raw(&self, id: u32) -> Result<Vec<FieldRow>> {
        mass::read_record(self.t, self.r, id, self.mass, &mut |n| self.count(n))
    }

    /// A record's rows, in no particular order.
    fn rows(&self, uuid: Uuid) -> Result<Vec<FieldRow>> {
        self.count(1)?;
        match derived::id_of(self.t, self.r, uuid.as_bytes())? {
            Some(id) => self.record(id),
            None => Ok(Vec::new()),
        }
    }

    /// A record's rows of one field: the record, read whole.
    fn rows_named(&self, uuid: Uuid, name: &str) -> Result<Vec<FieldRow>> {
        Ok(self.rows(uuid)?.into_iter().filter(|r| r.name == name).collect())
    }

    /// Counts `n` keys read; fails once the running query was asked to stop
    /// (`crate::interrupt`) — a read outside a query never is.
    fn count(&self, n: u64) -> Result<()> {
        // Before counting: a read refused is not a read.
        crate::interrupt::check(n)?;
        if let Some(reads) = self.reads {
            reads.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
            metafolder_core::slowlog::count_reads(n);
        }
        Ok(())
    }

    /// The dense ids of the metarecords holding a row of `name`, deleted
    /// ones possibly among them (they leave the universe and nothing else):
    /// the field's counts and its absences, as the transaction sees them when
    /// a write asks.
    fn holder_ids(&self, name: &str) -> Result<roaring::RoaringBitmap> {
        if let Some(holders) = self.holders {
            return holders(name);
        }
        let mut ids = derived::read_set(self.t, self.r, derived::ABSENT, Some(name))?;
        for bit in derived::read_counts(self.t, self.r, name)? {
            ids |= bit;
        }
        self.count(1)?;
        Ok(ids)
    }

    /// The ids holding `value` in `name` ([`Rows::value_holder_ids`]).
    fn value_holder_ids(&self, name: &str, value: &Value) -> Result<roaring::RoaringBitmap> {
        if let Some(values) = self.values {
            return values(name, value);
        }
        self.count(1)?;
        let Some(key) = derived::value_key(value) else {
            return derived::read_set(self.t, self.r, derived::ABSENT, Some(name));
        };
        let stored =
            derived::read_prefix_set(self.t, self.r, &derived::posting_prefix(name, &key))?;
        let pending = derived::events::read_events(self.t, self.r, name)?;
        Ok(if pending.is_empty() {
            stored
        } else {
            derived::events::replay(&pending, &key, stored)
        })
    }

    /// The uuid of a dense id.
    fn uuid_of_id(&self, id: u32) -> Result<Uuid> {
        self.count(1)?;
        let uuid = self.t.uuids.get(self.r, &id.to_be_bytes())?;
        Ok(uuid_of(uuid.with_context(|| format!("dense id {id} names no metarecord"))?))
    }

    /// The metarecords holding a row of `name`, in id order.
    fn holders(&self, name: &str) -> Result<Vec<Uuid>> {
        let mut out = Vec::new();
        for id in self.holder_ids(name)? {
            if self.live(id)? {
                out.push(self.uuid_of_id(id)?);
            }
        }
        Ok(out)
    }

    fn children(&self, field: &str, parent: &[u8; 16]) -> Result<Vec<(Uuid, Vec<u8>)>> {
        let Some(prefix) = self.raw_prefix_of(field, parent)? else { return Ok(Vec::new()) };
        let mut out = Vec::new();
        for e in self.t.index_raw_holder.prefix_iter(self.r, &prefix)? {
            let (k, v) = e?;
            // Counted as read, not after: a folder can hold the repository,
            // and a query stopped meanwhile stops here (`crate::interrupt`).
            self.count(1)?;
            if !self.placed(v)? {
                continue;
            }
            let child = derived::dense(v);
            out.push((self.uuid_of_id(child)?, self.node_name(field, &k[prefix.len()..], v)?));
        }
        self.count(1)?;
        Ok(out)
    }

    /// [`Rows::children_page`] over the forest table, which orders a
    /// parent's children by their name bytes: the page, and nothing else.
    fn children_page(
        &self,
        field: &str,
        parent: &[u8; 16],
        after: Option<&[u8]>,
        descending: bool,
        limit: usize,
    ) -> Result<Vec<(Uuid, Vec<u8>)>> {
        let Some(prefix) = self.raw_prefix_of(field, parent)? else { return Ok(Vec::new()) };
        let bound = after.map(|a| key(&[&prefix, &node_key(a)]));
        let top = key(&[&prefix, &[0xFF; 400]]);
        let entries: Entries<'_> = if descending {
            let hi = match &bound {
                Some(b) => std::ops::Bound::Excluded(b.as_slice()),
                None => std::ops::Bound::Excluded(top.as_slice()),
            };
            let range = (std::ops::Bound::Included(prefix.as_slice()), hi);
            Box::new(self.t.index_raw_holder.rev_range(self.r, &range)?)
        } else {
            let lo = match &bound {
                Some(b) => std::ops::Bound::Excluded(b.as_slice()),
                None => std::ops::Bound::Included(prefix.as_slice()),
            };
            let range = (lo, std::ops::Bound::Excluded(top.as_slice()));
            Box::new(self.t.index_raw_holder.range(self.r, &range)?)
        };
        let mut out = Vec::new();
        for entry in entries {
            let (k, v) = entry?;
            if !k.starts_with(&prefix) {
                break;
            }
            if !self.placed(v)? {
                continue;
            }
            let child = derived::dense(v);
            out.push((self.uuid_of_id(child)?, self.node_name(field, &k[prefix.len()..], v)?));
            if out.len() >= limit {
                break;
            }
        }
        self.count(out.len() as u64 + 1)?;
        Ok(out)
    }

    /// A position's name, from the end of its `index_raw_holder` key — or,
    /// for a name too long to be keyed whole, from its record's row (`v` is
    /// the child's dense id).
    fn node_name(&self, field: &str, key_end: &[u8], v: &[u8]) -> Result<Vec<u8>> {
        if key_end.len() <= NODE_MAX {
            return Ok(key_end.to_vec());
        }
        let rows = self.record(derived::dense(v))?;
        rows.iter()
            .filter(|r| r.name == field)
            .find_map(|r| match r.value.position() {
                Some((_, name)) if *node_key(name.as_bytes()) == *key_end => {
                    Some(name.as_bytes().to_vec())
                }
                _ => None,
            })
            .context("a forest position without its row")
    }

    fn op(&self, id: i64) -> Result<Option<OpRow>> {
        self.count(1)?;
        match self.t.log_entries.get(self.r, &be(id))? {
            Some(b) => Ok(Some(match massops::Stub::decode(id, b)? {
                Some(stub) => stub.op(self, id)?,
                None => dec_op(id, b)?,
            })),
            None => self.stub_at(id)?.map(|stub| stub.op(self, id)).transpose(),
        }
    }

    fn ops_of(&self, ids: impl Iterator<Item = i64>) -> Result<Vec<OpRow>> {
        self.ops_resolved(ids)
    }

    /// The operations at or before `from`, at most `max`, newest first: a
    /// backward range of `ops`.
    fn back(&self, from: i64, max: usize) -> Result<Vec<OpRow>> {
        let mut out = Vec::new();
        let end = be(from);
        let range = (std::ops::Bound::Unbounded, std::ops::Bound::Included(&end[..]));
        for e in self.t.log_entries.rev_range(self.r, &range)? {
            if out.len() >= max {
                break;
            }
            self.count(1)?;
            let (k, v) = e?;
            let id = from_be(k);
            match massops::Stub::decode(id, v)? {
                Some(stub) => {
                    for at in (id..=stub.last().min(from)).rev().take(max - out.len()) {
                        out.push(stub.op(self, at)?);
                    }
                }
                None => out.push(dec_op(id, v)?),
            }
        }
        Ok(out)
    }

    fn all_op_ids(&self) -> Result<Vec<i64>> {
        self.ids_from(0, usize::MAX)
    }

    fn label_op_ids(&self, label: i64) -> Result<Vec<i64>> {
        let mut out = Vec::new();
        for e in self.t.log_entries_by_context.prefix_iter(self.r, &be(label))? {
            let (k, v) = e?;
            let first = from_be(&k[8..]);
            // A stub's entry says where its operations end.
            let last = if v.is_empty() { first } else { from_be(v) };
            out.extend(first..=last);
        }
        self.count(out.len() as u64 + 1)?;
        Ok(out)
    }

    /// A label's oldest operation of its own, `None` when it holds none: one
    /// key.
    fn label_first_op(&self, label: i64) -> Result<Option<i64>> {
        self.count(1)?;
        Ok(match self.t.log_entries_by_context.prefix_iter(self.r, &be(label))?.next() {
            Some(e) => Some(from_be(&e?.0[8..])),
            None => None,
        })
    }

    /// The ids of the labels older than `before` (all when none), newest first,
    /// at most `max`: the end of a table keyed by id.
    fn label_ids_before(&self, before: Option<i64>, max: usize) -> Result<Vec<i64>> {
        let hi = be(before.map_or(i64::MAX, |b| b.saturating_sub(1)));
        let range = (std::ops::Bound::Unbounded, std::ops::Bound::Included(&hi[..]));
        let mut out = Vec::new();
        for e in self.t.log_contexts.rev_range(self.r, &range)?.take(max) {
            out.push(from_be(e?.0));
        }
        self.count(out.len() as u64 + 1)?;
        Ok(out)
    }

    /// How many operations a label holds of its own, off the keys of
    /// `ops_by_label` — one per operation, one per stub, which says its last
    /// id — so that no operation is read, nor any record a stub names.
    fn label_op_count(&self, label: i64) -> Result<i64> {
        let (mut n, mut seen) = (0i64, 0u64);
        for e in self.t.log_entries_by_context.prefix_iter(self.r, &be(label))? {
            let (k, v) = e?;
            seen += 1;
            n += if v.len() == 8 { from_be(v) - from_be(&k[8..]) + 1 } else { 1 };
        }
        self.count(seen + 1)?;
        Ok(n)
    }

    /// A label's newest operation of its own.
    fn label_last_op(&self, label: i64) -> Result<Option<i64>> {
        let (lo, hi) = (key(&[&be(label), &be(0)]), key(&[&be(label), &be(i64::MAX)]));
        let range = (std::ops::Bound::Included(&lo[..]), std::ops::Bound::Included(&hi[..]));
        self.count(1)?;
        Ok(match self.t.log_entries_by_context.rev_range(self.r, &range)?.next() {
            Some(e) => {
                let (k, v) = e?;
                Some(if v.len() == 8 { from_be(v) } else { from_be(&k[8..]) })
            }
            None => None,
        })
    }

    /// The labels `label` reverts (`kind` 0), or the labels that revert it
    /// (`kind` 1), each with whether it is the whole label.
    fn label_marks(&self, kind: u8, label: i64) -> Result<Vec<(i64, bool)>> {
        let mut out = Vec::new();
        for e in self.t.log_context_reverts.prefix_iter(self.r, &key(&[&[kind], &be(label)]))? {
            let (k, v) = e?;
            out.push((from_be(&k[9..]), v.first() == Some(&1)));
        }
        self.count(out.len() as u64 + 1)?;
        Ok(out)
    }

    fn label_children(&self, label: i64) -> Result<Vec<i64>> {
        let mut out = Vec::new();
        for e in self.t.log_context_children.prefix_iter(self.r, &be(label))? {
            out.push(from_be(&e?.0[8..]));
        }
        self.count(out.len() as u64 + 1)?;
        Ok(out)
    }

    /// The newest operation written at or before `timestamp_ms`. Timestamps
    /// grow with the ids (a write takes its time under the store's lock), so
    /// this is a binary search over the ids — a few keys, whatever the log's
    /// length. A clock set back makes it approximate, never wrong by more
    /// than the jump.
    fn last_op_at_or_before(&self, timestamp_ms: i64) -> Result<Option<i64>> {
        self.count(2)?;
        let Some((first, v)) = self.t.log_entries.first(self.r)? else { return Ok(None) };
        if op_timestamp(v)? > timestamp_ms {
            return Ok(None);
        }
        // A write to many records is one entry holding many ids, all of its time.
        let end_of = |id: i64, v: &[u8]| -> Result<i64> {
            Ok(massops::Stub::decode(id, v)?.map_or(id, |s| s.last()))
        };
        let mut lo = end_of(from_be(first), v)?;
        let Some((last, v)) = self.t.log_entries.last(self.r)? else { return Ok(None) };
        let mut hi = end_of(from_be(last), v)?;
        if op_timestamp(v)? <= timestamp_ms {
            return Ok(Some(hi));
        }
        // `lo` is an operation at or before the time; nothing above `hi` is.
        while lo < hi {
            let mid = lo + (hi - lo + 1) / 2;
            let from = be(mid);
            let range = (std::ops::Bound::Included(&from[..]), std::ops::Bound::Unbounded);
            self.count(1)?;
            let (k, v) = self
                .t
                .log_entries
                .range(self.r, &range)?
                .next()
                .context("an operation past mid")??;
            let id = from_be(k);
            if id <= hi && op_timestamp(v)? <= timestamp_ms {
                lo = end_of(id, v)?;
            } else {
                hi = mid - 1;
            }
        }
        Ok(Some(lo))
    }
}

/// `Rows` and `Log` for anything that can lend a `Read`.
macro_rules! kv_reads {
    ($ty:ty, |$me:ident, $read:ident| $with:expr) => {
        impl Rows for $ty {
            fn as_kv(&self) -> Option<&KvStore> {
                self.kv_store()
            }
            fn exists(&self, uuid: Uuid) -> Result<bool> {
                let $me = self;
                $with(&mut |$read: &Read| $read.exists(uuid))
            }
            fn rows(&self, uuid: Uuid) -> Result<Vec<FieldRow>> {
                let $me = self;
                $with(&mut |$read: &Read| $read.rows(uuid))
            }
            fn rows_named(&self, uuid: Uuid, name: &str) -> Result<Vec<FieldRow>> {
                let $me = self;
                $with(&mut |$read: &Read| $read.rows_named(uuid, name))
            }
            fn rows_for(&self, uuids: &[Uuid]) -> Result<HashMap<Uuid, Vec<FieldRow>>> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    let mut out = HashMap::new();
                    for &u in uuids {
                        let rows = $read.rows(u)?;
                        if !rows.is_empty() {
                            out.insert(u, rows);
                        }
                    }
                    Ok(out)
                })
            }
            fn metarecord_count(&self) -> Result<usize> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    let live = derived::read_live($read.t, $read.r)?;
                    $read.count(1 + live.max().map_or(0, |m| u64::from(m >> 16)))?;
                    Ok(live.len() as usize)
                })
            }
            fn metarecords(&self) -> Result<Vec<Uuid>> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    let live = derived::read_live($read.t, $read.r)?;
                    let mut out = Vec::with_capacity(live.len() as usize);
                    for id in &live {
                        $read.count(1)?;
                        out.push($read.uuid_of_id(id)?);
                    }
                    // In uuid order, as the store kept them before the
                    // numbering was primary.
                    out.sort_unstable();
                    Ok(out)
                })
            }
            fn for_each_row(&self, f: &mut dyn FnMut(Uuid, FieldRow) -> Result<()>) -> Result<()> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    for id in mass::record_ids($read.t, $read.r, $read.mass)? {
                        // An id only an entry names may be a metarecord's that
                        // is gone.
                        if !$read.live(id)? {
                            continue;
                        }
                        let uuid = $read.uuid_of_id(id)?;
                        for row in $read.record(id)? {
                            f(uuid, row)?;
                        }
                    }
                    Ok(())
                })
            }
            fn value_types(&self, name: &str) -> Result<Vec<String>> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    let prefix = name_key(name);
                    let mut out = Vec::new();
                    for e in $read.t.field_types.prefix_iter($read.r, &prefix)? {
                        $read.count(1)?;
                        let (k, _) = e?;
                        out.push(codec::field_type(&k[prefix.len()..])?.to_string());
                    }
                    Ok(out)
                })
            }
            fn holders(&self, name: &str) -> Result<Vec<Uuid>> {
                let $me = self;
                $with(&mut |$read: &Read| $read.holders(name))
            }
            fn holder_ids(&self, name: &str) -> Result<roaring::RoaringBitmap> {
                let $me = self;
                $with(&mut |$read: &Read| $read.holder_ids(name))
            }
            fn value_holder_ids(
                &self,
                name: &str,
                value: &Value,
            ) -> Result<roaring::RoaringBitmap> {
                let $me = self;
                $with(&mut |$read: &Read| $read.value_holder_ids(name, value))
            }
            fn uuids_of_ids(&self, ids: &roaring::RoaringBitmap) -> Result<Vec<Uuid>> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    let mut out = Vec::with_capacity(ids.len() as usize);
                    for id in ids {
                        if $read.live(id)? {
                            out.push($read.uuid_of_id(id)?);
                        }
                    }
                    Ok(out)
                })
            }
            fn children_page(
                &self,
                field: &str,
                parent: Uuid,
                after: Option<&[u8]>,
                descending: bool,
                limit: usize,
            ) -> Result<Vec<(Uuid, Vec<u8>)>> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    $read.children_page(field, parent.as_bytes(), after, descending, limit)
                })
            }
            fn children(&self, field: &str, parent: Uuid) -> Result<Vec<(Uuid, String)>> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    Ok($read
                        .children(field, parent.as_bytes())?
                        .into_iter()
                        .map(|(u, n)| (u, TreeName::from_bytes(n).display().into_owned()))
                        .collect())
                })
            }
            fn children_named(&self, field: &str, parent: Uuid) -> Result<Vec<(Uuid, TreeName)>> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    Ok($read
                        .children(field, parent.as_bytes())?
                        .into_iter()
                        .map(|(u, n)| (u, TreeName::from_bytes(n)))
                        .collect())
                })
            }
            fn child_by_bytes(
                &self,
                field: &str,
                parent: Option<Uuid>,
                name: &[u8],
            ) -> Result<Option<Uuid>> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    let p = parent.map_or(ROOT, |p| *p.as_bytes());
                    let Some(prefix) = $read.raw_prefix_of(field, &p)? else { return Ok(None) };
                    let k = key(&[&prefix, &node_key(name)]);
                    $read.count(1)?;
                    let Some(v) = $read.t.index_raw_holder.get($read.r, &k)? else {
                        return Ok(None);
                    };
                    if !$read.placed(v)? {
                        return Ok(None);
                    }
                    // A hashed key names the right position only if the name
                    // read back is the one asked for.
                    let found = $read.node_name(field, &k[k.len() - node_key(name).len()..], v)?;
                    if found != name {
                        return Ok(None);
                    }
                    Ok(Some($read.uuid_of_id(derived::dense(v))?))
                })
            }
            fn child_by_text(
                &self,
                field: &str,
                parent: Option<Uuid>,
                name: &str,
                nocase: bool,
            ) -> Result<Option<Uuid>> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    let p = parent.map_or(ROOT, |p| *p.as_bytes());
                    Ok($read.children(field, &p)?.into_iter().find_map(|(u, n)| {
                        let text = TreeName::from_bytes(n).display().into_owned();
                        // The fold `forest::normalize_name` keys the rule
                        // index with — Unicode, not only ASCII — so both agree
                        // on which names a case-insensitive filesystem equates.
                        let same = if nocase {
                            text.to_lowercase() == name.to_lowercase()
                        } else {
                            text == name
                        };
                        same.then_some(u)
                    }))
                })
            }
            fn forest(&self) -> Result<Vec<TreeRow>> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    let mut out = Vec::new();
                    for e in $read.t.index_raw_holder.iter($read.r)? {
                        $read.count(1)?;
                        let (k, v) = e?;
                        if !$read.placed(v)? {
                            continue;
                        }
                        // field · parent · name bytes: the field's end is its
                        // `00 00` terminator.
                        let end = (0..k.len() - 1)
                            .find(|&i| k[i] == 0 && k[i + 1] == 0)
                            .context("a forest key without its field")?;
                        let mut field = Vec::new();
                        let mut i = 0;
                        while i < end {
                            field.push(k[i]);
                            i += if k[i] == 0 { 2 } else { 1 };
                        }
                        let field = String::from_utf8(field)?;
                        let pid = derived::dense(&k[end + 2..end + 6]);
                        let parent = match pid {
                            derived::NO_TARGET => None,
                            pid => Some($read.uuid_of_id(pid)?),
                        };
                        let name = $read.node_name(&field, &k[end + 6..], v)?;
                        out.push(TreeRow {
                            field_name: field,
                            uuid: $read.uuid_of_id(derived::dense(v))?,
                            parent,
                            name: TreeName::from_bytes(name),
                        });
                    }
                    out.sort_by(|a, b| {
                        (&a.field_name, a.uuid, a.parent, a.name.as_bytes()).cmp(&(
                            &b.field_name,
                            b.uuid,
                            b.parent,
                            b.name.as_bytes(),
                        ))
                    });
                    Ok(out)
                })
            }
        }

        impl Log for $ty {
            fn head(&self) -> Result<Option<i64>> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    $read.count(1)?;
                    $read.meta(State::LogHead)
                })
            }
            fn op(&self, id: i64) -> Result<Option<OpRow>> {
                let $me = self;
                $with(&mut |$read: &Read| $read.op(id))
            }
            fn snapshots(&self, op_id: i64) -> Result<Vec<FieldRow>> {
                let $me = self;
                $with(&mut |$read: &Read| $read.snapshot_rows(op_id))
            }
            fn added(&self, op_id: i64) -> Result<Vec<FieldRow>> {
                let $me = self;
                $with(&mut |$read: &Read| $read.added_rows(op_id))
            }
            fn first_op(&self) -> Result<Option<i64>> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    $read.count(1)?;
                    Ok($read.t.log_entries.first($read.r)?.map(|(k, _)| from_be(k)))
                })
            }
            fn ops_back(&self, from: i64, max: Option<usize>) -> Result<Vec<OpRow>> {
                let $me = self;
                $with(&mut |$read: &Read| $read.back(from, max.unwrap_or(usize::MAX)))
            }
            fn all_ops(&self) -> Result<Vec<OpRow>> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    let ids = $read.all_op_ids()?;
                    $read.ops_of(ids.into_iter())
                })
            }
            fn labels(&self, ids: &[i64]) -> Result<HashMap<i64, LabelMeta>> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    let mut out = HashMap::new();
                    $read.count(ids.len() as u64)?;
                    for &id in ids {
                        if let Some(b) = $read.t.log_contexts.get($read.r, &be(id))? {
                            out.insert(id, dec_label(b)?);
                        }
                    }
                    Ok(out)
                })
            }
            fn counts(&self) -> Result<(i64, i64)> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    // Two B-tree statistics, not a walk.
                    $read.count(2)?;
                    // The log is counted by entry, a stub one (doc "Log storage").
                    let ops = $read.t.log_entries.len($read.r)? as i64;
                    Ok((ops, $read.t.log_contexts.len($read.r)? as i64))
                })
            }
            fn label_ops(&self, label: i64) -> Result<Vec<OpRow>> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    let ids = $read.label_op_ids(label)?;
                    $read.ops_of(ids.into_iter())
                })
            }
            fn label_first_op(&self, label: i64) -> Result<Option<i64>> {
                let $me = self;
                $with(&mut |$read: &Read| $read.label_first_op(label))
            }
            fn label_children(&self, label: i64) -> Result<Vec<i64>> {
                let $me = self;
                $with(&mut |$read: &Read| $read.label_children(label))
            }
            fn label_op_count(&self, label: i64) -> Result<i64> {
                let $me = self;
                $with(&mut |$read: &Read| $read.label_op_count(label))
            }
            fn label_op_ids(&self, label: i64) -> Result<Vec<i64>> {
                let $me = self;
                $with(&mut |$read: &Read| $read.label_op_ids(label))
            }
            fn label_ids_before(&self, before: Option<i64>, max: usize) -> Result<Vec<i64>> {
                let $me = self;
                $with(&mut |$read: &Read| $read.label_ids_before(before, max))
            }
            fn label_last_op(&self, label: i64) -> Result<Option<i64>> {
                let $me = self;
                $with(&mut |$read: &Read| $read.label_last_op(label))
            }
            fn label_reverts(&self, label: i64) -> Result<Vec<(i64, bool)>> {
                let $me = self;
                $with(&mut |$read: &Read| $read.label_marks(0, label))
            }
            fn label_reverted_by(&self, label: i64) -> Result<Vec<(i64, bool)>> {
                let $me = self;
                $with(&mut |$read: &Read| $read.label_marks(1, label))
            }
            fn reverters(&self, op: i64) -> Result<Vec<i64>> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    let mut out = Vec::new();
                    for e in $read.t.log_reverted_by.prefix_iter($read.r, &be(op))? {
                        out.push(from_be(&e?.0[8..]));
                    }
                    $read.count(out.len() as u64 + 1)?;
                    Ok(out)
                })
            }
            fn label_of_command(&self, key: Uuid) -> Result<Option<i64>> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    $read.count(1)?;
                    Ok($read.t.log_label_keys.get($read.r, key.as_bytes())?.map(from_be))
                })
            }
            fn watcher_label(&self) -> Result<Option<i64>> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    $read.count(1)?;
                    $read.meta(State::WatcherLogContext)
                })
            }
            fn entity_ops_after(&self, entity: Uuid, after: i64) -> Result<Vec<OpRow>> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    let ids = $read.entity_op_ids_after(entity, after)?;
                    $read.ops_resolved_of(ids.into_iter(), Some(entity))
                })
            }
            fn version_before_label(&self, label: i64, entity: Uuid) -> Result<Option<u64>> {
                crate::store::version_before_label(self, label, entity)
            }
            fn ops_after(&self, op: i64) -> Result<Vec<OpRow>> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    let ids = $read.ids_from(op.saturating_add(1), usize::MAX)?;
                    $read.ops_of(ids.into_iter())
                })
            }
            fn ops_after_count(&self, op: i64, cap: i64) -> Result<i64> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    let cap = usize::try_from(cap).unwrap_or(0);
                    Ok($read.ids_from(op.saturating_add(1), cap)?.len() as i64)
                })
            }
            fn last_op_at_or_before(&self, timestamp_ms: i64) -> Result<Option<i64>> {
                let $me = self;
                $with(&mut |$read: &Read| $read.last_op_at_or_before(timestamp_ms))
            }
        }
    };
}

kv_reads!(KvStore, |me, read| |f: &mut dyn FnMut(&Read) -> Result<_>| {
    let r = me.env.read_txn()?;
    f(&Read {
        t: &me.t,
        r: &r,
        reads: Some(&me.reads),
        live: None,
        holders: None,
        values: None,
        mass: me.mass_pending(),
        universe: Default::default(),
        stubs: Some((&me.stub_memo, me.generation.load(std::sync::atomic::Ordering::Relaxed))),
    })
});

/// The schema check's candidate questions (doc "Schema"), off the derived key
/// spaces: up to `limit` metarecords each.
impl KvStore {
    /// Those holding a non-`Nothing` `field` row of another type than
    /// `allowed`.
    pub fn wrong_type(&self, field: &str, allowed: &str, limit: i64) -> Result<Vec<Uuid>> {
        self.source()?.wrong_type(field, allowed, limit)
    }
    /// Those with more than `max` rows of `field`.
    pub fn count_over(&self, field: &str, max: i64, limit: i64) -> Result<Vec<Uuid>> {
        self.source()?.count_over(field, max, limit)
    }
    /// Those holding `field`, with fewer than `min` rows.
    pub fn count_under(&self, field: &str, min: i64, limit: i64) -> Result<Vec<Uuid>> {
        self.source()?.count_under(field, min, limit)
    }
    /// Those with no `field` row at all.
    pub fn missing(&self, field: &str, limit: i64) -> Result<Vec<Uuid>> {
        self.source()?.missing(field, limit)
    }
    /// Those declared one of `types` (`mf_schema`) with no `field` row.
    pub fn typed_missing(&self, types: &[String], field: &str, limit: i64) -> Result<Vec<Uuid>> {
        self.source()?.typed_missing(types, field, limit)
    }
}

impl KvStore {
    /// The dense ids of metarecords, as a query names them (a tool for tests:
    /// one lookup each, which is what a write by id avoids).
    pub fn dense_ids(&self, uuids: &[Uuid]) -> Result<roaring::RoaringBitmap> {
        let r = self.env.read_txn()?;
        let mut out = roaring::RoaringBitmap::new();
        for u in uuids {
            if let Some(id) = derived::id_of(&self.t, &r, u.as_bytes())? {
                out.insert(id);
            }
        }
        Ok(out)
    }

    /// Whether a deletion of many records is pending.
    pub(super) fn deletes_pending(&self) -> bool {
        self.deletes.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The uuids of dense ids, in the order of the ids (a tool for tests).
    pub fn uuids_of(&self, ids: &roaring::RoaringBitmap) -> Result<Vec<Uuid>> {
        let r = self.env.read_txn()?;
        let mut out = Vec::new();
        for id in ids {
            let uuid = self.t.uuids.get(&r, &id.to_be_bytes())?.context("a dense id of no uuid")?;
            out.push(uuid_of(uuid));
        }
        Ok(out)
    }

    /// How many keys the store's reads and query sources have read.
    pub fn reads(&self) -> u64 {
        self.reads.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The keys the last committed write wrote, by table name (the tables it
    /// wrote nothing to left out). A write is held to the *tables* it may
    /// touch: on a hard disk each scattered key is a seek, whatever its size
    /// (`tests/kv_cost.rs`).
    pub fn last_write_keys(&self) -> std::collections::BTreeMap<&'static str, u64> {
        let written = self.written.lock().unwrap_or_else(|e| e.into_inner());
        NAMES.iter().copied().zip(*written).filter(|(_, n)| *n > 0).collect()
    }

    fn kv_store(&self) -> Option<&KvStore> {
        Some(self)
    }
}

impl KvTxn<'_> {
    /// A write transaction is not a store a query can read a snapshot of.
    fn kv_store(&self) -> Option<&KvStore> {
        None
    }

    /// Runs `f` on the transaction's reads: its own writes, and the universe
    /// as it changed it.
    fn read_in<T>(&self, txn: &Wtxn<'_>, f: &mut dyn FnMut(&Read) -> Result<T>) -> Result<T> {
        let live = |id: u32| self.live_in(txn, id);
        let holders = |field: &str| self.holder_ids_now(field);
        let values = |field: &str, value: &Value| self.value_holders_now(field, value);
        f(&Read {
            t: &self.t,
            r: txn,
            reads: Some(self.reads),
            live: Some(&live),
            holders: Some(&holders),
            values: Some(&values),
            mass: self.mass_pending(),
            universe: Default::default(),
            stubs: None,
        })
    }
}

// ── Writing ─────────────────────────────────────────────────────────────────

/// A write transaction. Its reads see its own writes.
pub struct KvTxn<'e> {
    t: Tables,
    txn: RefCell<Wtxn<'e>>,
    /// The store on disk, when the store is held in memory: what the
    /// transaction's changes are committed to first.
    memory: Option<&'e memory::Memory>,
    /// The derived set chunks this transaction changed, written at commit.
    sets: RefCell<HashMap<Vec<u8>, roaring::RoaringBitmap>>,
    /// Which chunks of each bitmap (table tag · key without chunk) the cache
    /// holds: a bitmap is read back whole without scanning the cache.
    cached_chunks: RefCell<HashMap<Vec<u8>, std::collections::BTreeSet<u16>>>,
    /// The posting chunks the cache loaded that were stored: a chunk coming
    /// or going counts in the histogram of its field (doc "Query planner").
    chunks_existing: RefCell<std::collections::HashSet<Vec<u8>>>,
    /// The store's read counter: a write's reads count as much as a query's
    /// (`tests/log_cost.rs` holds the trim to it).
    reads: &'e std::sync::atomic::AtomicU64,
    /// Where the store keeps what its last write wrote.
    written: &'e std::sync::Mutex<[u64; table::TABLES]>,
    /// The store's count of committed writes, which this one adds to.
    generation: &'e std::sync::atomic::AtomicU64,
    /// The universe's chunks the transaction read without changing them
    /// (a changed one is in `sets`).
    universe: RefCell<HashMap<u16, roaring::RoaringBitmap>>,
    /// Each field's trigram state as the transaction read or changed it, and
    /// the fields whose state it changed, written at commit.
    grams: RefCell<HashMap<String, derived::GramState>>,
    grams_changed: RefCell<std::collections::HashSet<String>>,
    /// How many events each field has pending, as read or changed.
    events: RefCell<HashMap<String, usize>>,
    /// The store's flag that some part has entries pending: a write to many
    /// records raises it.
    mass_lists: TxnFlag<'e>,
    /// The store's flag that a deletion of many records is pending.
    deletes: TxnFlag<'e>,
    /// The store's flag that records given back hold the log's before.
    thawed: TxnFlag<'e>,
}

/// A flag of the store as a write transaction sees it. The flags say what the
/// committed store holds, which reads rely on (a deletion pending is sifted
/// through the universe only while its flag is up): what the transaction sets
/// reaches the store when it commits, and is forgotten if it does not.
struct TxnFlag<'e> {
    now: std::cell::Cell<bool>,
    store: &'e std::sync::atomic::AtomicBool,
}

impl<'e> TxnFlag<'e> {
    fn of(store: &'e std::sync::atomic::AtomicBool) -> Self {
        TxnFlag {
            now: std::cell::Cell::new(store.load(std::sync::atomic::Ordering::Relaxed)),
            store,
        }
    }

    fn get(&self) -> bool {
        self.now.get()
    }

    fn set(&self, up: bool) {
        self.now.set(up);
    }

    /// The transaction committed: the store holds what it says.
    fn publish(&self) {
        self.store.store(self.now.get(), std::sync::atomic::Ordering::Relaxed);
    }
}

kv_reads!(KvTxn<'_>, |me, read| |f: &mut dyn FnMut(&Read) -> Result<_>| {
    let txn = me.txn.borrow();
    me.read_in(&txn, f)
});

impl Begin for KvStore {
    fn begin_write(&mut self) -> Result<Box<dyn WriteTxn + '_>> {
        Ok(Box::new(KvTxn::new(self)?))
    }
    fn check(&self) -> Result<Vec<String>> {
        self.check_derived()
    }
    fn reindex(&mut self) -> Result<()> {
        KvStore::reindex(self)
    }
    fn backup_to(&self, dir: &Path) -> Result<()> {
        // LMDB's own hot copy: a read snapshot written out, compacted.
        let kv = dir.join(crate::repo::KV_DIR);
        std::fs::create_dir_all(&kv).with_context(|| format!("create {}", kv.display()))?;
        self.env
            .copy_to_path(kv.join("data.mdb"), heed::CompactionOption::Enabled)
            .context("copy the key-value store")?;
        Ok(())
    }
    fn as_kv_mut(&mut self) -> Option<&mut KvStore> {
        Some(self)
    }
}

impl KvStore {
    /// Grows the maps so the write transaction about to start has the room
    /// [`map_to_grant`] promises — the disk's free space, up to a cap: a
    /// write of any size commits unless the disk itself is full. Done here
    /// because LMDB allows a resize only while the process has no transaction
    /// open — which `&mut self` guarantees: every transaction borrows the
    /// store. Held in memory, the copy and the store on disk both grow, each
    /// by the room of its own filesystem.
    fn grow_map(&mut self) -> Result<()> {
        match &self.memory {
            Some(memory) => {
                grow(&self.env, &memory.dir)?;
                grow(&memory.disk, &self.dir)
            }
            None => grow(&self.env, &self.dir),
        }
    }

    /// Copies the store into memory, under `base`, and reads it there from
    /// now on (doc "Holding a repository in memory"); `progress(done, total)`
    /// in bytes. `on_lost` is told if the copy ever falls behind the disk.
    /// Already held, nothing is done.
    pub fn hold_in_memory(
        &mut self,
        base: &Path,
        on_lost: memory::OnLost,
        progress: &dyn Fn(u64, u64),
    ) -> Result<memory::Residency> {
        if self.memory.is_none() {
            // `&mut self`: no transaction is open, the file is what was
            // committed last.
            let (dir, held) = memory::Memory::copy_in(&self.dir.join("data.mdb"), base, progress)?;
            let env = open_env(&dir, INITIAL_MAP, false);
            let opened = env.and_then(|env| Ok((Tables::open(&env)?, env)));
            let (t, env) = match opened {
                Ok(opened) => opened,
                Err(e) => {
                    let _ = std::fs::remove_dir_all(&dir);
                    return Err(e);
                }
            };
            let disk = std::mem::replace(&mut self.env, env);
            let disk_t = std::mem::replace(&mut self.t, t);
            self.resident.set(Some(dir.clone()));
            self.memory = Some(memory::Memory::new(disk, disk_t, dir, held, on_lost));
        }
        Ok(self.resident.get().expect("held above"))
    }

    /// Rewrites the store without its free pages (doc "Compacting a
    /// repository"): LMDB's compacting copy, written beside the store,
    /// opened and checked, then renamed over it — the file is at every
    /// instant the old store or the new one, whole. `&mut self`: no
    /// transaction is open, so nothing written meanwhile can be lost.
    ///
    /// Refused while the store is held in memory: the copy there is the one
    /// read, and letting go of it first leaves one store to compact.
    pub fn compact(&mut self) -> Result<Compacted> {
        if self.memory.is_some() {
            return Err(DomainError::Conflict(
                "the repository is held in memory; let go of it first (repos:toggle-memory), \
                 then compact"
                    .into(),
            )
            .into());
        }
        let file = self.dir.join("data.mdb");
        let size = |p: &Path| std::fs::metadata(p).map(|m| m.len());
        let before = size(&file).with_context(|| format!("read {}", file.display()))?;
        let copy = self.dir.join("compact.partial");
        let swap = self.dir.join("compact.swap");
        let cleanup = || {
            let _ = std::fs::remove_dir_all(&copy);
            let _ = std::fs::remove_dir_all(&swap);
        };
        cleanup();

        // The copy, checked as a backup is: a page damaged in the store
        // would otherwise replace it silently.
        let checked = (|| -> Result<()> {
            std::fs::create_dir_all(&copy).with_context(|| format!("create {}", copy.display()))?;
            self.env
                .copy_to_path(copy.join("data.mdb"), heed::CompactionOption::Enabled)
                .context("copy the key-value store")?;
            let problems = KvStore::open_with(&copy, INITIAL_MAP, false)
                .context("open the compacted copy")?
                .check_derived()
                .context("check the compacted copy")?;
            if let Some(first) = problems.first() {
                bail!(
                    "the compacted copy does not check clean — the store may be damaged; it was \
                     left as it is ({} problem(s), first: {first})",
                    problems.len()
                );
            }
            Ok(())
        })();
        if let Err(e) = checked {
            cleanup();
            return Err(e);
        }

        // The store's environment is closed before the file is replaced and
        // opened again; meanwhile an empty one stands in, which nothing reads
        // (`&mut self`).
        let map_size = self.env.info().map_size;
        std::fs::create_dir_all(&swap).with_context(|| format!("create {}", swap.display()))?;
        let stand_in = open_env(&swap, INITIAL_MAP, false)?;
        let stand_in_t = Tables::open(&stand_in)?;
        let old = std::mem::replace(&mut self.env, stand_in);
        self.t = stand_in_t;
        old.prepare_for_closing().wait();
        let renamed = std::fs::rename(copy.join("data.mdb"), &file)
            .with_context(|| format!("move the compacted copy to {}", file.display()))
            .and_then(|()| {
                std::fs::File::open(&self.dir)
                    .and_then(|d| d.sync_all())
                    .with_context(|| format!("sync {}", self.dir.display()))
            });
        // Opened again whether the rename was made or not: the file is one
        // store or the other.
        let reopened = open_env(&self.dir, map_size, self.durable)
            .and_then(|env| Ok((Tables::open(&env)?, env)));
        let (t, env) = match reopened {
            Ok(reopened) => reopened,
            Err(e) => {
                return Err(StoreClosed(format!(
                    "the store could not be opened again after compacting it: {e:#}; load the \
                     repository again"
                ))
                .into())
            }
        };
        self.t = t;
        std::mem::replace(&mut self.env, env).prepare_for_closing().wait();
        cleanup();
        renamed?;
        Ok(Compacted { before, after: size(&file)? })
    }

    /// Lets go of the copy in memory: the store is read on disk again, and
    /// the copy removed. Not held, nothing is done.
    pub fn release_memory(&mut self) {
        if let Some(memory) = self.memory.take() {
            self.env = memory.disk.clone();
            self.t = memory.disk_t;
            self.resident.set(None);
        }
    }

    /// What is held in memory, `None` when the store is not.
    pub fn residency(&self) -> Option<memory::Residency> {
        self.resident.get()
    }

    /// The handle `GET /repos` reads the residency through.
    pub fn residency_handle(&self) -> memory::ResidencyHandle {
        self.resident.clone()
    }
}

/// Grows `env`'s map, as [`KvStore::grow_map`] says, by the free space of
/// the filesystem holding `dir`.
fn grow(env: &Env<WithoutTls>, dir: &Path) -> Result<()> {
    let info = env.info();
    let used = (info.last_page_number + 1) * env.unnamed_database_stat().page_size as usize;
    // Unknown free space: at least as much room as the store holds.
    let free = free_space(dir).unwrap_or(used.max(INITIAL_MAP));
    if let Some(size) = map_to_grant(used, info.map_size, free) {
        // SAFETY: no transaction is open in this process (see
        // `KvStore::grow_map`), and the repository lock keeps every other
        // process out of the file.
        unsafe { env.resize(size) }.context("grow the key-value store's map")?;
    }
    Ok(())
}

impl KvStore {
    /// Brings a store of an older layout to [`LAYOUT`], in one transaction:
    ///
    /// * the rows of the older layouts — `cells` (the first, keyed `uuid · row
    ///   id`) and `field_cells` (keyed by field too) — become one key of
    ///   `records` per metarecord, under its dense id, and their tables, and
    ///   `row_owner` and `by_field`, are emptied. A metarecord with no dense
    ///   id is given one;
    /// * the forest's entries hold the dense id alone, in place of a row id
    ///   and the id;
    /// * the log of an older layout is **dropped, not converted** — the
    ///   history tree (layouts 1 and 2: operations chained to a parent,
    ///   `op_children`, `restorations`), and the sequence before it was split
    ///   by origin (3), the one grouped by writes (4), and the one with no
    ///   index of its reverts (5), the one with no marks on its labels for the
    ///   labels they revert (6 to 8), the one whose operations carry no version delta
    ///   and keep the rows they left (9). The log is emptied, the metarecords kept
    ///   (user decision, October and November 2026; doc "The operation log").
    ///
    /// A new store has nothing to move and only records the layout. Answers
    /// whether the derived key spaces must be derived again: they must when a
    /// metarecord was given an id, which they do not know.
    /// Removes the tables of the older layouts (and `parts`, which the postings
    /// replaced), emptied by the conversion (or
    /// by an earlier open): a named table, even empty, counts against
    /// `max_dbs` and stays in the file's catalog. A store of the current
    /// layout holds no data in them, so this runs at every open, and does
    /// nothing when there are none.
    fn remove_older_tables(&self) -> Result<()> {
        const OLDER: [&str; 28] = [
            "meta",
            "ops_by_label",
            "ops_by_entity",
            "reverted_by",
            "watcher_ops",
            "user_ops",
            "label_kids",
            "label_keys",
            "label_reverts",
            "labels",
            "snaps",
            "ops",
            "grams",
            "hist",
            "events",
            "forest",
            "kids",
            "sets",
            "records",
            "parts",
            "cells",
            "field_cells",
            "row_owner",
            "by_field",
            "op_children",
            "restorations",
            "ops_by_rev",
            "revisions",
        ];
        let r = self.env.read_txn()?;
        let found = self
            .env
            .open_databases_and_commit(r, |dbo, r| -> heed::Result<_> {
                let mut found = Vec::new();
                for name in OLDER {
                    found.push(dbo.open_database::<Bytes, Bytes>(r, Some(name))?);
                }
                let found: [_; 28] = found.try_into().expect("one per name");
                Ok(CommitOrAbort::<_, ()>::Commit(found))
            })?
            .commit()
            .expect("the databases are committed");
        if found.iter().all(Option::is_none) {
            return Ok(());
        }
        let mut w = self.env.write_txn()?;
        for db in found.into_iter().flatten() {
            // SAFETY: no other copy of these handles exists (they were opened
            // above for this only), and this transaction has not written them.
            unsafe { db.remove(&mut w)? };
        }
        w.commit()?;
        Ok(())
    }

    fn migrate_layout(&mut self, from: Option<i64>, old_meta: &[(Vec<u8>, i64)]) -> Result<bool> {
        const BATCH: usize = 10_000;
        // The tables of older layouts, opened (if there) before the write:
        // heed opens a database in a transaction of its own.
        const GONE: [&str; 4] = ["op_children", "restorations", "ops_by_rev", "revisions"];
        const ROWS: [&str; 4] = ["cells", "field_cells", "row_owner", "by_field"];
        // `records` of layouts 8 to 10: the table `metarecords` is now.
        let old_records = open_old_table(&self.env, "records")?;
        let r = self.env.read_txn()?;
        let (rows, gone) = self
            .env
            .open_databases_and_commit(r, |dbo, r| -> heed::Result<_> {
                let mut rows = Vec::new();
                for name in ROWS {
                    rows.push(dbo.open_database::<Bytes, Bytes>(r, Some(name))?);
                }
                let rows: [_; 4] = rows.try_into().expect("one per name");
                let mut gone = Vec::new();
                for name in GONE {
                    gone.push(dbo.open_database::<Bytes, Bytes>(r, Some(name))?);
                }
                let gone: [_; 4] = gone.try_into().expect("one per name");
                Ok(CommitOrAbort::<_, ()>::Commit((rows, gone)))
            })?
            .commit()
            .expect("the databases are committed");
        let txn = KvTxn::new(self)?;
        // The counters `meta` kept, in the state tables (layout 11) — not
        // what the log's, which is emptied below, nor the versions.
        let old = |name: &[u8]| old_meta.iter().find(|(k, _)| k == name).map(|(_, v)| *v);
        for (name, key) in [
            (&b"next_op"[..], State::NextLogOp),
            (b"next_label", State::NextLogContext),
            (b"numbering", State::NextReindex),
            (b"next_id", State::NextMetarecordId),
        ] {
            if let Some(n) = old(name) {
                txn.meta_put(key, n)?;
            }
        }
        // Two clocks merged into one: past both, whatever each numbered.
        if let Some(n) = old(b"mass_op").max(old(b"event_seq")) {
            txn.meta_put(State::NextWriteNumber, n)?;
        }
        if let Some(n) = old(b"derived") {
            txn.meta_put(State::DerivedVersion, n)?;
        }
        {
            let mut w = txn.txn.borrow_mut();
            let t = txn.t;
            // `metarecords` said which metarecords exist (a uuid each, up to
            // layout 10): read, then emptied, the table holding the records
            // from layout 11 (doc "Store tables").
            let uuids: Vec<[u8; 16]> = {
                let mut out = Vec::new();
                for e in t.metarecords.iter(&w)? {
                    out.push(e?.0.try_into().context("a 16-byte uuid in the older metarecords")?);
                }
                out
            };
            t.metarecords.clear(&mut w)?;
            drop(w);
            let existing: std::collections::HashSet<[u8; 16]> = uuids.iter().copied().collect();
            for uuid in &uuids {
                if txn.dense_id(uuid)?.is_none() {
                    let id = u32::try_from(txn.next(State::NextMetarecordId)?)
                        .context("dense ids exhausted")?;
                    let mut w = txn.txn.borrow_mut();
                    t.ids.put(&mut w, uuid, &id.to_be_bytes())?;
                    t.uuids.put(&mut w, &id.to_be_bytes(), uuid)?;
                }
            }
            let mut w = txn.txn.borrow_mut();
            // Layouts 8 to 10 kept the records and the pending writes, their
            // values in the column form: written again in the compact one, a
            // field's nothing beside values dropped (layout 11).
            if let Some(old) = old_records.filter(|_| from.is_some_and(|layout| layout >= 8)) {
                copy_reencoded(old, t.metarecords, &mut w, |v| {
                    let mut record = v10::dec_record(v)?;
                    record.drop_mixed_nothing();
                    Ok(record.encode())
                })?;
                old.clear(w.unjournaled())?;
                reencode(t.mass, &mut w, |k, v| {
                    let op = u64::from_be_bytes(k[4..12].try_into()?);
                    Ok(v10::dec_listed(op, v)?.encode())
                })?;
            }
            // The rows, a metarecord's together (both tables keep them in
            // uuid order).
            for old in rows.iter().take(2).flatten() {
                let mut after: Option<Vec<u8>> = None;
                let mut group: Option<([u8; 16], ConvertedRows)> = None;
                loop {
                    let batch: Vec<(Vec<u8>, Vec<u8>)> = {
                        let lo = match &after {
                            Some(k) => std::ops::Bound::Excluded(k.as_slice()),
                            None => std::ops::Bound::Unbounded,
                        };
                        let range = (lo, std::ops::Bound::Unbounded);
                        old.range(&w, &range)?
                            .take(BATCH)
                            .map(|e| e.map(|(k, v)| (k.to_vec(), v.to_vec())))
                            .collect::<heed::Result<_>>()?
                    };
                    let Some((last, _)) = batch.last() else { break };
                    after = Some(last.clone());
                    for (k, v) in &batch {
                        let uuid: [u8; 16] = k[..16].try_into().expect("a 16-byte uuid");
                        let id = i64::from_be_bytes(v[..8].try_into().expect("a row id"));
                        let row = v10::dec_row(v)?;
                        match &mut group {
                            Some((u, rows)) if *u == uuid => rows.push((id, row)),
                            _ => {
                                if let Some((u, rows)) = group.take() {
                                    file_converted(&t, &mut w, &existing, &u, rows)?;
                                }
                                group = Some((uuid, vec![(id, row)]));
                            }
                        }
                    }
                }
                if let Some((u, rows)) = group.take() {
                    file_converted(&t, &mut w, &existing, &u, rows)?;
                }
            }
            for old in rows.iter().flatten() {
                old.clear(w.unjournaled())?;
            }
            // The forest is `index_raw_holder` from layout 11, an index: the
            // older table goes, and the reindex below files the positions.
            // The live metarecords, primary from layout 11: those that existed,
            // less the ids a deletion of many records still pending took out
            // (their key stayed until its fold); each has a record.
            let mut live = roaring::RoaringBitmap::new();
            for uuid in &existing {
                if let Some(id) = derived::id_of(&t, &w, uuid)? {
                    live.insert(id);
                }
            }
            let pending_deletes: Vec<(Vec<u8>, Vec<u8>)> = t
                .mass
                .prefix_iter(&w, &mass::DELETES_PART.to_be_bytes())?
                .map(|e| e.map(|(k, v)| (k.to_vec(), v.to_vec())))
                .collect::<heed::Result<_>>()?;
            for (k, v) in pending_deletes {
                let op = u64::from_be_bytes(k[4..12].try_into()?);
                live -= &mass::Listed::decode(op, &v)?.ids;
            }
            for id in &live {
                let k = id.to_be_bytes();
                if t.metarecords.get(&w, &k)?.is_none() {
                    t.metarecords.put(&mut w, &k, &mass::Record::plain(Vec::new()).encode())?;
                }
            }
            t.live_metarecords.clear(&mut w)?;
            if let (Some(lo), Some(hi)) = (live.min(), live.max()) {
                for chunk in (lo >> 16)..=(hi >> 16) {
                    let mut part = roaring::RoaringBitmap::new();
                    part.insert_range((chunk << 16)..=((chunk << 16) | 0xFFFF));
                    part &= &live;
                    if part.is_empty() {
                        continue;
                    }
                    part.optimize();
                    let mut bytes = Vec::with_capacity(part.serialized_size());
                    part.serialize_into(&mut bytes)?;
                    t.live_metarecords.put(&mut w, &(chunk as u16).to_be_bytes(), &bytes)?;
                }
            }
            // The log of a store of an older layout is dropped (layout 11
            // writes it in one form of entry, which an older log has not: the
            // log is emptied rather than converted, user decision, November
            // 2026).
            {
                for old in gone.into_iter().flatten() {
                    old.clear(w.unjournaled())?;
                }
                for db in [
                    t.log_entries,
                    t.log_snaps,
                    t.log_entries_by_context,
                    t.log_entries_by_metarecord,
                    t.log_contexts,
                    t.log_context_children,
                    t.log_label_keys,
                    t.log_reverted_by,
                    t.log_watcher_entries,
                    t.log_user_entries,
                    t.log_context_reverts,
                ] {
                    db.clear(&mut w)?;
                }
                State::LogHead.delete(&t, &mut w)?;
                State::WatcherLogContext.delete(&t, &mut w)?;
            }
        }
        txn.meta_put(State::PrimaryVersion, LAYOUT)?;
        txn.finish()?;
        // The live metarecords and the records are written anew: the indexes
        // are derived again.
        Ok(true)
    }
}

/// How many values — rows that are not `Nothing` — of `field` `rows` hold.
fn valued_in(rows: &[FieldRow], field: &str) -> u64 {
    rows.iter().filter(|r| r.name == field && !matches!(r.value, Value::Nothing)).count() as u64
}

/// A metarecord's rows as an older layout kept them, each with its row id.
type ConvertedRows = Vec<(i64, FieldRow)>;

/// Files one metarecord's converted rows, oldest first, under its dense id.
/// Rows of a metarecord that is not there are dropped.
fn file_converted(
    t: &Tables,
    w: &mut table::Wtxn<'_>,
    existing: &std::collections::HashSet<[u8; 16]>,
    uuid: &[u8; 16],
    mut rows: ConvertedRows,
) -> Result<()> {
    if !existing.contains(uuid) {
        return Ok(());
    }
    let id = derived::id_of(t, w, uuid)?.context("a metarecord with no dense id")?;
    rows.sort_by_key(|(row_id, _)| *row_id);
    let rows: Vec<FieldRow> = rows.into_iter().map(|(_, row)| row).collect();
    let mut record = mass::Record::plain(rows);
    record.drop_mixed_nothing();
    t.metarecords.put(w, &id.to_be_bytes(), &record.encode())?;
    Ok(())
}

/// A table of an older layout, if the store has one.
fn open_old_table(
    env: &Env<WithoutTls>,
    name: &str,
) -> Result<Option<heed::Database<'static, Bytes, Bytes>>> {
    let r = env.read_txn()?;
    Ok(env
        .open_databases_and_commit(r, |dbo, r| -> heed::Result<_> {
            Ok(CommitOrAbort::<_, ()>::Commit(dbo.open_database::<Bytes, Bytes>(r, Some(name))?))
        })?
        .commit()
        .expect("the database is committed"))
}

/// Copies every entry of `from`, an older table, into `to`, its value
/// converted by `convert`, a batch at a time.
fn copy_reencoded(
    from: heed::Database<'static, Bytes, Bytes>,
    to: Db,
    w: &mut table::Wtxn<'_>,
    convert: impl Fn(&[u8]) -> Result<Vec<u8>>,
) -> Result<()> {
    const BATCH: usize = 10_000;
    let mut after: Option<Vec<u8>> = None;
    loop {
        let batch: Vec<(Vec<u8>, Vec<u8>)> = {
            let lo = match &after {
                Some(k) => std::ops::Bound::Excluded(k.as_slice()),
                None => std::ops::Bound::Unbounded,
            };
            from.range(w, &(lo, std::ops::Bound::Unbounded))?
                .take(BATCH)
                .map(|e| e.map(|(k, v)| (k.to_vec(), v.to_vec())))
                .collect::<heed::Result<_>>()?
        };
        let Some((last, _)) = batch.last() else { return Ok(()) };
        after = Some(last.clone());
        for (k, v) in &batch {
            to.put(w, k, &convert(v)?)?;
        }
    }
}

/// Writes again, in the compact encoding (doc "Value encoding in the
/// store"), the values of `table` that layout 10 and before wrote — a
/// record's, or a pending write's — read by `convert`, a batch at a time.
fn reencode(
    table: Db,
    w: &mut table::Wtxn<'_>,
    convert: impl Fn(&[u8], &[u8]) -> Result<Vec<u8>>,
) -> Result<()> {
    const BATCH: usize = 10_000;
    let mut after: Option<Vec<u8>> = None;
    loop {
        let batch: Vec<(Vec<u8>, Vec<u8>)> = {
            let lo = match &after {
                Some(k) => std::ops::Bound::Excluded(k.as_slice()),
                None => std::ops::Bound::Unbounded,
            };
            table
                .range(w, &(lo, std::ops::Bound::Unbounded))?
                .take(BATCH)
                .map(|e| e.map(|(k, v)| (k.to_vec(), v.to_vec())))
                .collect::<heed::Result<_>>()?
        };
        let Some((last, _)) = batch.last() else { return Ok(()) };
        after = Some(last.clone());
        for (k, v) in &batch {
            let new = convert(k, v)?;
            table.put(w, k, &new)?;
        }
    }
}

impl<'e> KvTxn<'e> {
    fn new(store: &'e mut KvStore) -> Result<KvTxn<'e>> {
        if let Some(lost) = store.memory.as_ref().and_then(|m| m.lost()) {
            bail!("{lost}");
        }
        store.grow_map()?;
        let store: &'e KvStore = store;
        let memory = store.memory.as_ref();
        Ok(KvTxn {
            t: store.t,
            txn: RefCell::new(Wtxn::new(store.env.write_txn()?, memory.is_some())),
            memory,
            sets: RefCell::new(HashMap::new()),
            cached_chunks: RefCell::new(HashMap::new()),
            chunks_existing: RefCell::new(Default::default()),
            reads: &store.reads,
            mass_lists: TxnFlag::of(&store.mass),
            deletes: TxnFlag::of(&store.deletes),
            thawed: TxnFlag::of(&store.thawed),
            written: &store.written,
            generation: &store.generation,
            universe: RefCell::new(HashMap::new()),
            grams: RefCell::new(HashMap::new()),
            grams_changed: RefCell::new(Default::default()),
            events: RefCell::new(HashMap::new()),
        })
    }

    fn meta_put(&self, key: State, n: i64) -> Result<()> {
        key.put(&self.t, &mut self.txn.borrow_mut(), n)
    }

    /// Writes what the transaction still holds in memory, and commits.
    fn finish(self) -> Result<()> {
        metafolder_core::slowlog::timed("commit.derived", || self.flush_sets())?;
        let wtxn = self.txn.into_inner();
        let written = wtxn.written();
        let (txn, changes) = wtxn.into_parts();
        // The store's own commit: writing the dirty pages, the free-page list,
        // and the fsync.
        metafolder_core::slowlog::timed("commit.store", || -> Result<()> {
            match (self.memory, changes) {
                (Some(memory), Some(changes)) => memory.commit(txn, &changes)?,
                _ => txn.commit().context("Failed to commit write transaction")?,
            }
            Ok(())
        })?;
        // Every write commits here: an operation that never did is a read,
        // held to its own threshold in the slow-operation log.
        metafolder_core::slowlog::mark_write();
        *self.written.lock().unwrap_or_else(|e| e.into_inner()) = written;
        for flag in [&self.mass_lists, &self.deletes, &self.thawed] {
            flag.publish();
        }
        self.generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
}

impl KvTxn<'_> {
    /// The dense id of a metarecord that exists: a deleted one keeps its
    /// record, which no write may change.
    fn live_id(&self, uuid: Uuid) -> Result<Option<u32>> {
        let Some(id) = self.dense_id(uuid.as_bytes())? else { return Ok(None) };
        let live = self.live_in(&self.txn.borrow(), id)?;
        Ok(live.then_some(id))
    }

    fn next(&self, counter: State) -> Result<i64> {
        let mut w = self.txn.borrow_mut();
        let n = counter.get(&self.t, &w)?.unwrap_or(1);
        counter.put(&self.t, &mut w, n + 1)?;
        Ok(n)
    }

    fn count_type(&self, name: &str, value: &Value, delta: i64) -> Result<()> {
        if matches!(value, Value::Nothing) {
            return Ok(());
        }
        self.count_type_named(name, &[codec::type_code(value)], delta)
    }

    /// [`Self::count_type`] for a type code (one byte: `field_types` keys a
    /// field's types by their codes, doc "Store tables").
    fn count_type_named(&self, name: &str, code: &[u8], delta: i64) -> Result<()> {
        let k = key(&[&name_key(name), code]);
        let mut w = self.txn.borrow_mut();
        let n = self.t.field_types.get(&w, &k)?.map_or(0, from_be) + delta;
        if n <= 0 {
            self.t.field_types.delete(&mut w, &k)?;
        } else {
            self.t.field_types.put(&mut w, &k, &be(n))?;
        }
        Ok(())
    }

    /// Writes a record's rows under its key — none included: every
    /// metarecord has a record (doc "Store tables").
    fn put_record(&self, id: u32, rows: &[FieldRow]) -> Result<()> {
        let mut w = self.txn.borrow_mut();
        self.t.metarecords.put(&mut w, &id.to_be_bytes(), &enc_rows(rows))?;
        Ok(())
    }

    /// What a row's going away changes outside its record: its forest
    /// position, the count of its type, the derived data. `before` holds the
    /// rows the record held before the write that removes this one — one or
    /// many — which the count of values of the row's field is derived from.
    fn row_gone(&self, owner: Uuid, row: &FieldRow, before: &[FieldRow]) -> Result<()> {
        if let Some(k) = self.raw_key_now(&row.name, &row.value)? {
            self.t.index_raw_holder.delete(&mut self.txn.borrow_mut(), &k)?;
        }
        self.count_type(&row.name, &row.value, -1)?;
        self.derive_row(owner, &row.name, &row.value, -1, valued_in(before, &row.name))
    }

    /// Writes a new label, and its place among its parent's children.
    fn put_label(&self, meta: &LabelMeta, command: Option<Uuid>) -> Result<i64> {
        let id = self.next(State::NextLogContext)?;
        let mut w = self.txn.borrow_mut();
        self.t.log_contexts.put(&mut w, &be(id), &enc_label(meta, command))?;
        if let Some(parent) = meta.parent {
            self.t.log_context_children.put(&mut w, &key(&[&be(parent), &be(id)]), &[])?;
        }
        Ok(id)
    }

    /// Drops `label` if nothing is left in it — no operation, no child label —
    /// and then its parent on the same terms. The watcher's label stays: the
    /// daemon writes under it.
    fn drop_if_empty(&self, label: i64) -> Result<usize> {
        let mut dropped = 0;
        let mut next = Some(label);
        while let Some(label) = next {
            let (empty, parent) = {
                let txn = self.txn.borrow();
                let read = Read {
                    t: &self.t,
                    r: &txn,
                    reads: Some(self.reads),
                    live: None,
                    holders: None,
                    values: None,
                    mass: false,
                    universe: Default::default(),
                    stubs: None,
                };
                let Some(meta) = self.t.log_contexts.get(&txn, &be(label))? else { break };
                let meta = dec_label(meta)?;
                let empty = read.label_first_op(label)?.is_none()
                    && read.label_children(label)?.is_empty()
                    && read.meta(State::WatcherLogContext)? != Some(label);
                (empty, meta.parent)
            };
            if !empty {
                break;
            }
            self.drop_label(label)?;
            dropped += 1;
            next = parent;
        }
        Ok(dropped)
    }
    fn remove_op(&self, id: i64) -> Result<()> {
        let Some(op) = Log::op(self, id)? else { return Ok(()) };
        let mut w = self.txn.borrow_mut();
        let t = self.t;
        t.log_entries.delete(&mut w, &be(id))?;
        let prefix = be(id);
        let snaps: Vec<Vec<u8>> = t
            .log_snaps
            .prefix_iter(&w, &prefix)?
            .map(|e| e.map(|(k, _)| k.to_vec()))
            .collect::<std::result::Result<_, _>>()?;
        for k in snaps {
            t.log_snaps.delete(&mut w, &k)?;
        }
        t.log_entries_by_context.delete(&mut w, &key(&[&be(op.label_id), &be(id)]))?;
        if let Some(target) = op.reverts_op_id {
            t.log_reverted_by.delete(&mut w, &key(&[&be(target), &be(id)]))?;
        }
        t.log_entries_by_metarecord.delete(&mut w, &key(&[op.entity_uuid.as_bytes(), &be(id)]))?;
        t.log_watcher_entries.delete(&mut w, &be(id))?;
        t.log_user_entries.delete(&mut w, &be(id))?;
        Ok(())
    }

    /// The table of the operations of `origin`'s retention class.
    fn class(&self, origin: Option<&str>) -> Db {
        if origin == Some(crate::log::ORIGIN_WATCHER) {
            self.t.log_watcher_entries
        } else {
            self.t.log_user_entries
        }
    }
}

impl WriteTxn for KvTxn<'_> {
    fn restore_metarecord(&self, uuid: Uuid) -> Result<Option<Vec<FieldRow>>> {
        self.restore_deleted(uuid)
    }

    fn was_deleted(&self, uuid: Uuid) -> Result<bool> {
        let Some(id) = self.dense_id(uuid.as_bytes())? else { return Ok(false) };
        Ok(!self.live_in(&self.txn.borrow(), id)? && !self.reserved(id)?)
    }

    fn mass_apply(
        &self,
        kind: MassKind,
        field: &str,
        values: &[Value],
        ids: &roaring::RoaringBitmap,
        label: i64,
        timestamp: i64,
        origin: Option<&str>,
    ) -> Result<Option<crate::store::MassApplied>> {
        self.mass_log(kind, field, values, ids, label, timestamp, origin, None)
    }

    fn mass_delete(
        &self,
        ids: &roaring::RoaringBitmap,
        label: i64,
        timestamp: i64,
        origin: Option<&str>,
    ) -> Result<Option<crate::store::MassApplied>> {
        self.mass_delete_log(ids, label, timestamp, origin)
    }

    fn mass_revert(
        &self,
        first_op: i64,
        last_op: i64,
        label: i64,
        timestamp: i64,
        origin: Option<&str>,
    ) -> Result<Option<crate::store::MassApplied>> {
        self.mass_revert_log(first_op, last_op, label, timestamp, origin)
    }

    fn mass_undelete(
        &self,
        first_op: i64,
        last_op: i64,
    ) -> Result<Option<crate::store::MassApplied>> {
        self.mass_undelete_log(first_op, last_op)
    }

    fn rows_over_ids(&self, field: &str, n: u64) -> Result<Option<roaring::RoaringBitmap>> {
        self.rows_over_now(field, n).map(Some)
    }

    fn dense_ids(
        &self,
        uuids: &[Uuid],
        must_exist: bool,
    ) -> Result<Option<roaring::RoaringBitmap>> {
        self.dense_ids_of(uuids, must_exist).map(Some)
    }

    fn uuids_of(&self, ids: &roaring::RoaringBitmap) -> Result<Vec<Uuid>> {
        self.uuids_of_ids(ids)
    }

    fn create_metarecord(&self, uuid: Uuid) -> Result<()> {
        if Rows::exists(self, uuid)? {
            bail!("metarecord {uuid} already exists");
        }
        // A dense id, live, and an empty record: the version is computed from
        // the rows when read (doc "Metarecord version").
        self.derive_created(uuid)
    }

    /// Removes a metarecord: its existence and its place in the universe —
    /// and nothing of what the store holds about it, neither the indexes nor
    /// its record, which every read sifts through the universe (doc "Deleting
    /// a metarecord in the store"). On a hard disk each index entry is a seek,
    /// and a file has dozens. What it leaves is what [`KvTxn::restore_deleted`]
    /// relies on: the id comes back with its entries, and its record says what
    /// they describe. Only a reindex frees the record.
    ///
    /// A folder that still has children is removed the same way: a write that
    /// leaves it so is refused at its commit (`check_forest_integrity`), and a
    /// write that deletes the children too leaves nothing to unpick.
    fn remove_metarecord(&self, uuid: Uuid) -> Result<()> {
        let id = self.dense_id(uuid.as_bytes())?;
        let mut rows = Vec::new();
        if let Some(id) = id {
            self.freeze_given_back(&roaring::RoaringBitmap::from_iter([id]))?;
            rows = self.load_record(id)?.plain_rows();
        }
        for row in &rows {
            self.count_type(&row.name, &row.value, -1)?;
            self.derive_dropped(uuid, &row.name, &row.value)?;
        }
        self.derive_removed(uuid)
    }

    fn insert_row(&self, uuid: Uuid, name: &str, value: &Value) -> Result<()> {
        if !Rows::exists(self, uuid)? {
            bail!("a row for metarecord {uuid}, which does not exist");
        }
        let id =
            self.dense_id(uuid.as_bytes())?.with_context(|| format!("no dense id for {uuid}"))?;
        let loaded = self.load_record(id)?;
        let before = loaded.plain_rows();
        // The two uniqueness rules of the data model, with the messages
        // clients already know.
        if name == "mfr_path" && before.iter().any(|r| r.name == name) {
            return Err(DomainError::BadRequest(
                "mfr_path is single-valued: a metarecord tracks at most one path".into(),
            )
            .into());
        }
        if let Some(k) = self.raw_key_now(name, value)? {
            // Occupied by a metarecord that exists: what a deleted one left
            // there is written over.
            let txn = self.txn.borrow();
            let occupied = match self.t.index_raw_holder.get(&txn, &k)? {
                Some(v) => self.live_in(&txn, derived::dense(v))?,
                None => false,
            };
            if occupied {
                return Err(DomainError::BadRequest(format!(
                    "tree position already occupied for field '{name}'"
                ))
                .into());
            }
        }
        let op = self.write_number(&loaded)?;
        let mut rows = loaded.rows.clone();
        rows.push((FieldRow { name: name.to_string(), value: value.clone() }, op));
        self.save_record(&loaded, rows, Vec::new(), Vec::new())?;
        self.file_position(name, value, id)?;
        self.count_type(name, value, 1)?;
        self.derive_row(uuid, name, value, 1, valued_in(&before, name))
    }

    fn delete_row(&self, owner: Uuid, name: &str, value: &Value) -> Result<()> {
        let Some(id) = self.live_id(owner)? else { return Ok(()) };
        let loaded = self.load_record(id)?;
        let Some(at) = loaded.rows.iter().position(|(r, _)| r.name == name && &r.value == value)
        else {
            return Ok(());
        };
        let op = self.write_number(&loaded)?;
        let mut rows = loaded.rows.clone();
        let (row, _) = rows.remove(at);
        let tombs = if loaded.entries.is_empty() {
            Vec::new()
        } else {
            vec![(name.to_string(), value.clone(), op)]
        };
        self.save_record(&loaded, rows, tombs, Vec::new())?;
        self.row_gone(owner, &row, &loaded.plain_rows())
    }

    fn delete_rows(&self, uuid: Uuid, name: Option<&str>) -> Result<()> {
        let Some(id) = self.live_id(uuid)? else { return Ok(()) };
        let loaded = self.load_record(id)?;
        let wanted = |row: &FieldRow| name.is_none_or(|n| n == row.name);
        let gone: Vec<FieldRow> =
            loaded.rows.iter().filter(|(r, _)| wanted(r)).map(|(r, _)| r.clone()).collect();
        if gone.is_empty() {
            return Ok(());
        }
        let op = self.write_number(&loaded)?;
        let kept: Vec<(FieldRow, u64)> =
            loaded.rows.iter().filter(|(r, _)| !wanted(r)).cloned().collect();
        let mut cleared: Vec<(String, u64)> = Vec::new();
        if !loaded.entries.is_empty() {
            for row in &gone {
                if !cleared.iter().any(|(n, _)| *n == row.name) {
                    cleared.push((row.name.clone(), op));
                }
            }
        }
        self.save_record(&loaded, kept, Vec::new(), cleared)?;
        let before = loaded.plain_rows();
        for row in &gone {
            self.row_gone(uuid, row, &before)?;
        }
        Ok(())
    }

    fn create_label(&self, meta: &LabelMeta) -> Result<i64> {
        self.put_label(meta, None)
    }

    fn create_command_label(&self, meta: &LabelMeta, key: Uuid) -> Result<i64> {
        let id = self.put_label(meta, Some(key))?;
        self.t.log_label_keys.put(&mut self.txn.borrow_mut(), key.as_bytes(), &be(id))?;
        Ok(id)
    }

    fn drop_label(&self, id: i64) -> Result<()> {
        let Some(meta) = Log::labels(self, &[id])?.remove(&id) else { return Ok(()) };
        let command = {
            let txn = self.txn.borrow();
            match self.t.log_contexts.get(&txn, &be(id))? {
                Some(b) => dec_label_key(b)?,
                None => None,
            }
        };
        let mut w = self.txn.borrow_mut();
        self.t.log_contexts.delete(&mut w, &be(id))?;
        if let Some(parent) = meta.parent {
            self.t.log_context_children.delete(&mut w, &key(&[&be(parent), &be(id)]))?;
        }
        if let Some(command) = command {
            self.t.log_label_keys.delete(&mut w, command.as_bytes())?;
        }
        // What it reverts, and what reverts it, each side of the pair.
        for kind in [0u8, 1] {
            let prefix = key(&[&[kind], &be(id)]);
            let pairs: Vec<Vec<u8>> = self
                .t
                .log_context_reverts
                .prefix_iter(&w, &prefix)?
                .map(|e| e.map(|(k, _)| k.to_vec()))
                .collect::<std::result::Result<_, _>>()?;
            for k in pairs {
                let other = &k[9..];
                self.t.log_context_reverts.delete(&mut w, &k)?;
                self.t.log_context_reverts.delete(&mut w, &key(&[&[1 - kind], other, &be(id)]))?;
            }
        }
        Ok(())
    }

    fn note_label_reverts(&self, reverter: i64, reverted: i64, tree: bool) -> Result<()> {
        let mut w = self.txn.borrow_mut();
        let forward = key(&[&[0u8], &be(reverter), &be(reverted)]);
        let already =
            self.t.log_context_reverts.get(&w, &forward)?.is_some_and(|v| v.first() == Some(&1));
        let flag = [u8::from(tree || already)];
        self.t.log_context_reverts.put(&mut w, &forward, &flag)?;
        self.t.log_context_reverts.put(
            &mut w,
            &key(&[&[1u8], &be(reverted), &be(reverter)]),
            &flag,
        )?;
        Ok(())
    }

    fn set_watcher_label(&self, id: i64) -> Result<()> {
        self.meta_put(State::WatcherLogContext, id)
    }

    fn append_ops(&self, label: i64, timestamp: i64, ops: &[NewOp]) -> Result<i64> {
        let base = {
            let w = self.txn.borrow();
            State::NextLogOp.get(&self.t, &w)?.unwrap_or(1)
        };
        let mut w = self.txn.borrow_mut();
        let t = self.t;
        for (i, op) in ops.iter().enumerate() {
            let id = base + i as i64;
            t.log_entries.put(&mut w, &be(id), &enc_op(op, label, timestamp))?;
            // What the write took off, and — of an append alone — the value
            // it added, which a revert takes off in its turn.
            let added: &[FieldRow] =
                if op.op_type == crate::log::OpType::AppendField { &op.after } else { &[] };
            for (after, rows) in [(0u8, &op.before[..]), (1u8, added)] {
                for (n, row) in rows.iter().enumerate() {
                    let k = key(&[&be(id), &[after], &(n as u32).to_be_bytes()]);
                    t.log_snaps.put(&mut w, &k, &enc_row(row))?;
                }
            }
            t.log_entries_by_context.put(&mut w, &key(&[&be(label), &be(id)]), &[])?;
            if let Some(target) = op.reverts_op_id {
                t.log_reverted_by.put(&mut w, &key(&[&be(target), &be(id)]), &[])?;
            }
            t.log_entries_by_metarecord.put(&mut w, &key(&[op.entity.as_bytes(), &be(id)]), &[])?;
            self.class(op.origin.as_deref()).put(&mut w, &be(id), &[])?;
        }
        let last = base + ops.len() as i64 - 1;
        State::NextLogOp.put(&t, &mut w, last + 1)?;
        Ok(last)
    }

    fn set_head(&self, op: Option<i64>) -> Result<()> {
        let mut w = self.txn.borrow_mut();
        match op {
            Some(op) => State::LogHead.put(&self.t, &mut w, op)?,
            None => {
                State::LogHead.delete(&self.t, &mut w)?;
            }
        }
        Ok(())
    }

    /// The log trim (`log::Retention`), over these tables: for each origin
    /// past its limit and slack, the oldest operations down to the limit — the
    /// first keys of its table — and the labels left empty. Reads what it deletes and two statistics,
    /// never the operations it keeps (`tests/log_cost.rs`): it runs once every
    /// `slack` writes, and what is kept may be a reconcile's.
    fn trim(&self, retention: Retention, keep_from: i64) -> Result<usize> {
        if !retention.enabled() {
            return Ok(0);
        }
        let count = |n: usize| {
            self.reads.fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
            metafolder_core::slowlog::count_reads(n as u64);
        };
        let mut doomed: Vec<i64> = Vec::new();
        for (table, limit) in [
            (self.t.log_watcher_entries, retention.watcher_ops),
            (self.t.log_user_entries, retention.user_ops),
        ] {
            if limit == 0 {
                continue;
            }
            let txn = self.txn.borrow();
            let total = table.len(&txn)?;
            count(1);
            if total <= limit + Retention::slack(limit) {
                continue;
            }
            let cut = (total - limit) as usize;
            let mut read = 0;
            for e in table.iter(&txn)?.take(cut) {
                read += 1;
                let id = from_be(e?.0);
                // The write committing is kept whole: trimmed by its own
                // commit, a write larger than the limit lost its oldest
                // operations, and an undo of it undid only part.
                if id >= keep_from {
                    break;
                }
                // A write to many records goes whole.
                doomed.extend(id..=self.entry_end(id)?);
            }
            count(read);
        }
        if doomed.is_empty() {
            return Ok(0);
        }
        self.delete_ops(&doomed)?;
        Ok(doomed.len())
    }

    fn clear_metarecords(&self) -> Result<()> {
        let mut w = self.txn.borrow_mut();
        let t = self.t;
        for db in [t.metarecords, t.live_metarecords, t.ids, t.field_types, t.index_raw_holder] {
            db.clear(&mut w)?;
        }
        drop(w);
        self.clear_derived()
    }

    fn delete_ops(&self, ids: &[i64]) -> Result<usize> {
        let (stubs, mut hit) = self.delete_stub_ops(ids)?;
        for &id in ids {
            if stubs.contains(&id) {
                continue;
            }
            if let Some(op) = Log::op(self, id)? {
                hit.insert(op.label_id);
                self.remove_op(id)?;
            }
        }
        let mut dropped = 0;
        for label in hit {
            dropped += self.drop_if_empty(label)?;
        }
        Ok(dropped)
    }

    fn commit(self: Box<Self>) -> Result<()> {
        (*self).finish()
    }
}

#[cfg(test)]
mod map_tests {
    use super::{map_to_grant, page_multiple, ROOM_CAP};

    const G: usize = 1 << 30;

    #[test]
    fn test_a_transaction_starts_with_the_disks_free_space_as_room() {
        // 1 GiB map, 10 MiB used, 50 GiB free: room becomes the 50 GiB.
        let map = map_to_grant(10 << 20, G, 50 * G).expect("grown");
        assert_eq!(map, page_multiple((10 << 20) + 50 * G));
    }

    #[test]
    fn test_room_that_shrinks_with_the_free_space_is_not_regranted() {
        // The store grew by 5 GiB into a map granted when 50 GiB were free:
        // 45 GiB of room, 45 GiB free — nothing to do, no resize per write.
        let map = page_multiple(50 * G);
        assert_eq!(map_to_grant(5 * G, map, 45 * G), None);
        // Space freed elsewhere on the disk: the room follows it.
        assert!(map_to_grant(5 * G, map, 60 * G).is_some());
    }

    #[test]
    fn test_room_is_capped_and_renewed_at_half_the_cap() {
        let map = map_to_grant(0, G, 10 * ROOM_CAP).unwrap();
        assert_eq!(map, page_multiple(ROOM_CAP));
        // Still over half the cap of room: kept.
        assert_eq!(map_to_grant(ROOM_CAP / 4, map, 10 * ROOM_CAP), None);
        // Under half: renewed to a full cap beyond what is used.
        let used = ROOM_CAP / 2 + G;
        assert_eq!(map_to_grant(used, map, 10 * ROOM_CAP), Some(page_multiple(used + ROOM_CAP)));
    }

    #[test]
    fn test_the_map_never_shrinks() {
        assert_eq!(map_to_grant(G, 100 * G, 0), None);
    }
}

#[cfg(test)]
mod memory_tests {
    use super::{memory, test_store, Env, Tables, TestDir, WithoutTls};
    use crate::store::{Begin, Rows};
    use metafolder_core::metarecord::Value;
    use std::sync::{Arc, Mutex};
    use uuid::Uuid;

    /// A directory standing for the RAM the copies go to.
    fn base() -> TestDir {
        TestDir(
            std::env::temp_dir()
                .join("metafolder-tests")
                .join(format!("unit_memory_{}", Uuid::new_v4())),
        )
    }

    /// What `on_lost` was told, and the callback telling it.
    fn told() -> (Arc<Mutex<Vec<String>>>, memory::OnLost) {
        let told = Arc::new(Mutex::new(Vec::new()));
        let sink = told.clone();
        (told, Box::new(move |m| sink.lock().unwrap().push(m)))
    }

    fn fill(store: &mut super::KvStore, rows: usize) -> anyhow::Result<()> {
        let w = store.begin_write()?;
        for i in 0..rows {
            let uuid = Uuid::new_v4();
            w.create_metarecord(uuid)?;
            w.insert_row(uuid, "text", &Value::String(format!("{i:0>200}")))?;
        }
        w.commit()
    }

    /// Every key and value of every table.
    fn dump(env: &Env<WithoutTls>, t: &Tables) -> Vec<(usize, Vec<u8>, Vec<u8>)> {
        let r = env.read_txn().unwrap();
        let mut out = Vec::new();
        for (i, db) in t.all().iter().enumerate() {
            for e in db.iter(&r).unwrap() {
                let (k, v) = e.unwrap();
                out.push((i, k.to_vec(), v.to_vec()));
            }
        }
        out
    }

    /// The copy and the store on disk hold the same keys and values.
    fn assert_in_step(store: &super::KvStore) {
        let disk = store.memory.as_ref().expect("held in memory");
        let (copy, on_disk) = (dump(&store.env, &store.t), dump(&disk.disk, &disk.disk_t));
        assert_eq!(copy.len(), on_disk.len());
        assert!(copy == on_disk, "the copy and the disk differ");
    }

    #[test]
    fn test_a_store_is_copied_into_memory_only_when_asked() {
        let (mut store, _dir) = test_store();
        fill(&mut store, 10).unwrap();
        assert_eq!(store.residency(), None);
        let base = base();
        let calls = std::cell::Cell::new(0);
        let (_, on_lost) = told();
        let held =
            store.hold_in_memory(&base.0, on_lost, &|_, _| calls.set(calls.get() + 1)).unwrap();
        assert!(calls.get() >= 1, "the copy reports its progress");
        let copy = std::fs::read_dir(&base.0).unwrap().next().unwrap().unwrap().path();
        assert_eq!(held.bytes, std::fs::metadata(copy.join("data.mdb")).unwrap().len());
        assert_eq!(store.residency(), Some(held));
        // Read from the copy, the store answers as before.
        assert_eq!(store.metarecord_count().unwrap(), 10);
        assert_in_step(&store);
    }

    #[test]
    fn test_a_write_to_a_store_held_in_memory_is_on_disk_too() {
        let (mut store, dir) = test_store();
        let base = base();
        let (told, on_lost) = told();
        store.hold_in_memory(&base.0, on_lost, &|_, _| {}).unwrap();
        fill(&mut store, 300).unwrap();
        fill(&mut store, 5).unwrap();
        assert_in_step(&store);
        // A derived index rewritten whole is replayed whole, its tables
        // cleared first.
        store.reindex().unwrap();
        assert_in_step(&store);
        assert!(told.lock().unwrap().is_empty());
        drop(store);
        // The copy goes with the store; the disk keeps every write.
        assert_eq!(std::fs::read_dir(&base.0).unwrap().count(), 0);
        let store = super::KvStore::open_unsynced(&dir.0).unwrap();
        assert_eq!(store.metarecord_count().unwrap(), 305);
    }

    #[test]
    fn test_a_write_the_disk_refuses_leaves_no_trace_in_memory() {
        let (mut store, _dir) = test_store();
        let base = base();
        let (told, on_lost) = told();
        store.hold_in_memory(&base.0, on_lost, &|_, _| {}).unwrap();
        fill(&mut store, 3).unwrap();
        store.memory.as_ref().unwrap().fail.store(1, std::sync::atomic::Ordering::SeqCst);
        let refused = fill(&mut store, 4).unwrap_err();
        assert!(format!("{refused:#}").contains("disk"), "{refused:#}");
        assert_eq!(store.metarecord_count().unwrap(), 3, "the copy took a write the disk refused");
        assert_in_step(&store);
        // Nothing is lost: the next write is taken by both.
        assert!(told.lock().unwrap().is_empty());
        fill(&mut store, 4).unwrap();
        assert_eq!(store.metarecord_count().unwrap(), 7);
        assert_in_step(&store);
    }

    #[test]
    fn test_a_copy_that_cannot_take_what_the_disk_took_is_lost() {
        let (mut store, dir) = test_store();
        let base = base();
        let (told, on_lost) = told();
        store.hold_in_memory(&base.0, on_lost, &|_, _| {}).unwrap();
        store.memory.as_ref().unwrap().fail.store(2, std::sync::atomic::Ordering::SeqCst);
        let failed = fill(&mut store, 4).unwrap_err();
        let told = told.lock().unwrap().clone();
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(format!("{failed:#}").contains(&told[0]), "{failed:#}");
        // Behind the disk for good: no write is taken any more.
        let refused = fill(&mut store, 1).map(|_| ()).unwrap_err();
        assert!(format!("{refused:#}").contains(&told[0]), "{refused:#}");
        drop(store);
        let store = super::KvStore::open_unsynced(&dir.0).unwrap();
        assert_eq!(store.metarecord_count().unwrap(), 4, "the disk keeps what it took");
    }

    #[test]
    fn test_a_store_held_in_memory_can_be_let_go_and_held_again() {
        let (mut store, _dir) = test_store();
        let base = base();
        store.hold_in_memory(&base.0, told().1, &|_, _| {}).unwrap();
        fill(&mut store, 2).unwrap();
        store.release_memory();
        assert_eq!(store.residency(), None);
        assert_eq!(std::fs::read_dir(&base.0).unwrap().count(), 0, "the copy is removed");
        // Read and written on disk again.
        assert_eq!(store.metarecord_count().unwrap(), 2);
        fill(&mut store, 3).unwrap();
        store.hold_in_memory(&base.0, told().1, &|_, _| {}).unwrap();
        assert_eq!(store.metarecord_count().unwrap(), 5);
        // Letting go of a store not held is nothing.
        store.release_memory();
        store.release_memory();
        assert_eq!(store.residency(), None);
    }

    #[test]
    fn test_the_copies_a_crash_left_are_removed_and_those_in_use_kept() {
        let (mut store, _dir) = test_store();
        let base = base();
        store.hold_in_memory(&base.0, told().1, &|_, _| {}).unwrap();
        // Left by a daemon gone: its lock file, held by nobody.
        let stale = base.0.join("stale");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("held.lock"), b"").unwrap();
        std::fs::write(stale.join("data.mdb"), b"old").unwrap();
        memory::clean_stale(&base.0);
        assert!(!stale.exists());
        assert_eq!(std::fs::read_dir(&base.0).unwrap().count(), 1, "the copy in use stays");
        fill(&mut store, 1).unwrap();
        assert_in_step(&store);
    }
}
