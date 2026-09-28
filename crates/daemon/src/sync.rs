//! Cross-repo synchronisation state (spec-sync.org). Sync state — the links
//! between metarecords of two repositories and the snapshot of their common
//! field state at the last sync — lives *outside the data model*, in a
//! per-pair key-value store (`sync-<uuid_a>-<uuid_b>/`, LMDB) held under one
//! repo's `internal/`. This module is the storage layer over that store; the
//! cross-repo orchestration (status truth table, candidates, records inline)
//! lives in the HTTP handlers, which hold both repositories.
//!
//! | table  | key                    | value                                   |
//! |--------|------------------------|-----------------------------------------|
//! | meta   | name                   | text                                    |
//! | links  | link uuid              | record_a · record_b · version_a · _b    |
//! | by_a   | record_a               | link uuid                               |
//! | by_b   | record_b               | link uuid                               |
//! | snaps  | link uuid · index      | one snapshot field (JSON)               |

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use anyhow::{bail, Context, Result};
use heed::types::Bytes;
use heed::{Database, Env, EnvOpenOptions, WithoutTls};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use metafolder_core::hex;
use metafolder_core::metarecord::Value;
use metafolder_core::sync::MutexExt;

use crate::rows;

pub const FORMAT_VERSION: &str = "2";

/// The room a pair's store reserves: address space, not disk — the file grows
/// with what it holds.
const MAP_SIZE: usize = 8 << 30;

/// Orders a pair of repo UUIDs into canonical `(a, b)` roles: the
/// lexicographically smaller 32-char-hex UUID is repo A. `None` when the two
/// UUIDs are equal (a repo cannot be paired with itself).
pub fn canonical_pair(x: Uuid, y: Uuid) -> Option<(Uuid, Uuid)> {
    match x.as_bytes().cmp(y.as_bytes()) {
        std::cmp::Ordering::Less => Some((x, y)),
        std::cmp::Ordering::Greater => Some((y, x)),
        std::cmp::Ordering::Equal => None,
    }
}

/// The sync store's directory name for a canonical pair.
pub fn sync_db_filename(a: Uuid, b: Uuid) -> String {
    format!("sync-{}-{}", a.as_simple(), b.as_simple())
}

/// A pair's sync store.
#[derive(Clone)]
pub struct SyncDb {
    env: Env<WithoutTls>,
    meta: Database<Bytes, Bytes>,
    links: Database<Bytes, Bytes>,
    by_a: Database<Bytes, Bytes>,
    by_b: Database<Bytes, Bytes>,
    snaps: Database<Bytes, Bytes>,
}

/// The stores this process has open, by path. LMDB allows one environment
/// per file in a process (heed refuses a second opening), and two requests on
/// one pair can run at once: they share it.
static OPEN: LazyLock<Mutex<HashMap<PathBuf, SyncDb>>> = LazyLock::new(Default::default);

/// Opens (creating it if new) a pair's sync store.
pub fn open(path: &Path) -> Result<SyncDb> {
    std::fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
    let key = path.canonicalize().with_context(|| format!("resolve {}", path.display()))?;
    let mut open = OPEN.lock_recover();
    if let Some(db) = open.get(&key) {
        return Ok(db.clone());
    }
    // SAFETY: the registry above makes this the environment's one opening in
    // the process; the map is read-only (no WRITEMAP).
    let env = unsafe {
        let mut options = EnvOpenOptions::new().read_txn_without_tls();
        options.map_size(MAP_SIZE).max_dbs(8);
        options.open(&key)
    }
    .with_context(|| format!("open the sync store at {}", key.display()))?;
    let mut w = env.write_txn()?;
    let mut table = |name| env.create_database::<Bytes, Bytes>(&mut w, Some(name));
    let (meta, links, by_a, by_b, snaps) =
        (table("meta")?, table("links")?, table("by_a")?, table("by_b")?, table("snaps")?);
    w.commit()?;
    let db = SyncDb { env, meta, links, by_a, by_b, snaps };
    open.insert(key, db.clone());
    Ok(db)
}

/// Writes the identification `meta` rows into a sync store.
pub fn write_meta(db: &SyncDb, a: Uuid, b: Uuid, host: Uuid) -> Result<()> {
    let mut w = db.env.write_txn()?;
    for (k, v) in [
        ("format_version", FORMAT_VERSION.to_string()),
        ("repo_a", a.as_simple().to_string()),
        ("repo_b", b.as_simple().to_string()),
        ("host", host.as_simple().to_string()),
    ] {
        db.meta.put(&mut w, k.as_bytes(), v.as_bytes())?;
    }
    w.commit()?;
    Ok(())
}

/// Reads a `meta` value.
pub fn read_meta(db: &SyncDb, key: &str) -> Result<Option<String>> {
    let r = db.env.read_txn()?;
    Ok(db.meta.get(&r, key.as_bytes())?.map(|v| String::from_utf8_lossy(v).into_owned()))
}

/// The result of locating a pair's sync store across the two loaded repos'
/// `internal/` directories (spec-sync "Location and discovery").
pub enum Located {
    /// Found in exactly one repo's `internal/`.
    Found(PathBuf),
    /// Present in neither: the pair has no sync state yet.
    Absent,
    /// Present in both — ambiguous; the daemon never merges (409).
    Ambiguous,
}

/// Locates the sync store for canonical pair `(a, b)` given the two repos'
/// `internal/` directories.
pub fn locate(a_internal: &Path, b_internal: &Path, a: Uuid, b: Uuid) -> Located {
    let name = sync_db_filename(a, b);
    let in_a = a_internal.join(&name);
    let in_b = b_internal.join(&name);
    match (in_a.exists(), in_b.exists()) {
        (true, true) => Located::Ambiguous,
        (true, false) => Located::Found(in_a),
        (false, true) => Located::Found(in_b),
        (false, false) => Located::Absent,
    }
}

/// One link (spec-sync "Links").
#[derive(Debug, Clone)]
pub struct Link {
    pub uuid: Uuid,
    pub record_a: Uuid,
    pub record_b: Uuid,
    pub version_a: Option<u64>,
    pub version_b: Option<u64>,
}

fn enc_version(out: &mut Vec<u8>, v: Option<u64>) {
    match v {
        None => out.push(0),
        Some(v) => {
            out.push(1);
            out.extend_from_slice(&v.to_be_bytes());
        }
    }
}

fn enc_link(l: &Link) -> Vec<u8> {
    let mut out = Vec::with_capacity(50);
    out.extend_from_slice(l.record_a.as_bytes());
    out.extend_from_slice(l.record_b.as_bytes());
    enc_version(&mut out, l.version_a);
    enc_version(&mut out, l.version_b);
    out
}

fn dec_link(uuid: Uuid, b: &[u8]) -> Result<Link> {
    let uuid_at = |i: usize| -> Result<Uuid> {
        Ok(Uuid::from_slice(b.get(i..i + 16).context("a truncated link")?)?)
    };
    let mut at = 32;
    let mut version = || -> Result<Option<u64>> {
        let flag = *b.get(at).context("a truncated link")?;
        at += 1;
        if flag == 0 {
            return Ok(None);
        }
        let bytes: [u8; 8] = b.get(at..at + 8).context("a truncated link")?.try_into()?;
        at += 8;
        Ok(Some(u64::from_be_bytes(bytes)))
    };
    let (version_a, version_b) = (version()?, version()?);
    Ok(Link { uuid, record_a: uuid_at(0)?, record_b: uuid_at(16)?, version_a, version_b })
}

/// All links, ordered by UUID.
pub fn list_links(db: &SyncDb) -> Result<Vec<Link>> {
    let r = db.env.read_txn()?;
    let mut out = Vec::new();
    for e in db.links.iter(&r)? {
        let (k, v) = e?;
        out.push(dec_link(Uuid::from_slice(k)?, v)?);
    }
    Ok(out)
}

/// One link by its UUID.
pub fn get_link(db: &SyncDb, uuid: Uuid) -> Result<Option<Link>> {
    let r = db.env.read_txn()?;
    db.links.get(&r, uuid.as_bytes())?.map(|v| dec_link(uuid, v)).transpose()
}

/// The link (if any) whose given-side record is `record`.
pub fn link_for_record(db: &SyncDb, side: Side, record: Uuid) -> Result<Option<Link>> {
    let r = db.env.read_txn()?;
    let index = match side {
        Side::A => db.by_a,
        Side::B => db.by_b,
    };
    let Some(link) = index.get(&r, record.as_bytes())? else { return Ok(None) };
    let uuid = Uuid::from_slice(link)?;
    db.links.get(&r, uuid.as_bytes())?.map(|v| dec_link(uuid, v)).transpose()
}

/// Canonical role of a repo within a pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    A,
    B,
}

/// Creates a link with no versions and no snapshot (state `never_synced`).
/// A record already linked in this pair, on either side, is refused.
pub fn create_link(db: &SyncDb, record_a: Uuid, record_b: Uuid) -> Result<Link> {
    let mut w = db.env.write_txn()?;
    if db.by_a.get(&w, record_a.as_bytes())?.is_some()
        || db.by_b.get(&w, record_b.as_bytes())?.is_some()
    {
        bail!("a record is already linked in this pair");
    }
    let link = Link { uuid: Uuid::new_v4(), record_a, record_b, version_a: None, version_b: None };
    db.links.put(&mut w, link.uuid.as_bytes(), &enc_link(&link))?;
    db.by_a.put(&mut w, record_a.as_bytes(), link.uuid.as_bytes())?;
    db.by_b.put(&mut w, record_b.as_bytes(), link.uuid.as_bytes())?;
    w.commit()?;
    Ok(link)
}

/// Deletes a link and its snapshot.
pub fn delete_link(db: &SyncDb, uuid: Uuid) -> Result<bool> {
    let mut w = db.env.write_txn()?;
    let Some(link) = db.links.get(&w, uuid.as_bytes())?.map(|v| dec_link(uuid, v)).transpose()?
    else {
        return Ok(false);
    };
    db.links.delete(&mut w, uuid.as_bytes())?;
    db.by_a.delete(&mut w, link.record_a.as_bytes())?;
    db.by_b.delete(&mut w, link.record_b.as_bytes())?;
    delete_snapshot(db, &mut w, uuid)?;
    w.commit()?;
    Ok(true)
}

/// One snapshot field: the common value at the last sync, in dual perspective
/// (spec-sync "Ref and TreeRef fields in the snapshot"). `value` holds repo A's
/// perspective; `value_uuid_b` the B-perspective UUID for `ref`/`tree_ref`.
#[derive(Debug, Clone)]
pub struct SnapshotField {
    pub name: String,
    pub value: Value,
    pub value_uuid_b: Option<Uuid>,
}

/// A snapshot field as stored: the value in its column form
/// (spec-data-model "Storage"), byte strings in hex.
#[derive(Serialize, Deserialize)]
struct StoredField {
    name: String,
    value_type: String,
    text: Option<String>,
    int: Option<i64>,
    real: Option<f64>,
    uuid: Option<String>,
    ref_repo: Option<String>,
    value_name: Option<String>,
    name_bytes: Option<String>,
    uuid_b: Option<String>,
}

fn enc_field(f: &SnapshotField) -> Result<Vec<u8>> {
    let e = rows::encode_value(&f.value);
    let h = |b: Option<Vec<u8>>| b.map(|b| hex::encode(&b));
    Ok(serde_json::to_vec(&StoredField {
        name: f.name.clone(),
        value_type: e.value_type.to_string(),
        text: e.text,
        int: e.int,
        real: e.real,
        uuid: h(e.uuid),
        ref_repo: h(e.ref_repo),
        value_name: e.name,
        name_bytes: h(e.name_bytes),
        uuid_b: f.value_uuid_b.map(|u| hex::encode(u.as_bytes())),
    })?)
}

fn dec_field(b: &[u8]) -> Result<SnapshotField> {
    let s: StoredField = serde_json::from_slice(b).context("a snapshot field")?;
    let bytes = |h: Option<String>| -> Result<Option<Vec<u8>>> {
        h.map(|h| hex::decode(&h).context("a hex column")).transpose()
    };
    let value = rows::decode_value(rows::RawValue {
        value_type: s.value_type,
        text: s.text,
        int: s.int,
        real: s.real,
        uuid: bytes(s.uuid)?,
        ref_repo: bytes(s.ref_repo)?,
        name: s.value_name,
        name_bytes: bytes(s.name_bytes)?,
    })?;
    let value_uuid_b = bytes(s.uuid_b)?.map(|b| Uuid::from_slice(&b)).transpose()?;
    Ok(SnapshotField { name: s.name, value, value_uuid_b })
}

/// Deletes one link's snapshot fields: the keys its uuid prefixes.
fn delete_snapshot(db: &SyncDb, w: &mut heed::RwTxn, link: Uuid) -> Result<()> {
    let mut end = link.as_bytes().to_vec();
    end.extend_from_slice(&[0xff; 4]);
    let range =
        (std::ops::Bound::Included(&link.as_bytes()[..]), std::ops::Bound::Included(&end[..]));
    db.snaps.delete_range(w, &range)?;
    Ok(())
}

/// The snapshot fields of a link.
pub fn read_snapshot(db: &SyncDb, link: Uuid) -> Result<Vec<SnapshotField>> {
    let r = db.env.read_txn()?;
    let mut out = Vec::new();
    for e in db.snaps.prefix_iter(&r, link.as_bytes())? {
        out.push(dec_field(e?.1)?);
    }
    Ok(out)
}

/// One entry of a sync-commit batch: set a link's recorded versions and replace
/// its snapshot.
pub struct Commit {
    pub link: Uuid,
    pub version_a: u64,
    pub version_b: u64,
    pub snapshot: Vec<SnapshotField>,
}

/// Applies a batch of sync-commits in a single transaction (spec-sync
/// `POST …/links/commit`): per commit, update the link's versions and replace
/// its snapshot.
pub fn commit_batch(db: &SyncDb, commits: &[Commit]) -> Result<()> {
    let mut w = db.env.write_txn()?;
    for c in commits {
        let Some(mut link) =
            db.links.get(&w, c.link.as_bytes())?.map(|v| dec_link(c.link, v)).transpose()?
        else {
            bail!("link not found: {}", c.link);
        };
        link.version_a = Some(c.version_a);
        link.version_b = Some(c.version_b);
        db.links.put(&mut w, c.link.as_bytes(), &enc_link(&link))?;
        delete_snapshot(db, &mut w, c.link)?;
        for (i, f) in c.snapshot.iter().enumerate() {
            let mut key = c.link.as_bytes().to_vec();
            key.extend_from_slice(&(i as u32).to_be_bytes());
            db.snaps.put(&mut w, &key, &enc_field(f)?)?;
        }
    }
    w.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir()
            .join("metafolder-tests")
            .join(format!("sync_{tag}_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Two requests on one pair open its store at once: they share it rather
    /// than the second failing on LMDB's one-opening-per-process rule.
    #[test]
    fn a_store_opened_twice_is_shared() {
        let root = dir("twice");
        let path = root.join(sync_db_filename(Uuid::new_v4(), Uuid::new_v4()));
        let first = open(&path).unwrap();
        let second = open(&path).unwrap();
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let link = create_link(&first, a, b).unwrap();
        assert_eq!(get_link(&second, link.uuid).unwrap().unwrap().record_b, b);
        std::fs::remove_dir_all(root).ok();
    }

    /// A snapshot keeps every value type, a name that is not UTF-8 included,
    /// and a commit replaces it whole.
    #[test]
    fn snapshots_round_trip_and_are_replaced() {
        use metafolder_core::metarecord::TreeName;
        let root = dir("snap");
        let db = open(&root.join("s")).unwrap();
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let link = create_link(&db, a, b).unwrap();
        let parent = Uuid::new_v4();
        let fields = vec![
            SnapshotField {
                name: "s".into(),
                value: Value::String("x".into()),
                value_uuid_b: None,
            },
            SnapshotField { name: "f".into(), value: Value::Float(1.5), value_uuid_b: None },
            SnapshotField {
                name: "p".into(),
                value: Value::TreeRef {
                    parent: Some(parent),
                    name: TreeName::from_bytes(b"caf\xe9".to_vec()),
                },
                value_uuid_b: Some(Uuid::new_v4()),
            },
        ];
        commit_batch(
            &db,
            &[Commit { link: link.uuid, version_a: 1, version_b: 2, snapshot: fields.clone() }],
        )
        .unwrap();
        let got = read_snapshot(&db, link.uuid).unwrap();
        assert_eq!(got.len(), 3);
        for (g, w) in got.iter().zip(&fields) {
            assert_eq!((&g.name, &g.value, g.value_uuid_b), (&w.name, &w.value, w.value_uuid_b));
        }
        let l = get_link(&db, link.uuid).unwrap().unwrap();
        assert_eq!((l.version_a, l.version_b), (Some(1), Some(2)));

        commit_batch(
            &db,
            &[Commit {
                link: link.uuid,
                version_a: 3,
                version_b: 4,
                snapshot: fields[..1].to_vec(),
            }],
        )
        .unwrap();
        assert_eq!(read_snapshot(&db, link.uuid).unwrap().len(), 1, "replaced, not appended");
        assert!(delete_link(&db, link.uuid).unwrap());
        assert!(read_snapshot(&db, link.uuid).unwrap().is_empty(), "the snapshot goes too");
        assert!(link_for_record(&db, Side::A, a).unwrap().is_none());
        std::fs::remove_dir_all(root).ok();
    }
}
