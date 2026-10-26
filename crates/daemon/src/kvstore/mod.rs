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
//! | metarecords   | uuid                             | version                   |
//! | field_cells   | uuid · field name · row id       | field name · value        |
//! | row_owner     | row id                           | uuid · field name         |
//! | by_field      | name · row id                    | uuid                      |
//! | field_types   | name · value type                | rows of that type         |
//! | forest        | field · parent · name bytes      | uuid · row id             |
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
//! A record's rows of one field are one prefix of `field_cells`: checking a
//! candidate on the field searched reads those rows, not all of its record's
//! (doc "Store tables"). `row_owner` holds that prefix, so a
//! row is still addressed by its id alone. The first layout kept the rows in
//! `cells` keyed `uuid · row id`; [`KvStore::open`] migrates it
//! ([`LAYOUT`]).
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
//! Row ids, operation ids and label ids are allocated from counters in
//! `meta` and never reused; a row put back under its own id moves the counter
//! past it.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::Path;

use anyhow::{bail, Context, Result};
use heed::types::Bytes;
use heed::{CommitOrAbort, Database, Env, EnvOpenOptions, RoTxn, RwTxn, WithoutTls};
use metafolder_core::metarecord::{TreeName, Value};
use uuid::Uuid;

use crate::error::DomainError;
use crate::log::{OpRow, Retention};
use crate::rows::{self, FieldRow, RawValue, TreeRow};
use crate::store::{Begin, LabelMeta, Log, NewOp, Questions, Rows, WriteTxn};

mod derived;
pub mod resident;
mod source;

pub use source::KvSource;

type Db = Database<'static, Bytes, Bytes>;

/// The parent key of a forest root: the zero uuid, which no metarecord has.
const ROOT: [u8; 16] = [0; 16];

#[derive(Clone, Copy)]
struct Tables {
    meta: Db,
    metarecords: Db,
    cells: Db,
    row_owner: Db,
    by_field: Db,
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
}

/// A repository database on LMDB.
pub struct KvStore {
    env: Env<WithoutTls>,
    t: Tables,
    /// Held for the store's lifetime: one daemon per repository.
    _lock: File,
    /// Where the store lives: the free space of its filesystem is the room
    /// a write transaction starts with ([`map_to_grant`]).
    dir: std::path::PathBuf,
    /// Keys read so far by the store's reads and its query sources — what
    /// the cost assertions count (`tests/kv_cost.rs`).
    reads: std::sync::atomic::AtomicU64,
    /// What of the map is held in memory, once [`KvStore::make_resident`]
    /// asked for it (doc "Holding a repository in memory"); shared with
    /// whoever reports it, so that reading it never waits on the store.
    resident: ResidencyHandle,
}

/// What a store holds in memory, readable without the store itself — `GET
/// /repos` must not wait behind a write for it.
#[derive(Clone, Default)]
pub struct ResidencyHandle(std::sync::Arc<std::sync::Mutex<Option<resident::Resident>>>);

impl ResidencyHandle {
    /// `None` until the store was asked to be held in memory.
    pub fn get(&self) -> Option<resident::Residency> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|r| r.residency())
    }
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

/// The prefix of a record's rows of one field in `field_cells` — and what
/// `row_owner` maps each of their ids to.
fn cell_prefix(uuid: &[u8], field: &str) -> Vec<u8> {
    key(&[uuid, &name_key(field)])
}

/// The version of the primary layout this code writes, `layout` in `meta`
/// (absent: the first one, rows keyed `uuid · row id` in `cells`; 2: the
/// history tree, operations chained to a parent; 3: the sequence, before the
/// operations were split by origin; 4: operations grouped by write; 5:
/// before the reverts of an operation were indexed).
const LAYOUT: i64 = 6;

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

fn enc_row(row: &FieldRow) -> Vec<u8> {
    let mut out = Out::default();
    out.int(row.id).bytes(row.name.as_bytes());
    put_value(&mut out, &row.value);
    out.0
}

fn dec_row(b: &[u8]) -> Result<FieldRow> {
    let mut r = In(b);
    Ok(FieldRow { id: r.int()?, name: r.string()?, value: get_value(&mut r)? })
}

fn enc_op(op: &NewOp, label: i64, timestamp: i64) -> Vec<u8> {
    let mut out = Out::default();
    out.int(label)
        .int(timestamp)
        .opt_bytes(op.origin.as_deref().map(str::as_bytes))
        .bytes(op.op_type.as_str().as_bytes())
        .bytes(op.entity.as_bytes())
        .opt_int(op.version_before.map(|v| v as i64))
        .opt_int(op.version_after.map(|v| v as i64))
        .opt_bytes(op.field_name.as_deref().map(str::as_bytes))
        .opt_int(op.reverts_op_id);
    out.0
}

fn dec_op(id: i64, b: &[u8]) -> Result<OpRow> {
    let mut r = In(b);
    Ok(OpRow {
        id,
        label_id: r.int()?,
        timestamp: r.int()?,
        origin: r.opt_string()?,
        op_type: r.string()?,
        entity_uuid: uuid_of(r.bytes()?),
        entity_version_before: r.opt_int()?.map(|v| v as u64),
        entity_version_after: r.opt_int()?.map(|v| v as u64),
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
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: a valid NUL-terminated path and a zeroed out-parameter.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some((st.f_bavail as u128 * st.f_frsize as u128).min(usize::MAX as u128) as usize)
}

/// `n` rounded up to the system's page size, as LMDB requires of a map size.
fn page_multiple(n: usize) -> usize {
    // SAFETY: sysconf has no preconditions.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as usize;
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
            use std::os::fd::AsRawFd;
            // An `flock` belongs to the open file description, which a `fork`
            // shares with the child until its `exec` closes it: a process
            // that spawns anything holds its own lock from another
            // descriptor for that instant, and a store closed then reopened
            // can find it taken. Such a holder lets go within milliseconds;
            // a second daemon does not, and is refused after the wait.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
            loop {
                // SAFETY: flock on a descriptor this function owns.
                let rc = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
                if rc == 0 {
                    break;
                }
                if std::time::Instant::now() > deadline {
                    bail!("the repository is already open in another daemon ({})", dir.display());
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        // SAFETY: the lock above makes this process the file's only opener;
        // the map is read-only (no WRITEMAP), so a stray write in the process
        // cannot reach it (doc "Storage safety").
        let env = unsafe {
            let held = std::fs::metadata(dir.join("data.mdb")).map_or(0, |m| m.len() as usize);
            let map_size = page_multiple(map_size.max(held.saturating_mul(2)));
            let mut options = EnvOpenOptions::new().read_txn_without_tls();
            options.map_size(map_size).max_dbs(32);
            if !durable {
                // SAFETY: NO_SYNC trades durability for speed, which is the
                // caller's stated choice (`open_unsynced`).
                options.flags(heed::EnvFlags::NO_SYNC);
            }
            options.open(dir)
        }
        .with_context(|| format!("open the key-value store in {}", dir.display()))?;
        // In `Tables`' order. heed hands a database out for good (`'static`)
        // only once the transaction that created it has committed.
        const NAMES: [&str; 23] = [
            "meta",
            "metarecords",
            "field_cells",
            "row_owner",
            "by_field",
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
        ];
        let w = env.write_txn()?;
        let created = env.create_databases_and_commit(w, |dbo, w| -> heed::Result<_> {
            let mut dbs = Vec::with_capacity(NAMES.len());
            for name in NAMES {
                dbs.push(dbo.create_database::<Bytes, Bytes>(w, Some(name))?);
            }
            let dbs: [_; 23] = dbs.try_into().expect("one database per name");
            Ok(CommitOrAbort::<_, ()>::Commit(dbs))
        })?;
        let [meta, metarecords, cells, row_owner, by_field, field_types, forest, ops, snaps, ops_by_label, ops_by_entity, labels, label_kids, label_keys, reverted_by, watcher_ops, user_ops, ids, uuids, sets, parts, kids, grams] =
            created.commit().expect("the databases are committed");
        let t = Tables {
            meta,
            metarecords,
            cells,
            row_owner,
            by_field,
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
        };
        let (derived, layout) = {
            let r = env.read_txn()?;
            (t.meta.get(&r, b"derived")?.map(from_be), t.meta.get(&r, b"layout")?.map(from_be))
        };
        let mut store = KvStore {
            env,
            t,
            _lock: lock,
            dir: dir.to_path_buf(),
            reads: Default::default(),
            resident: Default::default(),
        };
        if layout != Some(LAYOUT) {
            store.migrate_layout().context("migrate the key-value store's rows")?;
        }
        // A store from before the derived key spaces (or of another format of
        // them) gets them derived now: their migration.
        if derived != Some(derived::DERIVED_VERSION) {
            store.reindex().context("derive the key-value store's indexes")?;
        }
        Ok(store)
    }
}

// ── Reading (shared by the store and its write transactions) ──────────────────

struct Read<'a> {
    t: &'a Tables,
    r: &'a RoTxn<'a>,
    /// The store's read counter; `None` where nothing counts.
    reads: Option<&'a std::sync::atomic::AtomicU64>,
}

impl Read<'_> {
    fn meta(&self, name: &str) -> Result<Option<i64>> {
        Ok(self.t.meta.get(self.r, name.as_bytes())?.map(from_be))
    }

    fn version(&self, uuid: Uuid) -> Result<Option<u64>> {
        self.count(1)?;
        Ok(self.t.metarecords.get(self.r, uuid.as_bytes())?.map(|b| from_be(b) as u64))
    }

    /// A record's rows, in row-id order (they are kept in field order).
    fn rows(&self, uuid: Uuid) -> Result<Vec<FieldRow>> {
        let mut out = Vec::new();
        for e in self.t.cells.prefix_iter(self.r, uuid.as_bytes())? {
            let (_, v) = e?;
            out.push(dec_row(v)?);
        }
        self.count(out.len() as u64 + 1)?;
        out.sort_unstable_by_key(|r| r.id);
        Ok(out)
    }

    /// A record's rows of one field, in row-id order: one prefix.
    fn rows_named(&self, uuid: Uuid, name: &str) -> Result<Vec<FieldRow>> {
        let mut out = Vec::new();
        for e in self.t.cells.prefix_iter(self.r, &cell_prefix(uuid.as_bytes(), name))? {
            let (_, v) = e?;
            out.push(dec_row(v)?);
        }
        self.count(out.len() as u64 + 1)?;
        Ok(out)
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

    fn row(&self, id: i64) -> Result<Option<(Uuid, FieldRow)>> {
        // The owner, and the cell.
        self.count(2)?;
        let Some(prefix) = self.t.row_owner.get(self.r, &be(id))? else { return Ok(None) };
        let cell = self.t.cells.get(self.r, &key(&[prefix, &be(id)]))?;
        Ok(match cell {
            Some(v) => Some((uuid_of(prefix), dec_row(v)?)),
            None => None,
        })
    }

    fn field_rows(&self, name: &str) -> Result<Vec<(Uuid, FieldRow)>> {
        let mut out = Vec::new();
        for e in self.t.by_field.prefix_iter(self.r, &name_key(name))? {
            // The index entry and the cell it names.
            self.count(2)?;
            let (k, owner) = e?;
            let id = from_be(&k[k.len() - 8..]);
            let cell = self.t.cells.get(self.r, &key(&[&cell_prefix(owner, name), &be(id)]))?;
            out.push((uuid_of(owner), dec_row(cell.context("a row the field index names")?)?));
        }
        Ok(out)
    }

    fn children(&self, field: &str, parent: &[u8; 16]) -> Result<Vec<(Uuid, Vec<u8>, i64)>> {
        let prefix = key(&[&name_key(field), parent]);
        let mut out = Vec::new();
        for e in self.t.forest.prefix_iter(self.r, &prefix)? {
            let (k, v) = e?;
            // Counted as read, not after: a folder can hold the repository,
            // and a query stopped meanwhile stops here (`crate::interrupt`).
            self.count(1)?;
            out.push((
                uuid_of(v),
                self.node_name(field, &k[prefix.len()..], v)?,
                from_be(&v[16..]),
            ));
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
            out.push((uuid_of(v), self.node_name(field, &k[prefix.len()..], v)?));
            if out.len() >= limit {
                break;
            }
        }
        self.count(out.len() as u64 + 1)?;
        Ok(out)
    }

    /// A position's name, from the end of its forest key — or, for a name
    /// too long to be keyed whole, from its row (`v` is uuid · row id).
    fn node_name(&self, field: &str, key_end: &[u8], v: &[u8]) -> Result<Vec<u8>> {
        if key_end.len() <= NODE_MAX {
            return Ok(key_end.to_vec());
        }
        let cell = key(&[&cell_prefix(&v[..16], field), &v[16..24]]);
        self.count(1)?;
        let row = self.t.cells.get(self.r, &cell)?.context("a forest position without its row")?;
        match dec_row(row)?.value.position() {
            Some((_, name)) => Ok(name.as_bytes().to_vec()),
            None => bail!("a forest position whose row is no position"),
        }
    }

    fn op(&self, id: i64) -> Result<Option<OpRow>> {
        self.count(1)?;
        let Some(b) = self.t.ops.get(self.r, &be(id))? else { return Ok(None) };
        Ok(Some(dec_op(id, b)?))
    }

    fn ops_of(&self, ids: impl Iterator<Item = i64>) -> Result<Vec<OpRow>> {
        let mut out = Vec::new();
        for id in ids {
            self.count(1)?;
            let b = self
                .t
                .ops
                .get(self.r, &be(id))?
                .with_context(|| format!("operation {id} vanished"))?;
            out.push(dec_op(id, b)?);
        }
        Ok(out)
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
            out.push(dec_op(from_be(k), v)?);
        }
        Ok(out)
    }

    fn all_op_ids(&self) -> Result<Vec<i64>> {
        let mut out = Vec::new();
        for e in self.t.ops.iter(self.r)? {
            out.push(from_be(e?.0));
        }
        self.count(out.len() as u64 + 1)?;
        Ok(out)
    }

    fn label_op_ids(&self, label: i64) -> Result<Vec<i64>> {
        let mut out = Vec::new();
        for e in self.t.ops_by_label.prefix_iter(self.r, &be(label))? {
            let (k, _) = e?;
            out.push(from_be(&k[8..]));
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
        let mut lo = from_be(first);
        let Some((last, v)) = self.t.ops.last(self.r)? else { return Ok(None) };
        let mut hi = from_be(last);
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
                lo = id;
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
            fn version(&self, uuid: Uuid) -> Result<Option<u64>> {
                let $me = self;
                $with(&mut |$read: &Read| $read.version(uuid))
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
            fn versions_for(&self, uuids: &[Uuid]) -> Result<HashMap<Uuid, u64>> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    let mut out = HashMap::new();
                    for &u in uuids {
                        if let Some(v) = $read.version(u)? {
                            out.insert(u, v);
                        }
                    }
                    Ok(out)
                })
            }
            fn metarecord_count(&self) -> Result<usize> {
                let $me = self;
                $with(&mut |$read: &Read| Ok($read.t.metarecords.len($read.r)? as usize))
            }
            fn row(&self, id: i64) -> Result<Option<FieldRow>> {
                let $me = self;
                $with(&mut |$read: &Read| Ok($read.row(id)?.map(|(_, r)| r)))
            }
            fn owner_of_row(&self, id: i64) -> Result<Option<Uuid>> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    $read.count(1)?;
                    Ok($read.t.row_owner.get($read.r, &be(id))?.map(uuid_of))
                })
            }
            fn metarecords(&self) -> Result<Vec<Uuid>> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    let mut out = Vec::new();
                    for e in $read.t.metarecords.iter($read.r)? {
                        $read.count(1)?;
                        out.push(uuid_of(e?.0));
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
                    for e in $read.t.row_owner.iter($read.r)? {
                        $read.count(2)?;
                        let (k, prefix) = e?;
                        let cell = $read.t.cells.get($read.r, &key(&[prefix, k]))?;
                        f(
                            uuid_of(prefix),
                            dec_row(cell.context("a row its owner does not hold")?)?,
                        )?;
                    }
                    Ok(())
                })
            }
            fn max_row_id(&self) -> Result<i64> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    Ok($read.t.row_owner.last($read.r)?.map_or(0, |(k, _)| from_be(k)))
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
                $with(&mut |$read: &Read| {
                    let mut seen = HashSet::new();
                    let mut out = Vec::new();
                    for e in $read.t.by_field.prefix_iter($read.r, &name_key(name))? {
                        $read.count(1)?;
                        let u = uuid_of(e?.1);
                        if seen.insert(u) {
                            out.push(u);
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
                        .map(|(u, n, _)| (u, TreeName::from_bytes(n).display().into_owned()))
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
                    Ok($read.children(field, &p)?.into_iter().find_map(|(u, n, _)| {
                        let text = TreeName::from_bytes(n).display().into_owned();
                        // The fold `tree_cache::normalize_name` keys the rule
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
                            id: from_be(&v[16..]),
                            field_name: field,
                            uuid: uuid_of(v),
                            parent: (!parent.is_nil()).then_some(parent),
                            name: TreeName::from_bytes(name),
                        });
                    }
                    out.sort_by(|a, b| {
                        (&a.field_name, a.uuid, a.id).cmp(&(&b.field_name, b.uuid, b.id))
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
                $with(&mut |$read: &Read| {
                    let mut out = Vec::new();
                    let prefix = key(&[&be(op_id), &[after as u8]]);
                    for e in $read.t.snaps.prefix_iter($read.r, &prefix)? {
                        out.push(dec_row(e?.1)?);
                    }
                    $read.count(out.len() as u64 + 1)?;
                    Ok(out)
                })
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
                    Ok(($read.t.ops.len($read.r)? as i64, $read.t.labels.len($read.r)? as i64))
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
                    let mut ids = Vec::new();
                    for e in $read.t.ops_by_entity.prefix_iter($read.r, entity.as_bytes())? {
                        $read.count(1)?;
                        let id = from_be(&e?.0[16..]);
                        if id > after {
                            ids.push(id);
                        }
                    }
                    $read.ops_of(ids.into_iter())
                })
            }
            fn version_before_label(&self, label: i64, entity: Uuid) -> Result<Option<u64>> {
                Ok(self
                    .label_ops(label)?
                    .into_iter()
                    .find(|o| o.entity_uuid == entity)
                    .and_then(|o| o.entity_version_before))
            }
            fn ops_after(&self, op: i64) -> Result<Vec<OpRow>> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    let mut ids = Vec::new();
                    let from = be(op.saturating_add(1));
                    let range = (std::ops::Bound::Included(&from[..]), std::ops::Bound::Unbounded);
                    for e in $read.t.ops.range($read.r, &range)? {
                        ids.push(from_be(e?.0));
                    }
                    $read.count(ids.len() as u64 + 1)?;
                    $read.ops_of(ids.into_iter())
                })
            }
            fn ops_after_count(&self, op: i64, cap: i64) -> Result<i64> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    let from = be(op.saturating_add(1));
                    let range = (std::ops::Bound::Included(&from[..]), std::ops::Bound::Unbounded);
                    let cap = usize::try_from(cap).unwrap_or(0);
                    let n = $read.t.ops.range($read.r, &range)?.take(cap).count();
                    $read.count(n as u64 + 1)?;
                    Ok(n as i64)
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
    f(&Read { t: &me.t, r: &r, reads: Some(&me.reads) })
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
    /// How many keys the store's reads and query sources have read.
    pub fn reads(&self) -> u64 {
        self.reads.load(std::sync::atomic::Ordering::Relaxed)
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
}

// ── Writing ─────────────────────────────────────────────────────────────────

/// A write transaction. Its reads see its own writes.
pub struct KvTxn<'e> {
    t: Tables,
    txn: RefCell<RwTxn<'e>>,
    /// The derived set chunks this transaction changed, written at commit.
    sets: RefCell<HashMap<Vec<u8>, roaring::RoaringBitmap>>,
    /// Which chunks of each bitmap (table tag · key without chunk) the cache
    /// holds: a bitmap is read back whole without scanning the cache.
    cached_chunks: RefCell<HashMap<Vec<u8>, std::collections::BTreeSet<u16>>>,
    /// The store's read counter: a write's reads count as much as a query's
    /// (`tests/log_cost.rs` holds the trim to it).
    reads: &'e std::sync::atomic::AtomicU64,
}

kv_reads!(KvTxn<'_>, |me, read| |f: &mut dyn FnMut(&Read) -> Result<_>| {
    let txn = me.txn.borrow();
    f(&Read { t: &me.t, r: &txn, reads: Some(me.reads) })
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
}

impl KvStore {
    /// Grows the map so the write transaction about to start has the room
    /// [`map_to_grant`] promises — the disk's free space, up to a cap: a
    /// write of any size commits unless the disk itself is full. Done here
    /// because LMDB allows a resize only while the process has no transaction
    /// open — which `&mut self` guarantees: every transaction borrows the
    /// store.
    fn grow_map(&mut self) -> Result<()> {
        let info = self.env.info();
        let used =
            (info.last_page_number + 1) * self.env.unnamed_database_stat().page_size as usize;
        // Unknown free space: at least as much room as the store holds.
        let free = free_space(&self.dir).unwrap_or(used.max(INITIAL_MAP));
        let resized = map_to_grant(used, info.map_size, free);
        if let Some(size) = resized {
            // SAFETY: no transaction is open in this process (see above), and
            // the repository lock keeps every other process out of the file.
            unsafe { self.env.resize(size) }.context("grow the key-value store's map")?;
        }
        self.follow_residency(resized.is_some());
        Ok(())
    }

    /// The bytes of the map in use — what is held in memory: the used pages,
    /// never past the file's end (reading there would fault).
    fn used_bytes(&self) -> usize {
        let used = (self.env.info().last_page_number + 1)
            * self.env.unnamed_database_stat().page_size as usize;
        let file = std::fs::metadata(self.dir.join("data.mdb")).map_or(used, |m| m.len() as usize);
        used.min(file)
    }

    /// Where the store's file is mapped in this process. LMDB does not say —
    /// its `me_mapaddr` is the address of a *fixed* map only, 0 otherwise —
    /// so it is found in `/proc/self/maps`: the mapping of `data.mdb` from
    /// its start. Linux only; `None` elsewhere.
    fn map_addr(&self) -> Option<usize> {
        use std::os::unix::fs::MetadataExt;
        let inode = std::fs::metadata(self.dir.join("data.mdb")).ok()?.ino();
        let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
        resident::mapping_of(&maps, inode)
    }

    /// Reads the store into memory and locks it there when the memory-lock
    /// limit allows (doc "Holding a repository in memory"); `progress(done,
    /// total)` in bytes. Asked again, it says what is held. The store must
    /// stay borrowed meanwhile: a resize would unmap what is being read.
    pub fn make_resident(&self, progress: &dyn Fn(u64, u64)) -> Result<resident::Residency> {
        let mut slot = self.resident.0.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            let addr = self.map_addr().context(
                "the store's map is not found in /proc/self/maps: holding it in memory needs Linux",
            )?;
            *slot = Some(resident::Resident::take(
                &resident::Kernel,
                addr,
                self.used_bytes(),
                progress,
            ));
        }
        Ok(slot.as_ref().expect("taken above").residency())
    }

    /// Lets go of what [`Self::make_resident`] held: unlocked, and no longer
    /// followed by the writes. Nothing held, nothing done.
    pub fn release_resident(&self) {
        let taken = self.resident.0.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(resident) = taken {
            resident.release(&resident::Kernel);
        }
    }

    /// What is held in memory, `None` when nothing was asked.
    pub fn residency(&self) -> Option<resident::Residency> {
        self.resident.get()
    }

    /// The handle `GET /repos` reads the residency through.
    pub fn residency_handle(&self) -> ResidencyHandle {
        self.resident.clone()
    }

    /// Takes in what the last writes added to the map, or all of it again
    /// when a resize moved it. Run as a write starts — after the resize, with
    /// no transaction open — so the pages a commit appended are locked by
    /// the next one (until then they are the page cache's freshest).
    fn follow_residency(&self, resized: bool) {
        let mut slot = self.resident.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(resident) = slot.as_mut() {
            // The address moves only with a resize: `/proc/self/maps` is read
            // then, not at every write.
            let addr = if resized { self.map_addr() } else { Some(resident.addr()) };
            match addr {
                Some(addr) => resident.follow(&resident::Kernel, addr, self.used_bytes()),
                // Mapped again where it cannot be found: whatever was locked
                // went with the old mapping.
                None => resident.lose("the store's map, moved by a resize, is not found"),
            }
        }
    }
}

impl KvStore {
    /// Brings a store of an older layout to [`LAYOUT`], in one transaction:
    ///
    /// * the first layout's rows (`cells`, keyed `uuid · row id`) move into
    ///   `field_cells`, keyed by field, and `row_owner` points at their new
    ///   prefixes — a batch of rows in memory at a time;
    /// * the log of an older layout is **dropped, not converted** — the
    ///   history tree (layouts 1 and 2: operations chained to a parent,
    ///   `op_children`, `restorations`), and the sequence before it was split
    ///   by origin (3), the one grouped by writes (4), and the one with no
    ///   index of its reverts (5). The log is emptied, the metarecords kept (user
    ///   decision, October 2026; doc "The operation log").
    ///
    /// A new store has nothing to move and only records the layout.
    fn migrate_layout(&mut self) -> Result<()> {
        const BATCH: usize = 10_000;
        // The tables of older layouts, opened (if there) before the write:
        // heed opens a database in a transaction of its own.
        const GONE: [&str; 4] = ["op_children", "restorations", "ops_by_rev", "revisions"];
        let r = self.env.read_txn()?;
        let (old_cells, gone) = self
            .env
            .open_databases_and_commit(r, |dbo, r| -> heed::Result<_> {
                let cells = dbo.open_database::<Bytes, Bytes>(r, Some("cells"))?;
                let mut gone = Vec::new();
                for name in GONE {
                    gone.push(dbo.open_database::<Bytes, Bytes>(r, Some(name))?);
                }
                let gone: [_; 4] = gone.try_into().expect("one per name");
                Ok(CommitOrAbort::<_, ()>::Commit((cells, gone)))
            })?
            .commit()
            .expect("the databases are committed");
        let txn = KvTxn::new(self)?;
        {
            let mut w = txn.txn.borrow_mut();
            let t = txn.t;
            if let Some(old) = old_cells {
                let mut after: Option<Vec<u8>> = None;
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
                        let (uuid, id) = (&k[..16], &k[16..24]);
                        let prefix = cell_prefix(uuid, &dec_row(v)?.name);
                        t.cells.put(&mut w, &key(&[&prefix, id]), v)?;
                        t.row_owner.put(&mut w, id, &prefix)?;
                    }
                }
                old.clear(&mut w)?;
            }
            for old in gone.into_iter().flatten() {
                old.clear(&mut w)?;
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
        txn.meta_put("layout", LAYOUT)?;
        txn.finish()
    }
}

impl<'e> KvTxn<'e> {
    fn new(store: &'e mut KvStore) -> Result<KvTxn<'e>> {
        store.grow_map()?;
        let store: &'e KvStore = store;
        Ok(KvTxn {
            t: store.t,
            txn: RefCell::new(store.env.write_txn()?),
            sets: RefCell::new(HashMap::new()),
            cached_chunks: RefCell::new(HashMap::new()),
            reads: &store.reads,
        })
    }

    fn meta_put(&self, name: &str, n: i64) -> Result<()> {
        self.t.meta.put(&mut self.txn.borrow_mut(), name.as_bytes(), &be(n))?;
        Ok(())
    }

    /// Writes what the transaction still holds in memory, and commits.
    fn finish(self) -> Result<()> {
        self.flush_sets()?;
        self.txn.into_inner().commit().context("Failed to commit write transaction")?;
        // Every write commits here: an operation that never did is a read,
        // held to its own threshold in the slow-operation log.
        metafolder_core::slowlog::mark_write();
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

    fn bump_past(&self, counter: &str, id: i64) -> Result<()> {
        let mut w = self.txn.borrow_mut();
        let n = self.t.meta.get(&w, counter.as_bytes())?.map_or(1, from_be);
        if id >= n {
            self.t.meta.put(&mut w, counter.as_bytes(), &be(id + 1))?;
        }
        Ok(())
    }

    fn count_type(&self, name: &str, value: &Value, delta: i64) -> Result<()> {
        if matches!(value, Value::Nothing) {
            return Ok(());
        }
        let k = key(&[&name_key(name), rows::encode_value(value).value_type.as_bytes()]);
        let mut w = self.txn.borrow_mut();
        let n = self.t.field_types.get(&w, &k)?.map_or(0, from_be) + delta;
        if n <= 0 {
            self.t.field_types.delete(&mut w, &k)?;
        } else {
            self.t.field_types.put(&mut w, &k, &be(n))?;
        }
        Ok(())
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
                let read = Read { t: &self.t, r: &txn, reads: Some(self.reads) };
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
    fn create_metarecord(&self, uuid: Uuid, version: u64) -> Result<()> {
        let mut w = self.txn.borrow_mut();
        if self.t.metarecords.get(&w, uuid.as_bytes())?.is_some() {
            bail!("metarecord {uuid} already exists");
        }
        self.t.metarecords.put(&mut w, uuid.as_bytes(), &be(version as i64))?;
        drop(w);
        self.derive_created(uuid)
    }

    fn remove_metarecord(&self, uuid: Uuid) -> Result<()> {
        self.delete_rows(uuid, None)?;
        self.t.metarecords.delete(&mut self.txn.borrow_mut(), uuid.as_bytes())?;
        self.derive_removed(uuid)
    }

    fn set_version(&self, uuid: Uuid, version: u64) -> Result<()> {
        let mut w = self.txn.borrow_mut();
        if self.t.metarecords.get(&w, uuid.as_bytes())?.is_some() {
            self.t.metarecords.put(&mut w, uuid.as_bytes(), &be(version as i64))?;
        }
        Ok(())
    }

    fn insert_row(&self, uuid: Uuid, name: &str, value: &Value, id: Option<i64>) -> Result<i64> {
        if Rows::version(self, uuid)?.is_none() {
            bail!("a row for metarecord {uuid}, which does not exist");
        }
        // The two uniqueness rules of the data model, with the messages
        // clients already know.
        if name == "mfr_path" && !Rows::rows_named(self, uuid, name)?.is_empty() {
            return Err(DomainError::BadRequest(
                "mfr_path is single-valued: a metarecord tracks at most one path".into(),
            )
            .into());
        }
        let forest = Self::forest_key(name, value);
        if let Some(k) = &forest {
            if self.t.forest.get(&self.txn.borrow(), k)?.is_some() {
                return Err(DomainError::BadRequest(format!(
                    "tree position already occupied for field '{name}'"
                ))
                .into());
            }
        }
        let id = match id {
            Some(id) => {
                if self.t.row_owner.get(&self.txn.borrow(), &be(id))?.is_some() {
                    bail!("row {id} already exists");
                }
                self.bump_past("next_row", id)?;
                id
            }
            None => self.next("next_row")?,
        };
        let row = FieldRow { id, name: name.to_string(), value: value.clone() };
        {
            let mut w = self.txn.borrow_mut();
            let t = self.t;
            let prefix = cell_prefix(uuid.as_bytes(), name);
            t.cells.put(&mut w, &key(&[&prefix, &be(id)]), &enc_row(&row))?;
            t.row_owner.put(&mut w, &be(id), &prefix)?;
            t.by_field.put(&mut w, &key(&[&name_key(name), &be(id)]), uuid.as_bytes())?;
            if let Some(k) = &forest {
                t.forest.put(&mut w, k, &key(&[uuid.as_bytes(), &be(id)]))?;
            }
        }
        self.count_type(name, value, 1)?;
        self.derive_row(uuid, name, value, 1)?;
        Ok(id)
    }

    fn delete_row(&self, id: i64) -> Result<()> {
        let Some((owner, row)) = ({
            let txn = self.txn.borrow();
            Read { t: &self.t, r: &txn, reads: Some(self.reads) }.row(id)?
        }) else {
            return Ok(());
        };
        {
            let mut w = self.txn.borrow_mut();
            let t = self.t;
            t.cells.delete(&mut w, &key(&[&cell_prefix(owner.as_bytes(), &row.name), &be(id)]))?;
            t.row_owner.delete(&mut w, &be(id))?;
            t.by_field.delete(&mut w, &key(&[&name_key(&row.name), &be(id)]))?;
            if let Some(k) = Self::forest_key(&row.name, &row.value) {
                t.forest.delete(&mut w, &k)?;
            }
        }
        self.count_type(&row.name, &row.value, -1)?;
        self.derive_row(owner, &row.name, &row.value, -1)
    }

    fn delete_rows(&self, uuid: Uuid, name: Option<&str>) -> Result<()> {
        for row in Rows::rows(self, uuid)? {
            if name.is_none_or(|n| n == row.name) {
                self.delete_row(row.id)?;
            }
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
    fn trim(&self, retention: Retention, _head: i64) -> Result<usize> {
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
            for e in table.iter(&txn)?.take(cut) {
                doomed.push(from_be(e?.0));
            }
            count(cut);
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
        for db in [t.metarecords, t.cells, t.row_owner, t.by_field, t.field_types, t.forest] {
            db.clear(&mut w)?;
        }
        drop(w);
        self.clear_derived()
    }

    fn delete_ops(&self, ids: &[i64]) -> Result<usize> {
        let mut hit: HashSet<i64> = HashSet::new();
        for &id in ids {
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
mod resident_tests {
    use super::test_store;
    use crate::store::Begin;
    use metafolder_core::metarecord::Value;
    use uuid::Uuid;

    /// The bytes of the store in use: what is read into memory.
    fn used(store: &super::KvStore) -> u64 {
        ((store.env.info().last_page_number + 1)
            * store.env.unnamed_database_stat().page_size as usize) as u64
    }

    fn fill(store: &mut super::KvStore, rows: usize) {
        let w = store.begin_write().unwrap();
        for i in 0..rows {
            let uuid = Uuid::new_v4();
            w.create_metarecord(uuid, 0).unwrap();
            w.insert_row(uuid, "text", &Value::String(format!("{i:0>200}")), None).unwrap();
        }
        w.commit().unwrap();
    }

    #[test]
    fn test_a_store_is_held_in_memory_only_when_asked() {
        let (store, _dir) = test_store();
        assert_eq!(store.residency(), None);
        let calls = std::cell::Cell::new(0);
        let residency = store.make_resident(&|_, _| calls.set(calls.get() + 1)).unwrap();
        assert_eq!(residency.bytes, used(&store));
        assert!(calls.get() >= 1, "the load reports its progress");
        // A test store is far below any memory-lock limit worth the name.
        assert!(residency.pinned, "{:?}", residency.reason);
        assert_eq!(store.residency(), Some(residency));
    }

    #[test]
    fn test_what_a_write_adds_is_held_too() {
        let (mut store, _dir) = test_store();
        store.make_resident(&|_, _| {}).unwrap();
        let before = store.residency().unwrap().bytes;
        fill(&mut store, 5000);
        // Followed when the next write starts: the map may move then.
        drop(store.begin_write().unwrap());
        let after = store.residency().unwrap();
        assert!(after.bytes > before, "{} then {}", before, after.bytes);
        assert_eq!(after.bytes, used(&store));
    }

    #[test]
    fn test_a_store_held_in_memory_can_be_let_go_and_held_again() {
        let (mut store, _dir) = test_store();
        store.make_resident(&|_, _| {}).unwrap();
        store.release_resident();
        assert_eq!(store.residency(), None);
        // Let go, it no longer follows the writes.
        fill(&mut store, 10);
        drop(store.begin_write().unwrap());
        assert_eq!(store.residency(), None);
        let again = store.make_resident(&|_, _| {}).unwrap();
        assert_eq!(again.bytes, used(&store));
        // Letting go of a store not held is nothing.
        store.release_resident();
        store.release_resident();
        assert_eq!(store.residency(), None);
    }
}
