//! The key-value storage backend (docs/spec-storage.org, increment 3): the
//! `store` traits over LMDB (through `heed`). Primary data and the event log
//! only — the query index stays the resident one, built from `Rows` like on
//! SQLite; the derived key spaces of the spec are increment 4.
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
//! | cells         | uuid · row id                    | field name · value        |
//! | row_owner     | row id                           | uuid                      |
//! | by_field      | name · row id                    | uuid                      |
//! | field_types   | name · value type                | rows of that type         |
//! | forest        | field · parent · name bytes      | uuid · row id             |
//! | ops           | op id                            | the operation             |
//! | snaps         | op id · after? · index           | a snapshot row            |
//! | op_children   | parent op · child op             | —                         |
//! | ops_by_rev    | revision · seq · op id           | —                         |
//! | ops_by_entity | uuid · op id                     | —                         |
//! | revisions     | revision id                      | timestamp · label · origin|
//! | restorations  | position                         | a restoration             |
//!
//! Row ids, operation ids and revision ids are allocated from counters in
//! `meta` and never reused, as SQLite's AUTOINCREMENT guarantees; a row put
//! back under its own id moves the counter past it.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::Path;

use anyhow::{bail, Context, Result};
use heed::types::Bytes;
use heed::{Database, Env, EnvOpenOptions, RoTxn, RwTxn, WithoutTls};
use metafolder_core::metarecord::{TreeName, Value};
use uuid::Uuid;

use crate::db::{self, FieldRow, RawValue, TreeRow};
use crate::error::DomainError;
use crate::log::{self, Delta, OpRow, Retention};
use crate::store::{Begin, Log, NewOp, Questions, Restoration, RevisionMeta, Rows, WriteTxn};

type Db = Database<Bytes, Bytes>;

/// The parent key of a forest root (the SQLite schema's zero-uuid sentinel).
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
    op_children: Db,
    ops_by_rev: Db,
    ops_by_entity: Db,
    revisions: Db,
    restorations: Db,
}

/// A repository database on LMDB.
pub struct KvStore {
    env: Env<WithoutTls>,
    t: Tables,
    /// Held for the store's lifetime: one daemon per repository.
    _lock: File,
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

/// A value, column by column as SQLite stores it (the same `encode_value` /
/// `decode_value` pair), so a name's exact bytes survive.
fn put_value(out: &mut Out, v: &Value) {
    let e = db::encode_value(v);
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
    db::decode_value(raw)
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

fn enc_op(op: &NewOp, parent: Option<i64>, rev: i64, seq: i64) -> Vec<u8> {
    let mut out = Out::default();
    out.opt_int(parent)
        .int(rev)
        .int(seq)
        .bytes(op.op_type.as_str().as_bytes())
        .bytes(op.entity.as_bytes())
        .opt_int(op.version_before.map(|v| v as i64))
        .opt_int(op.version_after.map(|v| v as i64))
        .opt_bytes(op.field_name.as_deref().map(str::as_bytes))
        .opt_int(op.reverts_op_id);
    out.0
}

/// An operation, without its revision's origin (the caller adds it).
fn dec_op(id: i64, b: &[u8]) -> Result<OpRow> {
    let mut r = In(b);
    Ok(OpRow {
        id,
        parent_id: r.opt_int()?,
        rev_id: r.int()?,
        seq: r.int()?,
        op_type: r.string()?,
        entity_uuid: uuid_of(r.bytes()?),
        entity_version_before: r.opt_int()?.map(|v| v as u64),
        entity_version_after: r.opt_int()?.map(|v| v as u64),
        field_name: r.opt_string()?,
        reverts_op_id: r.opt_int()?,
        origin: None,
    })
}

fn enc_revision(m: &RevisionMeta) -> Vec<u8> {
    let mut out = Out::default();
    out.int(m.timestamp)
        .opt_bytes(m.label.as_deref().map(str::as_bytes))
        .opt_bytes(m.origin.as_deref().map(str::as_bytes));
    out.0
}

fn dec_revision(b: &[u8]) -> Result<RevisionMeta> {
    let mut r = In(b);
    Ok(RevisionMeta { timestamp: r.int()?, label: r.opt_string()?, origin: r.opt_string()? })
}

fn enc_restoration(x: &Restoration) -> Vec<u8> {
    let mut out = Out::default();
    match x {
        Restoration::SetPath { entity, parent, name } => {
            out.0.push(0);
            out.bytes(entity.as_bytes())
                .opt_bytes(parent.as_ref().map(|p| p.as_bytes().as_slice()))
                .bytes(name.as_bytes());
        }
        Restoration::ClearPath { entity } => {
            out.0.push(1);
            out.bytes(entity.as_bytes());
        }
        Restoration::ClearHashes { entity } => {
            out.0.push(2);
            out.bytes(entity.as_bytes());
        }
    }
    out.0
}

fn dec_restoration(b: &[u8]) -> Result<Restoration> {
    let mut r = In(b);
    let tag = r.take(1)?[0];
    let entity = uuid_of(r.bytes()?);
    Ok(match tag {
        0 => Restoration::SetPath {
            entity,
            parent: r.opt_bytes()?.map(uuid_of),
            name: TreeName::from_bytes(r.bytes()?.to_vec()),
        },
        1 => Restoration::ClearPath { entity },
        _ => Restoration::ClearHashes { entity },
    })
}

// ── Opening ─────────────────────────────────────────────────────────────────

impl KvStore {
    /// Opens (creating it if needed) the store in `dir`, and takes the
    /// repository's lock: a second daemon opening it fails, as a second
    /// daemon opening a SQLite repository does.
    pub fn open(dir: &Path) -> Result<KvStore> {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let lock = File::create(dir.join("daemon.lock"))?;
        {
            use std::os::fd::AsRawFd;
            // SAFETY: flock on a descriptor this function owns.
            let rc = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc != 0 {
                bail!("the repository is already open in another daemon ({})", dir.display());
            }
        }
        // SAFETY: the lock above makes this process the file's only opener;
        // the map is read-only (no WRITEMAP), so a stray write in the process
        // cannot reach it (docs/spec-storage.org "Safety").
        let env = unsafe {
            EnvOpenOptions::new().read_txn_without_tls().map_size(1 << 40).max_dbs(16).open(dir)
        }
        .with_context(|| format!("open the key-value store in {}", dir.display()))?;
        let mut w = env.write_txn()?;
        let mut db = |name| env.create_database::<Bytes, Bytes>(&mut w, Some(name));
        let t = Tables {
            meta: db("meta")?,
            metarecords: db("metarecords")?,
            cells: db("cells")?,
            row_owner: db("row_owner")?,
            by_field: db("by_field")?,
            field_types: db("field_types")?,
            forest: db("forest")?,
            ops: db("ops")?,
            snaps: db("snaps")?,
            op_children: db("op_children")?,
            ops_by_rev: db("ops_by_rev")?,
            ops_by_entity: db("ops_by_entity")?,
            revisions: db("revisions")?,
            restorations: db("restorations")?,
        };
        w.commit()?;
        Ok(KvStore { env, t, _lock: lock })
    }
}

// ── Reading (shared by the store and its write transactions) ──────────────────

struct Read<'a> {
    t: &'a Tables,
    r: &'a RoTxn<'a>,
}

impl Read<'_> {
    fn meta(&self, name: &str) -> Result<Option<i64>> {
        Ok(self.t.meta.get(self.r, name.as_bytes())?.map(from_be))
    }

    fn version(&self, uuid: Uuid) -> Result<Option<u64>> {
        Ok(self.t.metarecords.get(self.r, uuid.as_bytes())?.map(|b| from_be(b) as u64))
    }

    fn rows(&self, uuid: Uuid) -> Result<Vec<FieldRow>> {
        let mut out = Vec::new();
        for e in self.t.cells.prefix_iter(self.r, uuid.as_bytes())? {
            let (_, v) = e?;
            out.push(dec_row(v)?);
        }
        Ok(out)
    }

    fn row(&self, id: i64) -> Result<Option<(Uuid, FieldRow)>> {
        let Some(owner) = self.t.row_owner.get(self.r, &be(id))? else { return Ok(None) };
        let owner = uuid_of(owner);
        let cell = self.t.cells.get(self.r, &key(&[owner.as_bytes(), &be(id)]))?;
        Ok(match cell {
            Some(v) => Some((owner, dec_row(v)?)),
            None => None,
        })
    }

    fn field_rows(&self, name: &str) -> Result<Vec<(Uuid, FieldRow)>> {
        let mut out = Vec::new();
        for e in self.t.by_field.prefix_iter(self.r, &name_key(name))? {
            let (k, owner) = e?;
            let id = from_be(&k[k.len() - 8..]);
            let owner = uuid_of(owner);
            let cell = self.t.cells.get(self.r, &key(&[owner.as_bytes(), &be(id)]))?;
            out.push((owner, dec_row(cell.context("a row the field index names")?)?));
        }
        Ok(out)
    }

    fn children(&self, field: &str, parent: &[u8; 16]) -> Result<Vec<(Uuid, Vec<u8>, i64)>> {
        let prefix = key(&[&name_key(field), parent]);
        let mut out = Vec::new();
        for e in self.t.forest.prefix_iter(self.r, &prefix)? {
            let (k, v) = e?;
            out.push((uuid_of(v), k[prefix.len()..].to_vec(), from_be(&v[16..])));
        }
        Ok(out)
    }

    fn origin(&self, rev: i64, cache: &mut HashMap<i64, Option<String>>) -> Result<Option<String>> {
        if let Some(o) = cache.get(&rev) {
            return Ok(o.clone());
        }
        let o = match self.t.revisions.get(self.r, &be(rev))? {
            Some(b) => dec_revision(b)?.origin,
            None => None,
        };
        cache.insert(rev, o.clone());
        Ok(o)
    }

    fn op(&self, id: i64) -> Result<Option<OpRow>> {
        let Some(b) = self.t.ops.get(self.r, &be(id))? else { return Ok(None) };
        let mut op = dec_op(id, b)?;
        op.origin = self.origin(op.rev_id, &mut HashMap::new())?;
        Ok(Some(op))
    }

    fn ops_with_origins(&self, ids: impl Iterator<Item = i64>) -> Result<Vec<OpRow>> {
        let mut cache = HashMap::new();
        let mut out = Vec::new();
        for id in ids {
            let b = self
                .t
                .ops
                .get(self.r, &be(id))?
                .with_context(|| format!("operation {id} vanished"))?;
            let mut op = dec_op(id, b)?;
            op.origin = self.origin(op.rev_id, &mut cache)?;
            out.push(op);
        }
        Ok(out)
    }

    /// The ancestor chain from `from`, at most `max` operations, HEAD-first.
    fn chain(&self, from: i64, max: usize) -> Result<Vec<OpRow>> {
        let mut out = Vec::new();
        let mut cur = Some(from);
        let mut cache = HashMap::new();
        while let Some(id) = cur {
            if out.len() >= max {
                break;
            }
            let Some(b) = self.t.ops.get(self.r, &be(id))? else { break };
            let mut op = dec_op(id, b)?;
            op.origin = self.origin(op.rev_id, &mut cache)?;
            cur = op.parent_id;
            out.push(op);
        }
        Ok(out)
    }

    fn all_op_ids(&self) -> Result<Vec<i64>> {
        let mut out = Vec::new();
        for e in self.t.ops.iter(self.r)? {
            out.push(from_be(e?.0));
        }
        Ok(out)
    }

    fn revision_op_ids(&self, rev: i64) -> Result<Vec<i64>> {
        let mut out = Vec::new();
        for e in self.t.ops_by_rev.prefix_iter(self.r, &be(rev))? {
            let (k, _) = e?;
            out.push(from_be(&k[16..]));
        }
        Ok(out)
    }

    fn ancestor_where(
        &self,
        head: i64,
        keep: &dyn Fn(&RevisionMeta) -> bool,
    ) -> Result<Option<i64>> {
        let mut seen: HashMap<i64, bool> = HashMap::new();
        let mut cur = Some(head);
        while let Some(id) = cur {
            let Some(b) = self.t.ops.get(self.r, &be(id))? else { return Ok(None) };
            let op = dec_op(id, b)?;
            let ok = match seen.get(&op.rev_id) {
                Some(ok) => *ok,
                None => {
                    let ok = match self.t.revisions.get(self.r, &be(op.rev_id))? {
                        Some(b) => keep(&dec_revision(b)?),
                        None => false,
                    };
                    seen.insert(op.rev_id, ok);
                    ok
                }
            };
            if ok {
                return Ok(Some(id));
            }
            cur = op.parent_id;
        }
        Ok(None)
    }
}

/// `Rows`, `Log` and `Questions` for anything that can lend a `Read`.
macro_rules! kv_reads {
    ($ty:ty, |$me:ident, $read:ident| $with:expr) => {
        impl Rows for $ty {
            fn version(&self, uuid: Uuid) -> Result<Option<u64>> {
                let $me = self;
                $with(&mut |$read: &Read| $read.version(uuid))
            }
            fn rows(&self, uuid: Uuid) -> Result<Vec<FieldRow>> {
                let $me = self;
                $with(&mut |$read: &Read| $read.rows(uuid))
            }
            fn rows_named(&self, uuid: Uuid, name: &str) -> Result<Vec<FieldRow>> {
                Ok(self.rows(uuid)?.into_iter().filter(|r| r.name == name).collect())
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
                $with(&mut |$read: &Read| Ok($read.t.row_owner.get($read.r, &be(id))?.map(uuid_of)))
            }
            fn metarecords(&self) -> Result<Vec<Uuid>> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    let mut out = Vec::new();
                    for e in $read.t.metarecords.iter($read.r)? {
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
                        let (k, owner) = e?;
                        let owner = uuid_of(owner);
                        let cell = $read.t.cells.get($read.r, &key(&[owner.as_bytes(), k]))?;
                        f(owner, dec_row(cell.context("a row its owner does not hold")?)?)?;
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
                        let u = uuid_of(e?.1);
                        if seen.insert(u) {
                            out.push(u);
                        }
                    }
                    Ok(out)
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
                    let k = key(&[&name_key(field), &p, name]);
                    Ok($read.t.forest.get($read.r, &k)?.map(uuid_of))
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
                        let same =
                            if nocase { text.eq_ignore_ascii_case(name) } else { text == name };
                        same.then_some(u)
                    }))
                })
            }
            fn forest(&self) -> Result<Vec<TreeRow>> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    let mut out = Vec::new();
                    for e in $read.t.forest.iter($read.r)? {
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
                        let parent = uuid_of(&k[end + 2..end + 18]);
                        out.push(TreeRow {
                            id: from_be(&v[16..]),
                            field_name: String::from_utf8(field)?,
                            uuid: uuid_of(v),
                            parent: (!parent.is_nil()).then_some(parent),
                            name: TreeName::from_bytes(k[end + 18..].to_vec()),
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
                $with(&mut |$read: &Read| $read.meta("head"))
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
                    Ok(out)
                })
            }
            fn ops_until(&self, from: i64, until: i64, max: usize) -> Result<Delta> {
                if from == until {
                    return Ok(Delta::Found(Vec::new()));
                }
                let $me = self;
                // As SQLite's walk: up to `max + 1` operations, the anchor
                // closing it when met.
                $with(&mut |$read: &Read| {
                    let mut rows = Vec::new();
                    let mut cur = Some(from);
                    let mut cache = HashMap::new();
                    while let Some(id) = cur {
                        if rows.len() == max + 1 {
                            break;
                        }
                        let Some(b) = $read.t.ops.get($read.r, &be(id))? else { break };
                        let mut op = dec_op(id, b)?;
                        op.origin = $read.origin(op.rev_id, &mut cache)?;
                        cur = if id == until { None } else { op.parent_id };
                        rows.push(op);
                    }
                    Ok(match rows.last() {
                        Some(last) if last.id == until => {
                            rows.pop();
                            Delta::Found(rows)
                        }
                        _ if rows.len() > max => Delta::Budget,
                        _ => Delta::Unrelated,
                    })
                })
            }
            fn ancestry(&self, from: i64) -> Result<Vec<i64>> {
                Ok(self.ancestry_ops(from, None)?.into_iter().map(|o| o.id).collect())
            }
            fn restorations(&self) -> Result<Vec<(i64, Restoration)>> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    let mut out = Vec::new();
                    for e in $read.t.restorations.iter($read.r)? {
                        let (k, v) = e?;
                        out.push((from_be(k), dec_restoration(v)?));
                    }
                    Ok(out)
                })
            }
            fn ancestry_ops(&self, from: i64, max: Option<usize>) -> Result<Vec<OpRow>> {
                let $me = self;
                $with(&mut |$read: &Read| $read.chain(from, max.unwrap_or(usize::MAX)))
            }
            fn all_ops(&self) -> Result<Vec<OpRow>> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    let ids = $read.all_op_ids()?;
                    $read.ops_with_origins(ids.into_iter())
                })
            }
            fn active_line(&self, head: i64) -> Result<Vec<OpRow>> {
                Ok(log::active_line_of(self.ancestry_ops(head, None)?, self.all_ops()?, head))
            }
            fn has_children(&self, op: i64) -> Result<bool> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    Ok($read.t.op_children.prefix_iter($read.r, &be(op))?.next().is_some())
                })
            }
            fn revisions(&self, ids: &[i64]) -> Result<HashMap<i64, RevisionMeta>> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    let mut out = HashMap::new();
                    for &id in ids {
                        if let Some(b) = $read.t.revisions.get($read.r, &be(id))? {
                            out.insert(id, dec_revision(b)?);
                        }
                    }
                    Ok(out)
                })
            }
            fn counts(&self) -> Result<(i64, i64)> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    Ok(($read.t.ops.len($read.r)? as i64, $read.t.revisions.len($read.r)? as i64))
                })
            }
            fn revision_ops(&self, rev: i64) -> Result<Vec<OpRow>> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    let ids = $read.revision_op_ids(rev)?;
                    $read.ops_with_origins(ids.into_iter())
                })
            }
            fn entity_ops_after(&self, entity: Uuid, after: i64) -> Result<Vec<OpRow>> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    let mut ids = Vec::new();
                    for e in $read.t.ops_by_entity.prefix_iter($read.r, entity.as_bytes())? {
                        let id = from_be(&e?.0[16..]);
                        if id > after {
                            ids.push(id);
                        }
                    }
                    $read.ops_with_origins(ids.into_iter())
                })
            }
            fn version_before_revision(&self, rev: i64, entity: Uuid) -> Result<Option<u64>> {
                Ok(self
                    .revision_ops(rev)?
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
                    $read.ops_with_origins(ids.into_iter())
                })
            }
            fn ops_after_count(&self, op: i64) -> Result<i64> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    let from = be(op.saturating_add(1));
                    let range = (std::ops::Bound::Included(&from[..]), std::ops::Bound::Unbounded);
                    Ok($read.t.ops.range($read.r, &range)?.count() as i64)
                })
            }
            fn ancestor_at_or_before(&self, head: i64, timestamp_ms: i64) -> Result<Option<i64>> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    $read.ancestor_where(head, &|m: &RevisionMeta| m.timestamp <= timestamp_ms)
                })
            }
            fn ancestor_labelled(&self, head: i64, label: &str) -> Result<Option<i64>> {
                let $me = self;
                $with(&mut |$read: &Read| {
                    $read
                        .ancestor_where(head, &|m: &RevisionMeta| m.label.as_deref() == Some(label))
                })
            }
            fn before_revision_of(&self, op: i64) -> Result<Option<i64>> {
                let head = self.op(op)?.with_context(|| format!("operation {op} not found"))?;
                let first = self.revision_ops(head.rev_id)?.into_iter().next();
                Ok(first.context("a revision without operations")?.parent_id)
            }
        }

        impl Questions for $ty {}
    };
}

kv_reads!(KvStore, |me, read| |f: &mut dyn FnMut(&Read) -> Result<_>| {
    let r = me.env.read_txn()?;
    f(&Read { t: &me.t, r: &r })
});

// ── Writing ─────────────────────────────────────────────────────────────────

/// A write transaction. Its reads see its own writes.
pub struct KvTxn<'e> {
    t: Tables,
    txn: RefCell<RwTxn<'e>>,
}

kv_reads!(KvTxn<'_>, |me, read| |f: &mut dyn FnMut(&Read) -> Result<_>| {
    let txn = me.txn.borrow();
    f(&Read { t: &me.t, r: &txn })
});

impl Begin for KvStore {
    fn begin_write(&mut self) -> Result<Box<dyn WriteTxn + '_>> {
        Ok(Box::new(KvTxn { t: self.t, txn: RefCell::new(self.env.write_txn()?) }))
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
        let k = key(&[&name_key(name), db::encode_value(value).value_type.as_bytes()]);
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
        let Value::TreeRef { parent, name: node } = value else { return None };
        let p = parent.map_or(ROOT, |p| *p.as_bytes());
        Some(key(&[&name_key(name), &p, node.as_bytes()]))
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
        if let Some(p) = op.parent_id {
            t.op_children.delete(&mut w, &key(&[&be(p), &be(id)]))?;
        }
        t.ops_by_rev.delete(&mut w, &key(&[&be(op.rev_id), &be(op.seq), &be(id)]))?;
        t.ops_by_entity.delete(&mut w, &key(&[op.entity_uuid.as_bytes(), &be(id)]))?;
        Ok(())
    }
}

impl WriteTxn for KvTxn<'_> {
    fn create_metarecord(&self, uuid: Uuid, version: u64) -> Result<()> {
        let mut w = self.txn.borrow_mut();
        if self.t.metarecords.get(&w, uuid.as_bytes())?.is_some() {
            bail!("metarecord {uuid} already exists");
        }
        self.t.metarecords.put(&mut w, uuid.as_bytes(), &be(version as i64))?;
        Ok(())
    }

    fn remove_metarecord(&self, uuid: Uuid) -> Result<()> {
        self.delete_rows(uuid, None)?;
        self.t.metarecords.delete(&mut self.txn.borrow_mut(), uuid.as_bytes())?;
        Ok(())
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
        // The two uniqueness rules SQLite's indexes enforce, with its messages.
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
            t.cells.put(&mut w, &key(&[uuid.as_bytes(), &be(id)]), &enc_row(&row))?;
            t.row_owner.put(&mut w, &be(id), uuid.as_bytes())?;
            t.by_field.put(&mut w, &key(&[&name_key(name), &be(id)]), uuid.as_bytes())?;
            if let Some(k) = &forest {
                t.forest.put(&mut w, k, &key(&[uuid.as_bytes(), &be(id)]))?;
            }
        }
        self.count_type(name, value, 1)?;
        Ok(id)
    }

    fn delete_row(&self, id: i64) -> Result<()> {
        let Some((owner, row)) = ({
            let txn = self.txn.borrow();
            Read { t: &self.t, r: &txn }.row(id)?
        }) else {
            return Ok(());
        };
        {
            let mut w = self.txn.borrow_mut();
            let t = self.t;
            t.cells.delete(&mut w, &key(&[owner.as_bytes(), &be(id)]))?;
            t.row_owner.delete(&mut w, &be(id))?;
            t.by_field.delete(&mut w, &key(&[&name_key(&row.name), &be(id)]))?;
            if let Some(k) = Self::forest_key(&row.name, &row.value) {
                t.forest.delete(&mut w, &k)?;
            }
        }
        self.count_type(&row.name, &row.value, -1)
    }

    fn delete_rows(&self, uuid: Uuid, name: Option<&str>) -> Result<()> {
        for row in Rows::rows(self, uuid)? {
            if name.is_none_or(|n| n == row.name) {
                self.delete_row(row.id)?;
            }
        }
        Ok(())
    }

    fn begin_revision(&self, label: Option<&str>, timestamp_ms: i64) -> Result<i64> {
        let rev = self.next("next_rev")?;
        let meta = RevisionMeta {
            timestamp: timestamp_ms,
            label: label.map(str::to_string),
            origin: None,
        };
        self.t.revisions.put(&mut self.txn.borrow_mut(), &be(rev), &enc_revision(&meta))?;
        Ok(rev)
    }

    fn set_revision_origin(&self, rev: i64, origin: &str) -> Result<()> {
        // Like SQLite's UPDATE: nothing to do for a revision that is not there.
        let Some(mut meta) = Log::revisions(self, &[rev])?.remove(&rev) else { return Ok(()) };
        meta.origin = Some(origin.to_string());
        self.t.revisions.put(&mut self.txn.borrow_mut(), &be(rev), &enc_revision(&meta))?;
        Ok(())
    }

    fn set_revision_label(&self, rev: i64, label: Option<&str>) -> Result<bool> {
        let Some(mut meta) = Log::revisions(self, &[rev])?.remove(&rev) else { return Ok(false) };
        meta.label = label.map(str::to_string);
        self.t.revisions.put(&mut self.txn.borrow_mut(), &be(rev), &enc_revision(&meta))?;
        Ok(true)
    }

    fn drop_revision(&self, rev: i64) -> Result<()> {
        for id in {
            let txn = self.txn.borrow();
            Read { t: &self.t, r: &txn }.revision_op_ids(rev)?
        } {
            self.remove_op(id)?;
        }
        self.t.revisions.delete(&mut self.txn.borrow_mut(), &be(rev))?;
        Ok(())
    }

    fn append_ops(
        &self,
        rev: i64,
        parent: Option<i64>,
        first_seq: i64,
        ops: &[NewOp],
    ) -> Result<i64> {
        let base = {
            let w = self.txn.borrow();
            self.t.meta.get(&w, b"next_op")?.map_or(1, from_be)
        };
        let mut w = self.txn.borrow_mut();
        let t = self.t;
        for (i, op) in ops.iter().enumerate() {
            let id = base + i as i64;
            let parent = if i == 0 { parent } else { Some(id - 1) };
            let seq = first_seq + i as i64;
            t.ops.put(&mut w, &be(id), &enc_op(op, parent, rev, seq))?;
            for (after, rows) in [(0u8, &op.before), (1u8, &op.after)] {
                for (n, row) in rows.iter().enumerate() {
                    let k = key(&[&be(id), &[after], &(n as u32).to_be_bytes()]);
                    t.snaps.put(&mut w, &k, &enc_row(row))?;
                }
            }
            if let Some(p) = parent {
                t.op_children.put(&mut w, &key(&[&be(p), &be(id)]), &[])?;
            }
            t.ops_by_rev.put(&mut w, &key(&[&be(rev), &be(seq), &be(id)]), &[])?;
            t.ops_by_entity.put(&mut w, &key(&[op.entity.as_bytes(), &be(id)]), &[])?;
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

    /// SQLite's `log::trim`, over these tables: keep the `revisions` newest
    /// (and, when asked, everything from the oldest labelled one), cutting at
    /// the first operation of the oldest kept revision — only when that cut is
    /// on HEAD's line of history.
    fn trim(&self, retention: Retention, head: i64) -> Result<usize> {
        if !retention.enabled() {
            return Ok(0);
        }
        let revs: Vec<(i64, RevisionMeta)> = {
            let txn = self.txn.borrow();
            let mut out = Vec::new();
            for e in self.t.revisions.iter(&txn)? {
                let (k, v) = e?;
                out.push((from_be(k), dec_revision(v)?));
            }
            out
        };
        let kept = revs.len() as u64;
        if kept <= retention.revisions + Retention::slack(retention.revisions) {
            return Ok(0);
        }
        let mut keep_from = revs[revs.len() - retention.revisions as usize].0;
        if retention.keep_labels {
            if let Some((id, _)) = revs.iter().find(|(_, m)| m.label.is_some()) {
                keep_from = keep_from.min(*id);
            }
        }
        let first = {
            let txn = self.txn.borrow();
            Read { t: &self.t, r: &txn }.revision_op_ids(keep_from)?
        };
        let Some(cutoff) = first.into_iter().min() else { return Ok(0) };
        let all = {
            let txn = self.txn.borrow();
            Read { t: &self.t, r: &txn }.all_op_ids()?
        };
        if all.first() == Some(&cutoff) {
            return Ok(0);
        }
        if !Log::ancestry(self, head)?.contains(&cutoff) {
            // Not on HEAD's line: the newest revisions sit on a branch a
            // rollback abandoned; cutting would delete what HEAD stands on.
            return Ok(0);
        }
        self.detach_op(cutoff)?;
        // Older than the cutoff, or below something doomed: children always
        // have larger ids than their parents, so one ascending pass settles it.
        let mut doomed: HashSet<i64> = HashSet::new();
        let mut revs_hit: HashSet<i64> = HashSet::new();
        for id in all {
            if id == cutoff {
                continue;
            }
            let op = Log::op(self, id)?.context("an operation listed a moment ago")?;
            if id < cutoff || op.parent_id.is_some_and(|p| doomed.contains(&p)) {
                doomed.insert(id);
                revs_hit.insert(op.rev_id);
            }
        }
        let mut ordered: Vec<i64> = doomed.iter().copied().collect();
        ordered.sort_unstable_by(|a, b| b.cmp(a));
        self.delete_ops(&ordered)?;
        for rev in revs_hit {
            let empty = {
                let txn = self.txn.borrow();
                Read { t: &self.t, r: &txn }.revision_op_ids(rev)?.is_empty()
            };
            if empty {
                self.t.revisions.delete(&mut self.txn.borrow_mut(), &be(rev))?;
            }
        }
        Ok(ordered.len())
    }

    fn clear_metarecords(&self) -> Result<()> {
        let mut w = self.txn.borrow_mut();
        let t = self.t;
        for db in [t.metarecords, t.cells, t.row_owner, t.by_field, t.field_types, t.forest] {
            db.clear(&mut w)?;
        }
        Ok(())
    }

    fn detach_op(&self, op: i64) -> Result<()> {
        let Some(row) = Log::op(self, op)? else { return Ok(()) };
        let Some(parent) = row.parent_id else { return Ok(()) };
        let b = self.t.ops.get(&self.txn.borrow(), &be(op))?.context("the operation")?.to_vec();
        // The parent is the encoding's first field: rewrite it as `None`.
        let rest = &b[9..];
        let mut out = vec![0u8];
        out.extend_from_slice(rest);
        let mut w = self.txn.borrow_mut();
        self.t.ops.put(&mut w, &be(op), &out)?;
        self.t.op_children.delete(&mut w, &key(&[&be(parent), &be(op)]))?;
        Ok(())
    }

    fn delete_ops(&self, ids: &[i64]) -> Result<()> {
        for &id in ids {
            self.remove_op(id)?;
        }
        Ok(())
    }

    fn drop_empty_revisions(&self) -> Result<()> {
        let revs: Vec<i64> = {
            let txn = self.txn.borrow();
            let mut out = Vec::new();
            for e in self.t.revisions.iter(&txn)? {
                out.push(from_be(e?.0));
            }
            out
        };
        for rev in revs {
            let empty = {
                let txn = self.txn.borrow();
                Read { t: &self.t, r: &txn }.revision_op_ids(rev)?.is_empty()
            };
            if empty {
                self.t.revisions.delete(&mut self.txn.borrow_mut(), &be(rev))?;
            }
        }
        Ok(())
    }

    fn queue_restoration(&self, restoration: &Restoration) -> Result<()> {
        let n = self.next("next_restoration")?;
        self.t.restorations.put(
            &mut self.txn.borrow_mut(),
            &be(n),
            &enc_restoration(restoration),
        )?;
        Ok(())
    }

    fn drop_restorations(&self, up_to: i64) -> Result<()> {
        let keys: Vec<i64> =
            Log::restorations(self)?.into_iter().map(|(k, _)| k).filter(|k| *k <= up_to).collect();
        let mut w = self.txn.borrow_mut();
        for k in keys {
            self.t.restorations.delete(&mut w, &be(k))?;
        }
        Ok(())
    }

    fn commit(self: Box<Self>) -> Result<()> {
        self.txn.into_inner().commit().context("Failed to commit write transaction")
    }
}
