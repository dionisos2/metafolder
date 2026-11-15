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
//! | meta          | name                             | counter / HEAD            |
//! | metarecords   | uuid                             | (empty: it exists)        |
//! | records       | dense id                         | the rows: field name · value |
//! | field_types   | name · value type                | rows of that type         |
//! | forest        | field · parent · name bytes      | uuid · dense id           |
//! | ops           | op id                            | the operation             |
//! | snaps         | op id · after? · index           | a snapshot row            |
//! | ops_by_label  | label · op id                    | —                         |
//! | ops_by_entity | uuid · op id                     | —                         |
//! | labels        | label id                         | name · description · parent · created |
//! | label_kids    | parent label · child label       | —                         |
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
//! Operations are grouped by nested labels (doc "Labels and contexts"): each
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
use crate::rows::{self, FieldRow, RawValue, TreeRow};
use crate::store::{Begin, LabelMeta, Log, MassKind, NewOp, Questions, Rows, WriteTxn};

mod derived;
mod mass;
mod massops;
pub mod memory;
mod source;
mod table;

pub use derived::Wear;
pub use source::{KvSource, RangeRows};
use table::{Db, Wtxn};

/// The parent key of a forest root: the zero uuid, which no metarecord has.
const ROOT: [u8; 16] = [0; 16];

/// What a write transaction answers to "which ids hold a row of this field".
type HoldersOf<'a> = dyn Fn(&str) -> Result<roaring::RoaringBitmap> + 'a;

#[derive(Clone, Copy)]
struct Tables {
    meta: Db,
    metarecords: Db,
    records: Db,
    mass: Db,
    field_types: Db,
    forest: Db,
    ops: Db,
    snaps: Db,
    ops_by_label: Db,
    ops_by_entity: Db,
    labels: Db,
    label_kids: Db,
    label_keys: Db,
    reverted_by: Db,
    watcher_ops: Db,
    user_ops: Db,
    // Derived (see `derived`).
    ids: Db,
    uuids: Db,
    sets: Db,
    parts: Db,
    kids: Db,
    grams: Db,
    hist: Db,
    events: Db,
}

/// The tables' names, in `Tables`' order.
const NAMES: [&str; table::TABLES] = [
    "meta",
    "metarecords",
    "records",
    "mass",
    "field_types",
    "forest",
    "ops",
    "snaps",
    "ops_by_label",
    "ops_by_entity",
    "labels",
    "label_kids",
    "label_keys",
    "reverted_by",
    "watcher_ops",
    "user_ops",
    "ids",
    "uuids",
    "sets",
    "parts",
    "kids",
    "grams",
    "hist",
    "events",
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
        let [meta, metarecords, records, mass, field_types, forest, ops, snaps, ops_by_label, ops_by_entity, labels, label_kids, label_keys, reverted_by, watcher_ops, user_ops, ids, uuids, sets, parts, kids, grams, hist, events] =
            dbs;
        Ok(Tables {
            meta,
            metarecords,
            records,
            mass,
            field_types,
            forest,
            ops,
            snaps,
            ops_by_label,
            ops_by_entity,
            labels,
            label_kids,
            label_keys,
            reverted_by,
            watcher_ops,
            user_ops,
            ids,
            uuids,
            sets,
            parts,
            kids,
            grams,
            hist,
            events,
        })
    }

    /// Every table, in the order of their indexes (see [`table::Change`]).
    fn all(&self) -> [Db; table::TABLES] {
        [
            self.meta,
            self.metarecords,
            self.records,
            self.mass,
            self.field_types,
            self.forest,
            self.ops,
            self.snaps,
            self.ops_by_label,
            self.ops_by_entity,
            self.labels,
            self.label_kids,
            self.label_keys,
            self.reverted_by,
            self.watcher_ops,
            self.user_ops,
            self.ids,
            self.uuids,
            self.sets,
            self.parts,
            self.kids,
            self.grams,
            self.hist,
            self.events,
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
    /// The keys the last committed write transaction wrote, per table — what
    /// the write cost assertions count ([`KvStore::last_write_keys`]).
    written: std::sync::Mutex<[u64; table::TABLES]>,
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

/// The version of the primary layout this code writes, `layout` in `meta`
/// (absent: the first one, rows keyed `uuid · row id` in `cells`; 2: the
/// history tree, operations chained to a parent; 3: the sequence, before the
/// operations were split by origin; 4: operations grouped by write; 5:
/// before the reverts of an operation were indexed; 6: a forest entry
/// without its metarecord's dense id; 7: a forest entry holding uuid · row id
/// · dense id, the rows one key each in `field_cells`; 8: a record's rows under
/// one key, its dense id, in `records`).
const LAYOUT: i64 = 8;

/// Where the dense id starts in a forest entry's value: uuid · dense id. The
/// id is what tells a position from one a deleted metarecord left behind,
/// without a read (doc "Deleting a metarecord in the store").
const FOREST_ID: usize = 16;

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

/// A value in its column form (`rows::encode_value` / `decode_value`), so a
/// name's exact bytes survive.
fn put_value(out: &mut Out, v: &Value) {
    let e = rows::encode_value(v);
    out.bytes(e.value_type.as_bytes())
        .opt_bytes(e.text.as_deref().map(str::as_bytes))
        .opt_int(e.int)
        .opt_int(e.real.map(|r| r.to_bits() as i64))
        .opt_bytes(e.uuid.as_deref())
        .opt_bytes(e.ref_repo.as_deref())
        .opt_bytes(e.name.as_deref().map(str::as_bytes))
        .opt_bytes(e.name_bytes.as_deref());
}

fn get_value(r: &mut In) -> Result<Value> {
    let raw = RawValue {
        value_type: r.string()?,
        text: r.opt_string()?,
        int: r.opt_int()?,
        real: r.opt_int()?.map(|b| f64::from_bits(b as u64)),
        uuid: r.opt_bytes()?.map(<[u8]>::to_vec),
        ref_repo: r.opt_bytes()?.map(<[u8]>::to_vec),
        name: r.opt_string()?,
        name_bytes: r.opt_bytes()?.map(<[u8]>::to_vec),
    };
    rows::decode_value(raw)
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

/// A record's rows, the value of its key in `records`: how many, then each
/// one's name and value.
fn enc_rows(rows: &[FieldRow]) -> Vec<u8> {
    let mut out = Out::default();
    out.int(rows.len() as i64);
    for row in rows {
        out.bytes(row.name.as_bytes());
        put_value(&mut out, &row.value);
    }
    out.0
}

fn dec_rows(b: &[u8]) -> Result<Vec<FieldRow>> {
    let mut r = In(b);
    let n = r.int()?;
    let mut rows = Vec::with_capacity(n.max(0) as usize);
    for _ in 0..n {
        rows.push(FieldRow { name: r.string()?, value: get_value(&mut r)? });
    }
    Ok(rows)
}

fn enc_op(op: &NewOp, label: i64, timestamp: i64) -> Vec<u8> {
    let mut out = Out::default();
    out.int(label)
        .int(timestamp)
        .opt_bytes(op.origin.as_deref().map(str::as_bytes))
        .bytes(op.op_type.as_str().as_bytes())
        .bytes(op.entity.as_bytes())
        // Where the entity's versions before and after were, until they
        // stopped being recorded (doc "Log storage"): two absent integers.
        .opt_int(None)
        .opt_int(None)
        .opt_bytes(op.field_name.as_deref().map(str::as_bytes))
        .opt_int(op.reverts_op_id);
    out.0
}

fn dec_op(id: i64, b: &[u8]) -> Result<OpRow> {
    let mut r = In(b);
    let (label_id, timestamp, origin) = (r.int()?, r.int()?, r.opt_string()?);
    let (op_type, entity_uuid) = (r.string()?, uuid_of(r.bytes()?));
    // The versions an older store recorded: read past.
    r.opt_int()?;
    r.opt_int()?;
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

/// A label, and the key of the command that opened it, if one did (a
/// command's later writes find it by that key).
fn enc_label(m: &LabelMeta, key: Option<Uuid>) -> Vec<u8> {
    let mut out = Out::default();
    out.bytes(m.name.as_bytes())
        .opt_bytes(m.description.as_deref().map(str::as_bytes))
        .opt_int(m.parent)
        .int(m.created)
        .opt_bytes(key.as_ref().map(|k| &k.as_bytes()[..]));
    out.0
}

/// The command key [`enc_label`] stored after the label.
fn dec_label_key(b: &[u8]) -> Result<Option<Uuid>> {
    let mut r = In(b);
    r.string()?;
    r.opt_bytes()?;
    r.opt_int()?;
    r.int()?;
    Ok(r.opt_bytes()?.map(uuid_of))
}

fn dec_label(b: &[u8]) -> Result<LabelMeta> {
    let mut r = In(b);
    Ok(LabelMeta {
        name: r.string()?,
        description: r.opt_string()?,
        parent: r.opt_int()?,
        created: r.int()?,
    })
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
            (t.meta.get(&r, b"derived")?.map(from_be), t.meta.get(&r, b"layout")?.map(from_be))
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
            written: Default::default(),
            memory: None,
            resident: Default::default(),
            durable,
        };
        {
            let r = store.env.read_txn()?;
            let pending = store.t.mass.len(&r)? > 0;
            store.mass.store(pending, std::sync::atomic::Ordering::Relaxed);
            let deleting =
                store.t.mass.prefix_iter(&r, &mass::DELETES_PART.to_be_bytes())?.next().is_some();
            store.deletes.store(deleting, std::sync::atomic::Ordering::Relaxed);
        }
        let mut derived = derived;
        if layout != Some(LAYOUT) {
            let rebuild =
                store.migrate_layout(layout).context("migrate the key-value store's rows")?;
            if rebuild {
                derived = None;
            }
        }
        // A store from before the derived key spaces (or of another format of
        // them) gets them derived now: their migration.
        if derived != Some(derived::DERIVED_VERSION) {
            store.reindex().context("derive the key-value store's indexes")?;
        }
        Ok(store)
    }
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
        options.map_size(map_size).max_dbs(32);
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
    /// Whether any part has entries pending, which a record is read with
    /// applied ([`mass`]).
    mass: bool,
    /// Whether a deletion of many records is pending: the records it deleted
    /// are still stored, and a read of one sifts it through the universe.
    deletes: bool,
    /// The universe's chunks read so far, when the read looks them up itself.
    universe: RefCell<HashMap<u16, roaring::RoaringBitmap>>,
}

impl Read<'_> {
    fn meta(&self, name: &str) -> Result<Option<i64>> {
        Ok(self.t.meta.get(self.r, name.as_bytes())?.map(from_be))
    }

    fn exists(&self, uuid: Uuid) -> Result<bool> {
        self.count(1)?;
        if self.t.metarecords.get(self.r, uuid.as_bytes())?.is_none() {
            return Ok(false);
        }
        self.holds(uuid)
    }

    /// Whether a metarecord whose key is stored still exists: with a deletion
    /// pending, a deleted one's key stays until the fold, and its id is out of
    /// the universe.
    fn holds(&self, uuid: Uuid) -> Result<bool> {
        if !self.deletes {
            return Ok(true);
        }
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
            let k = derived::set_key(derived::UNIVERSE, None, chunk);
            slot.insert(match self.t.sets.get(self.r, &k)? {
                Some(bytes) => derived::decode_set(bytes)?,
                None => roaring::RoaringBitmap::new(),
            });
        }
        Ok(seen[&chunk].contains(id))
    }

    /// Whether a forest entry (`v`: uuid · dense id) is the position
    /// of a metarecord that exists.
    fn placed(&self, v: &[u8]) -> Result<bool> {
        self.live(derived::dense(&v[FOREST_ID..]))
    }

    /// The rows of the record of dense id `id`: its one key. None for a
    /// metarecord a pending deletion took out of the universe.
    fn record(&self, id: u32) -> Result<Vec<FieldRow>> {
        if self.deletes && !self.live(id)? {
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

    fn field_rows(&self, name: &str) -> Result<Vec<(Uuid, FieldRow)>> {
        let mut out = Vec::new();
        for id in self.holder_ids(name)? {
            if !self.live(id)? {
                continue;
            }
            let uuid = self.uuid_of_id(id)?;
            for row in self.record(id)?.into_iter().filter(|r| r.name == name) {
                out.push((uuid, row));
            }
        }
        Ok(out)
    }

    fn children(&self, field: &str, parent: &[u8; 16]) -> Result<Vec<(Uuid, Vec<u8>)>> {
        let prefix = key(&[&name_key(field), parent]);
        let mut out = Vec::new();
        for e in self.t.forest.prefix_iter(self.r, &prefix)? {
            let (k, v) = e?;
            // Counted as read, not after: a folder can hold the repository,
            // and a query stopped meanwhile stops here (`crate::interrupt`).
            self.count(1)?;
            if !self.placed(v)? {
                continue;
            }
            out.push((uuid_of(v), self.node_name(field, &k[prefix.len()..], v)?));
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
        let prefix = key(&[&name_key(field), parent]);
        let bound = after.map(|a| key(&[&prefix, &node_key(a)]));
        let top = key(&[&prefix, &[0xFF; 400]]);
        let entries: Entries<'_> = if descending {
            let hi = match &bound {
                Some(b) => std::ops::Bound::Excluded(b.as_slice()),
                None => std::ops::Bound::Excluded(top.as_slice()),
            };
            let range = (std::ops::Bound::Included(prefix.as_slice()), hi);
            Box::new(self.t.forest.rev_range(self.r, &range)?)
        } else {
            let lo = match &bound {
                Some(b) => std::ops::Bound::Excluded(b.as_slice()),
                None => std::ops::Bound::Included(prefix.as_slice()),
            };
            let range = (lo, std::ops::Bound::Excluded(top.as_slice()));
            Box::new(self.t.forest.range(self.r, &range)?)
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
            out.push((uuid_of(v), self.node_name(field, &k[prefix.len()..], v)?));
            if out.len() >= limit {
                break;
            }
        }
        self.count(out.len() as u64 + 1)?;
        Ok(out)
    }

    /// A position's name, from the end of its forest key — or, for a name
    /// too long to be keyed whole, from its record's row (`v` is uuid · dense
    /// id).
    fn node_name(&self, field: &str, key_end: &[u8], v: &[u8]) -> Result<Vec<u8>> {
        if key_end.len() <= NODE_MAX {
            return Ok(key_end.to_vec());
        }
        let rows = self.record(derived::dense(&v[FOREST_ID..]))?;
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
        match self.t.ops.get(self.r, &be(id))? {
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
        for e in self.t.ops.rev_range(self.r, &range)? {
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
        for e in self.t.ops_by_label.prefix_iter(self.r, &be(label))? {
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
        Ok(match self.t.ops_by_label.prefix_iter(self.r, &be(label))?.next() {
            Some(e) => Some(from_be(&e?.0[8..])),
            None => None,
        })
    }

    fn label_children(&self, label: i64) -> Result<Vec<i64>> {
        let mut out = Vec::new();
        for e in self.t.label_kids.prefix_iter(self.r, &be(label))? {
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
        let Some((first, v)) = self.t.ops.first(self.r)? else { return Ok(None) };
        if op_timestamp(v)? > timestamp_ms {
            return Ok(None);
        }
        // A write to many records is one entry holding many ids, all of its time.
        let end_of = |id: i64, v: &[u8]| -> Result<i64> {
            Ok(massops::Stub::decode(id, v)?.map_or(id, |s| s.last()))
        };
        let mut lo = end_of(from_be(first), v)?;
        let Some((last, v)) = self.t.ops.last(self.r)? else { return Ok(None) };
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
            let (k, v) =
                self.t.ops.range(self.r, &range)?.next().context("an operation past mid")??;
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

/// `Rows`, `Log` and `Questions` for anything that can lend a `Read`.
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
                    if $read.deletes {
                        // The keys of the metarecords a deletion took out stay.
                        let universe =
                            derived::read_set($read.t, $read.r, derived::UNIVERSE, None)?;
                        return Ok(universe.len() as usize);
                    }
                    Ok($read.t.metarecords.len($read.r)? as usize)
                })
            }
            fn metarecords(&self) -> Result<Vec<Uuid>> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    let mut out = Vec::new();
                    for e in $read.t.metarecords.iter($read.r)? {
                        $read.count(1)?;
                        let uuid = uuid_of(e?.0);
                        if $read.holds(uuid)? {
                            out.push(uuid);
                        }
                    }
                    Ok(out)
                })
            }
            fn field_rows(&self, name: &str) -> Result<Vec<(Uuid, FieldRow)>> {
                let $me = self;
                $with(&mut |$read: &Read| $read.field_rows(name))
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
                        out.push(String::from_utf8(k[prefix.len()..].to_vec())?);
                    }
                    Ok(out)
                })
            }
            fn holders(&self, name: &str) -> Result<Vec<Uuid>> {
                let $me = self;
                $with(&mut |$read: &Read| $read.holders(name))
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
                    let k = key(&[&name_key(field), &p, &node_key(name)]);
                    $read.count(1)?;
                    let Some(v) = $read.t.forest.get($read.r, &k)? else { return Ok(None) };
                    if !$read.placed(v)? {
                        return Ok(None);
                    }
                    // A hashed key names the right position only if the name
                    // read back is the one asked for.
                    let found = $read.node_name(field, &k[k.len() - node_key(name).len()..], v)?;
                    Ok((found == name).then(|| uuid_of(v)))
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
                    for e in $read.t.forest.iter($read.r)? {
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
                        let parent = uuid_of(&k[end + 2..end + 18]);
                        let name = $read.node_name(&field, &k[end + 18..], v)?;
                        out.push(TreeRow {
                            field_name: field,
                            uuid: uuid_of(v),
                            parent: (!parent.is_nil()).then_some(parent),
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
                    $read.meta("head")
                })
            }
            fn op(&self, id: i64) -> Result<Option<OpRow>> {
                let $me = self;
                $with(&mut |$read: &Read| $read.op(id))
            }
            fn snapshots(&self, op_id: i64, after: bool) -> Result<Vec<FieldRow>> {
                let $me = self;
                $with(&mut |$read: &Read| $read.snapshot_rows(op_id, after))
            }
            fn first_op(&self) -> Result<Option<i64>> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    $read.count(1)?;
                    Ok($read.t.ops.first($read.r)?.map(|(k, _)| from_be(k)))
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
                        if let Some(b) = $read.t.labels.get($read.r, &be(id))? {
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
                    let ops = $read.t.ops.len($read.r)? as i64 + $read.mass_extra()?;
                    Ok((ops, $read.t.labels.len($read.r)? as i64))
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
            fn reverters(&self, op: i64) -> Result<Vec<i64>> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    let mut out = Vec::new();
                    for e in $read.t.reverted_by.prefix_iter($read.r, &be(op))? {
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
                    Ok($read.t.label_keys.get($read.r, key.as_bytes())?.map(from_be))
                })
            }
            fn watcher_label(&self) -> Result<Option<i64>> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    $read.count(1)?;
                    $read.meta("watcher_label")
                })
            }
            fn entity_ops_after(&self, entity: Uuid, after: i64) -> Result<Vec<OpRow>> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    let ids = $read.entity_op_ids_after(entity, after)?;
                    $read.ops_of(ids.into_iter())
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
        mass: me.mass_pending(),
        deletes: me.deletes_pending(),
        universe: Default::default(),
    })
});

/// A write transaction is answered by `store::derive`: no query source reads
/// its uncommitted writes.
impl Questions for KvTxn<'_> {}

/// The schema check's candidates come off the derived key spaces (doc
/// "Schema"); the other questions are `store::derive`'s.
impl Questions for KvStore {
    fn wrong_type(&self, field: &str, allowed: &str, limit: i64) -> Result<Vec<Uuid>> {
        self.source()?.wrong_type(field, allowed, limit)
    }
    fn count_over(&self, field: &str, max: i64, limit: i64) -> Result<Vec<Uuid>> {
        self.source()?.count_over(field, max, limit)
    }
    fn count_under(&self, field: &str, min: i64, limit: i64) -> Result<Vec<Uuid>> {
        self.source()?.count_under(field, min, limit)
    }
    fn missing(&self, field: &str, limit: i64) -> Result<Vec<Uuid>> {
        self.source()?.missing(field, limit)
    }
    fn typed_missing(&self, types: &[String], field: &str, limit: i64) -> Result<Vec<Uuid>> {
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
        f(&Read {
            t: &self.t,
            r: txn,
            reads: Some(self.reads),
            live: Some(&live),
            holders: Some(&holders),
            mass: self.mass_pending(),
            deletes: self.deletes_pending(),
            universe: Default::default(),
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
    /// The store's read counter: a write's reads count as much as a query's
    /// (`tests/log_cost.rs` holds the trim to it).
    reads: &'e std::sync::atomic::AtomicU64,
    /// Where the store keeps what its last write wrote.
    written: &'e std::sync::Mutex<[u64; table::TABLES]>,
    /// The universe's chunks the transaction read without changing them
    /// (a changed one is in `sets`).
    universe: RefCell<HashMap<u16, roaring::RoaringBitmap>>,
    /// Each field's trigram state as the transaction read or changed it, and
    /// the fields whose state it changed, written at commit.
    grams: RefCell<HashMap<String, derived::GramState>>,
    grams_changed: RefCell<std::collections::HashSet<String>>,
    /// The field a write to many records is being written on: its rows
    /// derive nothing, the event that ends it does (`derived::events`).
    mass: RefCell<Option<String>>,
    /// How many events each field has pending, as read or changed.
    events: RefCell<HashMap<String, usize>>,
    /// The store's flag that some part has entries pending: a write to many
    /// records raises it.
    mass_lists: &'e std::sync::atomic::AtomicBool,
    /// The store's flag that a deletion of many records is pending.
    deletes: &'e std::sync::atomic::AtomicBool,
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
    ///   index of its reverts (5). The log is emptied, the metarecords kept (user
    ///   decision, October 2026; doc "The operation log"). Layouts 6 and 7 keep
    ///   theirs.
    ///
    /// A new store has nothing to move and only records the layout. Answers
    /// whether the derived key spaces must be derived again: they must when a
    /// metarecord was given an id, which they do not know.
    fn migrate_layout(&mut self, from: Option<i64>) -> Result<bool> {
        const BATCH: usize = 10_000;
        // The tables of older layouts, opened (if there) before the write:
        // heed opens a database in a transaction of its own.
        const GONE: [&str; 4] = ["op_children", "restorations", "ops_by_rev", "revisions"];
        const ROWS: [&str; 4] = ["cells", "field_cells", "row_owner", "by_field"];
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
        let mut assigned = false;
        {
            let w = txn.txn.borrow_mut();
            let t = txn.t;
            // Every metarecord has a dense id from here on.
            let uuids: Vec<[u8; 16]> = {
                let mut out = Vec::new();
                for e in t.metarecords.iter(&w)? {
                    out.push(e?.0.try_into().expect("a 16-byte uuid"));
                }
                out
            };
            drop(w);
            for uuid in &uuids {
                if txn.dense_id(uuid)?.is_none() {
                    let id = u32::try_from(txn.next("next_id")?).context("dense ids exhausted")?;
                    let mut w = txn.txn.borrow_mut();
                    t.ids.put(&mut w, uuid, &id.to_be_bytes())?;
                    t.uuids.put(&mut w, &id.to_be_bytes(), uuid)?;
                    assigned = true;
                }
            }
            let mut w = txn.txn.borrow_mut();
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
                        let row = dec_row(v)?;
                        match &mut group {
                            Some((u, rows)) if *u == uuid => rows.push((id, row)),
                            _ => {
                                if let Some((u, rows)) = group.take() {
                                    file_converted(&t, &mut w, &u, rows)?;
                                }
                                group = Some((uuid, vec![(id, row)]));
                            }
                        }
                    }
                }
                if let Some((u, rows)) = group.take() {
                    file_converted(&t, &mut w, &u, rows)?;
                }
            }
            for old in rows.iter().flatten() {
                old.clear(w.unjournaled())?;
            }
            // The forest: a uuid and a dense id.
            let mut after: Option<Vec<u8>> = None;
            loop {
                let batch: Vec<(Vec<u8>, Vec<u8>)> = {
                    let lo = match &after {
                        Some(k) => std::ops::Bound::Excluded(k.as_slice()),
                        None => std::ops::Bound::Unbounded,
                    };
                    let range = (lo, std::ops::Bound::Unbounded);
                    t.forest
                        .range(&w, &range)?
                        .take(BATCH)
                        .map(|e| e.map(|(k, v)| (k.to_vec(), v.to_vec())))
                        .collect::<heed::Result<_>>()?
                };
                let Some((last, _)) = batch.last() else { break };
                after = Some(last.clone());
                for (k, v) in &batch {
                    // A position of a metarecord no longer there is dropped.
                    match derived::id_of(&t, &w, &v[..16])? {
                        Some(id) if t.metarecords.get(&w, &v[..16])?.is_some() => {
                            t.forest.put(&mut w, k, &[&v[..16], &id.to_be_bytes()[..]].concat())?;
                        }
                        _ => {
                            t.forest.delete(&mut w, k)?;
                        }
                    }
                }
            }
            if from.is_none_or(|layout| layout <= 5) {
                for old in gone.into_iter().flatten() {
                    old.clear(w.unjournaled())?;
                }
                for db in [
                    t.ops,
                    t.snaps,
                    t.ops_by_label,
                    t.ops_by_entity,
                    t.labels,
                    t.label_kids,
                    t.label_keys,
                    t.reverted_by,
                    t.watcher_ops,
                    t.user_ops,
                ] {
                    db.clear(&mut w)?;
                }
                t.meta.delete(&mut w, b"head")?;
                t.meta.delete(&mut w, b"watcher_label")?;
            }
        }
        txn.meta_put("layout", LAYOUT)?;
        txn.finish()?;
        Ok(assigned)
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
    uuid: &[u8; 16],
    mut rows: ConvertedRows,
) -> Result<()> {
    if t.metarecords.get(w, uuid)?.is_none() {
        return Ok(());
    }
    let id = derived::id_of(t, w, uuid)?.context("a metarecord with no dense id")?;
    rows.sort_by_key(|(row_id, _)| *row_id);
    let rows: Vec<FieldRow> = rows.into_iter().map(|(_, row)| row).collect();
    t.records.put(w, &id.to_be_bytes(), &enc_rows(&rows))?;
    Ok(())
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
            reads: &store.reads,
            mass_lists: &store.mass,
            deletes: &store.deletes,
            written: &store.written,
            universe: RefCell::new(HashMap::new()),
            grams: RefCell::new(HashMap::new()),
            grams_changed: RefCell::new(Default::default()),
            mass: RefCell::new(None),
            events: RefCell::new(HashMap::new()),
        })
    }

    fn meta_put(&self, name: &str, n: i64) -> Result<()> {
        self.t.meta.put(&mut self.txn.borrow_mut(), name.as_bytes(), &be(n))?;
        Ok(())
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
        Ok(())
    }
}

impl KvTxn<'_> {
    fn next(&self, counter: &str) -> Result<i64> {
        let mut w = self.txn.borrow_mut();
        let n = self.t.meta.get(&w, counter.as_bytes())?.map_or(1, from_be);
        self.t.meta.put(&mut w, counter.as_bytes(), &be(n + 1))?;
        Ok(n)
    }

    fn count_type(&self, name: &str, value: &Value, delta: i64) -> Result<()> {
        if matches!(value, Value::Nothing) {
            return Ok(());
        }
        self.count_type_named(name, rows::encode_value(value).value_type.as_bytes(), delta)
    }

    /// [`Self::count_type`] for a type named.
    fn count_type_named(&self, name: &str, value_type: &[u8], delta: i64) -> Result<()> {
        let k = key(&[&name_key(name), value_type]);
        let mut w = self.txn.borrow_mut();
        let n = self.t.field_types.get(&w, &k)?.map_or(0, from_be) + delta;
        if n <= 0 {
            self.t.field_types.delete(&mut w, &k)?;
        } else {
            self.t.field_types.put(&mut w, &k, &be(n))?;
        }
        Ok(())
    }

    /// Writes a record's rows under its key; a record with none has no key.
    fn put_record(&self, id: u32, rows: &[FieldRow]) -> Result<()> {
        let mut w = self.txn.borrow_mut();
        if rows.is_empty() {
            self.t.records.delete(&mut w, &id.to_be_bytes())?;
        } else {
            self.t.records.put(&mut w, &id.to_be_bytes(), &enc_rows(rows))?;
        }
        Ok(())
    }

    /// What a row's going away changes outside its record: its forest
    /// position, the count of its type, the derived data. `before` holds the
    /// rows the record held before the write that removes this one — one or
    /// many — which the count of values of the row's field is derived from.
    fn row_gone(&self, owner: Uuid, row: &FieldRow, before: &[FieldRow]) -> Result<()> {
        if let Some(k) = Self::forest_key(&row.name, &row.value) {
            self.t.forest.delete(&mut self.txn.borrow_mut(), &k)?;
        }
        self.count_type(&row.name, &row.value, -1)?;
        self.derive_row(owner, &row.name, &row.value, -1, valued_in(before, &row.name))
    }

    fn forest_key(name: &str, value: &Value) -> Option<Vec<u8>> {
        let (parent, node) = value.position()?;
        let p = parent.map_or(ROOT, |p| *p.as_bytes());
        Some(key(&[&name_key(name), &p, &node_key(node.as_bytes())]))
    }

    /// Writes a new label, and its place among its parent's children.
    fn put_label(&self, meta: &LabelMeta, command: Option<Uuid>) -> Result<i64> {
        let id = self.next("next_label")?;
        let mut w = self.txn.borrow_mut();
        self.t.labels.put(&mut w, &be(id), &enc_label(meta, command))?;
        if let Some(parent) = meta.parent {
            self.t.label_kids.put(&mut w, &key(&[&be(parent), &be(id)]), &[])?;
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
                    mass: false,
                    deletes: false,
                    universe: Default::default(),
                };
                let Some(meta) = self.t.labels.get(&txn, &be(label))? else { break };
                let meta = dec_label(meta)?;
                let empty = read.label_first_op(label)?.is_none()
                    && read.label_children(label)?.is_empty()
                    && read.meta("watcher_label")? != Some(label);
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
        t.ops.delete(&mut w, &be(id))?;
        let prefix = be(id);
        let snaps: Vec<Vec<u8>> = t
            .snaps
            .prefix_iter(&w, &prefix)?
            .map(|e| e.map(|(k, _)| k.to_vec()))
            .collect::<std::result::Result<_, _>>()?;
        for k in snaps {
            t.snaps.delete(&mut w, &k)?;
        }
        t.ops_by_label.delete(&mut w, &key(&[&be(op.label_id), &be(id)]))?;
        if let Some(target) = op.reverts_op_id {
            t.reverted_by.delete(&mut w, &key(&[&be(target), &be(id)]))?;
        }
        t.ops_by_entity.delete(&mut w, &key(&[op.entity_uuid.as_bytes(), &be(id)]))?;
        t.watcher_ops.delete(&mut w, &be(id))?;
        t.user_ops.delete(&mut w, &be(id))?;
        Ok(())
    }

    /// The table of the operations of `origin`'s retention class.
    fn class(&self, origin: Option<&str>) -> Db {
        if origin == Some(crate::log::ORIGIN_WATCHER) {
            self.t.watcher_ops
        } else {
            self.t.user_ops
        }
    }
}

impl WriteTxn for KvTxn<'_> {
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
        self.mass_log(kind, field, values, ids, label, timestamp, origin)
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

    fn holder_ids(&self, field: &str) -> Result<Option<roaring::RoaringBitmap>> {
        self.holder_ids_now(field).map(Some)
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

    fn mass_begin(&self, field: &str, kind: MassKind, values: &[Value]) -> Result<bool> {
        if !self.mass_eligible(field, kind, values)? {
            return Ok(false);
        }
        KvTxn::mass_begin(self, field);
        Ok(true)
    }

    fn mass_end(
        &self,
        field: &str,
        kind: MassKind,
        values: Vec<Value>,
        uuids: &[Uuid],
    ) -> Result<()> {
        KvTxn::mass_end(self, field, kind, values, uuids)
    }

    fn create_metarecord(&self, uuid: Uuid) -> Result<()> {
        let mut w = self.txn.borrow_mut();
        if self.t.metarecords.get(&w, uuid.as_bytes())?.is_some() {
            drop(w);
            // A deleted one's key stays until the fold, which leaves it be if
            // the metarecord is back by then.
            if Rows::exists(self, uuid)? {
                bail!("metarecord {uuid} already exists");
            }
            w = self.txn.borrow_mut();
        }
        // Its existence, and nothing else: the version is computed from the
        // rows when read (doc "Metarecord version").
        self.t.metarecords.put(&mut w, uuid.as_bytes(), &[])?;
        drop(w);
        self.derive_created(uuid)
    }

    /// Removes a metarecord: its rows, its existence and its place in the
    /// universe — and nothing of what the indexes hold about it, which every
    /// read sifts through the universe (doc "Deleting a metarecord in the
    /// store"). On a hard disk each index entry is a seek, and a file has
    /// dozens.
    fn remove_metarecord(&self, uuid: Uuid) -> Result<()> {
        // A parent of metarecords that exist is unpicked row by row: its
        // subtree leaves its ancestors' descendants, which no universe sifts.
        // The `Writer` refuses to commit that; a write deleting a folder
        // before its files passes here.
        if self.has_live_children(uuid)? {
            self.delete_rows(uuid, None)?;
        } else {
            let id = self.dense_id(uuid.as_bytes())?;
            let mut rows = Vec::new();
            if let Some(id) = id {
                rows = self.load_record(id)?.plain_rows();
                self.put_record(id, &[])?;
            }
            for row in &rows {
                self.count_type(&row.name, &row.value, -1)?;
                self.derive_dropped(uuid, &row.name, &row.value)?;
            }
        }
        self.t.metarecords.delete(&mut self.txn.borrow_mut(), uuid.as_bytes())?;
        self.derive_removed(uuid)
    }

    fn insert_row(&self, uuid: Uuid, name: &str, value: &Value) -> Result<()> {
        if !Rows::exists(self, uuid)? {
            bail!("a row for metarecord {uuid}, which does not exist");
        }
        let id =
            self.dense_id(uuid.as_bytes())?.with_context(|| format!("no dense id for {uuid}"))?;
        // A write to many records: the record is not written, the entry that
        // ends the write is (doc "Mass writes in the store").
        if self.in_mass(name) {
            return self.count_type(name, value, 1);
        }
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
        let forest = Self::forest_key(name, value);
        if let Some(k) = &forest {
            // Occupied by a metarecord that exists: what a deleted one left
            // there is written over.
            let txn = self.txn.borrow();
            let occupied = match self.t.forest.get(&txn, k)? {
                Some(v) => self.live_in(&txn, derived::dense(&v[FOREST_ID..]))?,
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
        if let Some(k) = &forest {
            let mut w = self.txn.borrow_mut();
            self.t.forest.put(&mut w, k, &key(&[uuid.as_bytes(), &id.to_be_bytes()]))?;
        }
        self.count_type(name, value, 1)?;
        self.derive_row(uuid, name, value, 1, valued_in(&before, name))
    }

    fn delete_row(&self, owner: Uuid, name: &str, value: &Value) -> Result<()> {
        let Some(id) = self.dense_id(owner.as_bytes())? else { return Ok(()) };
        let loaded = self.load_record(id)?;
        let Some(at) = loaded.rows.iter().position(|(r, _)| r.name == name && &r.value == value)
        else {
            return Ok(());
        };
        if self.in_mass(name) {
            return self.count_type(name, value, -1);
        }
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
        let Some(id) = self.dense_id(uuid.as_bytes())? else { return Ok(()) };
        let loaded = self.load_record(id)?;
        let wanted = |row: &FieldRow| name.is_none_or(|n| n == row.name);
        let gone: Vec<FieldRow> =
            loaded.rows.iter().filter(|(r, _)| wanted(r)).map(|(r, _)| r.clone()).collect();
        if gone.is_empty() {
            return Ok(());
        }
        if name.is_some_and(|n| self.in_mass(n)) {
            for row in &gone {
                self.count_type(&row.name, &row.value, -1)?;
            }
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
        self.t.label_keys.put(&mut self.txn.borrow_mut(), key.as_bytes(), &be(id))?;
        Ok(id)
    }

    fn drop_label(&self, id: i64) -> Result<()> {
        let Some(meta) = Log::labels(self, &[id])?.remove(&id) else { return Ok(()) };
        let command = {
            let txn = self.txn.borrow();
            match self.t.labels.get(&txn, &be(id))? {
                Some(b) => dec_label_key(b)?,
                None => None,
            }
        };
        let mut w = self.txn.borrow_mut();
        self.t.labels.delete(&mut w, &be(id))?;
        if let Some(parent) = meta.parent {
            self.t.label_kids.delete(&mut w, &key(&[&be(parent), &be(id)]))?;
        }
        if let Some(command) = command {
            self.t.label_keys.delete(&mut w, command.as_bytes())?;
        }
        Ok(())
    }

    fn set_watcher_label(&self, id: i64) -> Result<()> {
        self.meta_put("watcher_label", id)
    }

    fn append_ops(&self, label: i64, timestamp: i64, ops: &[NewOp]) -> Result<i64> {
        let base = {
            let w = self.txn.borrow();
            self.t.meta.get(&w, b"next_op")?.map_or(1, from_be)
        };
        let mut w = self.txn.borrow_mut();
        let t = self.t;
        for (i, op) in ops.iter().enumerate() {
            let id = base + i as i64;
            t.ops.put(&mut w, &be(id), &enc_op(op, label, timestamp))?;
            for (after, rows) in [(0u8, &op.before), (1u8, &op.after)] {
                for (n, row) in rows.iter().enumerate() {
                    let k = key(&[&be(id), &[after], &(n as u32).to_be_bytes()]);
                    t.snaps.put(&mut w, &k, &enc_row(row))?;
                }
            }
            t.ops_by_label.put(&mut w, &key(&[&be(label), &be(id)]), &[])?;
            if let Some(target) = op.reverts_op_id {
                t.reverted_by.put(&mut w, &key(&[&be(target), &be(id)]), &[])?;
            }
            t.ops_by_entity.put(&mut w, &key(&[op.entity.as_bytes(), &be(id)]), &[])?;
            self.class(op.origin.as_deref()).put(&mut w, &be(id), &[])?;
        }
        let last = base + ops.len() as i64 - 1;
        t.meta.put(&mut w, b"next_op", &be(last + 1))?;
        Ok(last)
    }

    fn set_head(&self, op: Option<i64>) -> Result<()> {
        let mut w = self.txn.borrow_mut();
        match op {
            Some(op) => self.t.meta.put(&mut w, b"head", &be(op))?,
            None => {
                self.t.meta.delete(&mut w, b"head")?;
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
        for (table, limit) in
            [(self.t.watcher_ops, retention.watcher_ops), (self.t.user_ops, retention.user_ops)]
        {
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
        for db in [t.metarecords, t.records, t.field_types, t.forest] {
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
