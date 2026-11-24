//! Cross-repo synchronisation state (doc "The sync database"). Sync state — the links
//! between metarecords of two repositories and the snapshot of their common
//! field state at the last sync — lives *outside the data model*, in a
//! per-pair key-value store (`sync-<uuid_a>-<uuid_b>/`, LMDB) held under one
//! repo's `internal/`. This module is the storage layer over that store; the
//! cross-repo orchestration (status truth table, candidates, records inline)
//! lives in the HTTP handlers, which hold both repositories.
//!
//! | table             | key            | value                                |
//! |-------------------|----------------|--------------------------------------|
//! | version           | `format_version` | the format (u32)                   |
//! | repositories      | `a`, `b`, `host` | a repository uuid                  |
//! | links             | link uuid      | record_a · record_b · version_a · _b |
//! | links_by_record_a | record_a       | link uuid                            |
//! | links_by_record_b | record_b       | link uuid                            |
//! | snapshots         | link uuid      | the link's snapshot entries          |

mod v2;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use anyhow::{bail, Context, Result};
use heed::types::Bytes;
use heed::{Database, Env, EnvOpenOptions, WithoutTls};
use uuid::Uuid;

use metafolder_core::metarecord::Value;
use metafolder_core::sync::MutexExt;

use crate::kvstore::codec::{Dec, Enc};

/// The format of a pair's store. A store of format 2 (`meta` table) is
/// converted when it is opened; a newer one is refused.
pub const FORMAT_VERSION: u32 = 3;

/// The key of the format in the `version` table.
const FORMAT_KEY: &[u8] = b"format_version";

/// The keys of the `repositories` table: canonical A, canonical B, the host.
const REPOSITORY_KEYS: [&[u8]; 3] = [b"a", b"b", b"host"];

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
    version: Database<'static, Bytes, Bytes>,
    repositories: Database<'static, Bytes, Bytes>,
    links: Database<'static, Bytes, Bytes>,
    links_by_record_a: Database<'static, Bytes, Bytes>,
    links_by_record_b: Database<'static, Bytes, Bytes>,
    snapshots: Database<'static, Bytes, Bytes>,
}

/// The stores this process has open, by canonical path, with the path they
/// were opened at. LMDB allows one environment per file in a process (heed
/// refuses a second opening), and two requests on one pair can run at once:
/// they share it. A repository let go closes the ones it hosts
/// ([`forget_under`]).
static OPEN: LazyLock<Mutex<HashMap<PathBuf, (PathBuf, SyncDb)>>> = LazyLock::new(Default::default);

/// Opens (creating it if new) a pair's sync store, converting one of an older
/// format.
pub fn open(path: &Path) -> Result<SyncDb> {
    std::fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
    let key = path.canonicalize().with_context(|| format!("resolve {}", path.display()))?;
    let mut open = OPEN.lock_recover();
    if let Some((_, db)) = open.get(&key) {
        return Ok(db.clone());
    }
    // SAFETY: the registry above makes this the environment's one opening in
    // the process; the map is read-only (no WRITEMAP).
    let env = unsafe {
        let mut options = EnvOpenOptions::new().read_txn_without_tls();
        // The current tables and those of format 2, while it is converted.
        options.map_size(MAP_SIZE).max_dbs(16);
        options.open(&key)
    }
    .with_context(|| format!("open the sync store at {}", key.display()))?;
    let old = v2::open(&env)?;
    let w = env.write_txn()?;
    let created = env.create_databases_and_commit(w, |dbo, w| -> heed::Result<_> {
        let mut table = |name| dbo.create_database::<Bytes, Bytes>(w, Some(name));
        let tables = [
            table("version")?,
            table("repositories")?,
            table("links")?,
            table("links_by_record_a")?,
            table("links_by_record_b")?,
            table("snapshots")?,
        ];
        Ok(heed::CommitOrAbort::<_, ()>::Commit(tables))
    })?;
    let [version, repositories, links, links_by_record_a, links_by_record_b, snapshots] =
        created.commit().expect("the databases are committed");
    let db = SyncDb {
        env,
        version,
        repositories,
        links,
        links_by_record_a,
        links_by_record_b,
        snapshots,
    };
    let stored = {
        let r = db.env.read_txn()?;
        db.version
            .get(&r, FORMAT_KEY)?
            .map(|v| -> Result<u32> { Ok(u32::from_be_bytes(v.try_into()?)) })
            .transpose()
            .context("the format of the sync store")?
    };
    match (stored, old) {
        (Some(FORMAT_VERSION), _) => {}
        (Some(v), _) => bail!(
            "the sync store at {} is of format {v}; this version of metafolder reads format \
             {FORMAT_VERSION}",
            key.display()
        ),
        (None, Some(old)) => v2::convert(&db, old)
            .with_context(|| format!("convert the sync store at {}", key.display()))?,
        (None, None) => {
            let mut w = db.env.write_txn()?;
            db.version.put(&mut w, FORMAT_KEY, &FORMAT_VERSION.to_be_bytes())?;
            w.commit()?;
        }
    }
    open.insert(key, (path.to_path_buf(), db.clone()));
    Ok(db)
}

/// Closes the stores opened under `dir` — a repository's `internal/`, when
/// the repository is let go. Kept open for the life of the process, a store
/// that moved with its repository's root was opened a second time at the new
/// place, where LMDB allows one opening of a file per process. The
/// environment closes once a request still holding it is done.
pub fn forget_under(dir: &Path) {
    let canonical = dir.canonicalize().ok();
    OPEN.lock_recover().retain(|key, (given, _)| {
        !(given.starts_with(dir) || canonical.as_ref().is_some_and(|c| key.starts_with(c)))
    });
}

/// Whether the store at `path` is open in this process.
#[cfg(test)]
fn is_open(path: &Path) -> bool {
    OPEN.lock_recover().values().any(|(given, _)| given == path)
}

/// The repositories a pair's store names: canonical A and B, and the host —
/// the one whose `internal/` holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Repositories {
    pub a: Uuid,
    pub b: Uuid,
    pub host: Uuid,
}

/// Writes the repositories the store names.
pub fn write_repositories(db: &SyncDb, r: Repositories) -> Result<()> {
    let mut w = db.env.write_txn()?;
    for (k, v) in REPOSITORY_KEYS.iter().zip([r.a, r.b, r.host]) {
        db.repositories.put(&mut w, k, v.as_bytes())?;
    }
    w.commit()?;
    Ok(())
}

/// The repositories the store names; `None` if it does not name them all.
pub fn read_repositories(db: &SyncDb) -> Result<Option<Repositories>> {
    let r = db.env.read_txn()?;
    let mut got = [None; 3];
    for (k, slot) in REPOSITORY_KEYS.iter().zip(&mut got) {
        *slot = db.repositories.get(&r, k)?.map(Uuid::from_slice).transpose()?;
    }
    Ok(match got {
        [Some(a), Some(b), Some(host)] => Some(Repositories { a, b, host }),
        _ => None,
    })
}

/// The result of locating a pair's sync store across the two loaded repos'
/// `internal/` directories (doc "The sync database").
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

/// One link (doc "The sync database").
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
        Side::A => db.links_by_record_a,
        Side::B => db.links_by_record_b,
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

impl Side {
    /// `"a"` / `"b"`, as the wire spells it.
    pub fn name(self) -> &'static str {
        match self {
            Side::A => "a",
            Side::B => "b",
        }
    }

    pub fn parse(s: &str) -> Result<Side> {
        match s {
            "a" => Ok(Side::A),
            "b" => Ok(Side::B),
            other => bail!("a side is 'a' or 'b', not '{other}'"),
        }
    }
}

/// Creates a link with no versions and no snapshot (state `never_synced`).
/// A record already linked in this pair, on either side, is refused.
pub fn create_link(db: &SyncDb, record_a: Uuid, record_b: Uuid) -> Result<Link> {
    let mut w = db.env.write_txn()?;
    if db.links_by_record_a.get(&w, record_a.as_bytes())?.is_some()
        || db.links_by_record_b.get(&w, record_b.as_bytes())?.is_some()
    {
        bail!("a record is already linked in this pair");
    }
    let link = Link { uuid: Uuid::new_v4(), record_a, record_b, version_a: None, version_b: None };
    db.links.put(&mut w, link.uuid.as_bytes(), &enc_link(&link))?;
    db.links_by_record_a.put(&mut w, record_a.as_bytes(), link.uuid.as_bytes())?;
    db.links_by_record_b.put(&mut w, record_b.as_bytes(), link.uuid.as_bytes())?;
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
    db.links_by_record_a.delete(&mut w, link.record_a.as_bytes())?;
    db.links_by_record_b.delete(&mut w, link.record_b.as_bytes())?;
    db.snapshots.delete(&mut w, uuid.as_bytes())?;
    w.commit()?;
    Ok(true)
}

/// One snapshot field. A *common* one (`side` = `None`) is a value both
/// repositories held at the last sync, in dual perspective (doc "The sync
/// database"): `value` is repo A's, `value_uuid_b` the
/// B-perspective UUID of a `ref` target or a `tree_ref` parent. A *per-side*
/// one records what one repository alone held — what sync does not equalize,
/// such as a file's own size and mtime.
#[derive(Debug, Clone)]
pub struct SnapshotField {
    pub name: String,
    pub value: Value,
    pub value_uuid_b: Option<Uuid>,
    pub side: Option<Side>,
}

/// The entry flag of a B-perspective uuid; the two low bits are the side.
const HAS_UUID_B: u8 = 4;

/// A link's snapshot, in the store's compact encoding (doc "Value encoding in
/// the store"):
///
/// ```text
/// snapshot = varint count · { entry }
/// entry    = varint len · name · flags · type · value [· uuid_b]
/// flags    = side (0 common, 1 a, 2 b) | 4 when uuid_b follows
/// ```
fn enc_snapshot(fields: &[SnapshotField]) -> Vec<u8> {
    let mut e = Enc::default();
    e.varint(fields.len() as u64);
    for f in fields {
        let side = match f.side {
            None => 0,
            Some(Side::A) => 1,
            Some(Side::B) => 2,
        };
        let flags = side | if f.value_uuid_b.is_some() { HAS_UUID_B } else { 0 };
        e.bytes(f.name.as_bytes()).raw(&[flags]).value(&f.value);
        if let Some(u) = f.value_uuid_b {
            e.raw(u.as_bytes());
        }
    }
    e.0
}

fn dec_snapshot(b: &[u8]) -> Result<Vec<SnapshotField>> {
    let mut d = Dec(b);
    let count = d.varint()?;
    let mut out = Vec::new();
    for _ in 0..count {
        let name = d.string()?;
        let flags = d.take(1)?[0];
        let side = match flags & !HAS_UUID_B {
            0 => None,
            1 => Some(Side::A),
            2 => Some(Side::B),
            _ => bail!("a snapshot entry of flags {flags}"),
        };
        let value = d.value()?;
        let value_uuid_b = (flags & HAS_UUID_B != 0).then(|| d.uuid()).transpose()?;
        out.push(SnapshotField { name, value, value_uuid_b, side });
    }
    if !d.is_empty() {
        bail!("bytes after a snapshot");
    }
    Ok(out)
}

/// The snapshot fields of a link.
pub fn read_snapshot(db: &SyncDb, link: Uuid) -> Result<Vec<SnapshotField>> {
    let r = db.env.read_txn()?;
    db.snapshots.get(&r, link.as_bytes())?.map_or(Ok(Vec::new()), dec_snapshot)
}

/// One entry of a sync-commit batch: set a link's recorded versions and replace
/// its snapshot.
pub struct Commit {
    pub link: Uuid,
    pub version_a: u64,
    pub version_b: u64,
    pub snapshot: Vec<SnapshotField>,
}

/// Applies a batch of sync-commits in a single transaction (doc "Sync
/// endpoints", `POST …/links/commit`): per commit, update the link's versions and replace
/// its snapshot.
pub fn commit_batch(db: &SyncDb, commits: &[Commit]) -> Result<()> {
    let mut w = db.env.write_txn()?;
    for c in commits {
        let Some(mut link) =
            db.links.get(&w, c.link.as_bytes())?.map(|v| dec_link(c.link, v)).transpose()?
        else {
            return Err(
                crate::error::DomainError::NotFound(format!("link not found: {}", c.link)).into()
            );
        };
        link.version_a = Some(c.version_a);
        link.version_b = Some(c.version_b);
        db.links.put(&mut w, c.link.as_bytes(), &enc_link(&link))?;
        // An empty snapshot is an absent one: one form for it.
        if c.snapshot.is_empty() {
            db.snapshots.delete(&mut w, c.link.as_bytes())?;
        } else {
            db.snapshots.put(&mut w, c.link.as_bytes(), &enc_snapshot(&c.snapshot))?;
        }
    }
    w.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use metafolder_core::metarecord::TreeName;

    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir()
            .join("metafolder-tests")
            .join(format!("sync_{tag}_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A snapshot holding every kind of entry: common, per side, with a
    /// B-perspective uuid, a name that is not UTF-8.
    fn sample_snapshot() -> Vec<SnapshotField> {
        vec![
            SnapshotField {
                name: "s".into(),
                value: Value::String("x".into()),
                value_uuid_b: None,
                side: None,
            },
            SnapshotField {
                name: "f".into(),
                value: Value::Float(1.5),
                value_uuid_b: None,
                side: Some(Side::B),
            },
            SnapshotField {
                name: "d".into(),
                value: Value::DateTime(1_700_000_000_123),
                value_uuid_b: None,
                side: Some(Side::A),
            },
            SnapshotField {
                name: "p".into(),
                value: Value::TreeRef {
                    parent: Some(Uuid::new_v4()),
                    name: TreeName::from_bytes(b"caf\xe9".to_vec()),
                },
                value_uuid_b: Some(Uuid::new_v4()),
                side: None,
            },
            SnapshotField {
                name: "r".into(),
                value: Value::Ref(Uuid::new_v4()),
                value_uuid_b: None,
                side: None,
            },
        ]
    }

    fn assert_same(got: &[SnapshotField], want: &[SnapshotField]) {
        assert_eq!(got.len(), want.len());
        for (g, w) in got.iter().zip(want) {
            assert_eq!(
                (&g.name, &g.value, g.value_uuid_b, g.side),
                (&w.name, &w.value, w.value_uuid_b, w.side)
            );
        }
    }

    /// The names of the tables in the store's catalog.
    fn tables(db: &SyncDb) -> Vec<String> {
        let r = db.env.read_txn().unwrap();
        // The unnamed database's keys are the named ones.
        let main = db
            .env
            .open_databases_and_commit(r, |dbo, r| -> heed::Result<_> {
                Ok(heed::CommitOrAbort::<_, ()>::Commit(
                    dbo.open_database::<heed::types::Str, Bytes>(r, None)?,
                ))
            })
            .unwrap()
            .commit()
            .expect("committed")
            .expect("the unnamed database");
        let r = db.env.read_txn().unwrap();
        let names = main.iter(&r).unwrap().map(|e| e.unwrap().0.to_string()).collect();
        names
    }

    /// Closes the store at `path` (opened under `root`), as unloading its host
    /// does.
    fn close(root: &Path, db: SyncDb) {
        forget_under(root);
        db.env.prepare_for_closing().wait();
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

    /// A repository let go closes the pair stores it hosts: kept open for the
    /// life of the process, a store moved with its repository's root was
    /// opened a second time at the new place — two openings of one LMDB file
    /// in a process, which breaks its locking.
    #[test]
    fn the_stores_under_a_directory_are_forgotten_with_it() {
        let root = dir("forget");
        let (host, other) = (root.join("host/internal"), root.join("other/internal"));
        let hosted = host.join(sync_db_filename(Uuid::new_v4(), Uuid::new_v4()));
        let elsewhere = other.join(sync_db_filename(Uuid::new_v4(), Uuid::new_v4()));
        drop(open(&hosted).unwrap());
        drop(open(&elsewhere).unwrap());
        assert!(is_open(&hosted) && is_open(&elsewhere));

        forget_under(&host);
        assert!(!is_open(&hosted), "the host's store is closed");
        assert!(is_open(&elsewhere), "another repository's is not");
        std::fs::remove_dir_all(root).ok();
    }

    /// The repositories a store names read back as written, and a store that
    /// names none reads as `None`.
    #[test]
    fn the_repositories_round_trip() {
        let root = dir("repos");
        let db = open(&root.join("s")).unwrap();
        assert_eq!(read_repositories(&db).unwrap(), None);
        let r = Repositories { a: Uuid::new_v4(), b: Uuid::new_v4(), host: Uuid::new_v4() };
        write_repositories(&db, r).unwrap();
        assert_eq!(read_repositories(&db).unwrap(), Some(r));
        std::fs::remove_dir_all(root).ok();
    }

    /// A snapshot keeps every value type, a name that is not UTF-8 included,
    /// and a commit replaces it whole.
    #[test]
    fn snapshots_round_trip_and_are_replaced() {
        let root = dir("snap");
        let db = open(&root.join("s")).unwrap();
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let link = create_link(&db, a, b).unwrap();
        let fields = sample_snapshot();
        commit_batch(
            &db,
            &[Commit { link: link.uuid, version_a: 1, version_b: 2, snapshot: fields.clone() }],
        )
        .unwrap();
        assert_same(&read_snapshot(&db, link.uuid).unwrap(), &fields);
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

    /// A store of format 2 — the `meta` table, a snapshot field per key in
    /// JSON — is converted when it is opened: the repositories, the links, their
    /// versions and snapshots are kept, and the tables of format 2 go.
    #[test]
    fn a_format_2_store_is_converted_at_opening() {
        use v2::fixture::OldLink;
        let root = dir("v2");
        let path = root.join("s");
        let pair = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
        let [synced, never, ra, rb, na, nb] = [(); 6].map(|_| Uuid::new_v4());
        let snapshot = sample_snapshot();
        v2::fixture::write(
            &path,
            pair,
            &[
                OldLink {
                    uuid: synced,
                    record_a: ra,
                    record_b: rb,
                    versions: Some((7, 9)),
                    snapshot: snapshot.clone(),
                },
                OldLink {
                    uuid: never,
                    record_a: na,
                    record_b: nb,
                    versions: None,
                    snapshot: vec![],
                },
            ],
        );

        let db = open(&path).unwrap();
        assert_eq!(
            read_repositories(&db).unwrap(),
            Some(Repositories { a: pair[0], b: pair[1], host: pair[2] })
        );
        let l = link_for_record(&db, Side::A, ra).unwrap().expect("the A index is kept");
        assert_eq!((l.uuid, l.record_b, l.version_a, l.version_b), (synced, rb, Some(7), Some(9)));
        assert_eq!(link_for_record(&db, Side::B, nb).unwrap().map(|l| l.uuid), Some(never));
        let n = get_link(&db, never).unwrap().unwrap();
        assert_eq!((n.version_a, n.version_b), (None, None), "a link never synced stays so");
        assert_eq!(list_links(&db).unwrap().len(), 2);
        assert_same(&read_snapshot(&db, synced).unwrap(), &snapshot);
        assert!(read_snapshot(&db, never).unwrap().is_empty());

        let mut names = tables(&db);
        names.sort();
        assert_eq!(
            names,
            [
                "links",
                "links_by_record_a",
                "links_by_record_b",
                "repositories",
                "snapshots",
                "version"
            ],
            "the tables of format 2 are removed"
        );
        std::fs::remove_dir_all(root).ok();
    }

    /// A store of the current format is opened without being written.
    #[test]
    fn an_up_to_date_store_is_opened_without_a_write() {
        let root = dir("current");
        let path = root.join("s");
        let db = open(&path).unwrap();
        create_link(&db, Uuid::new_v4(), Uuid::new_v4()).unwrap();
        let before = db.env.info().last_txn_id;
        close(&root, db);

        let db = open(&path).unwrap();
        assert_eq!(db.env.info().last_txn_id, before, "no transaction was committed");
        std::fs::remove_dir_all(root).ok();
    }

    /// A store of a format newer than this daemon's is refused, not misread.
    #[test]
    fn a_newer_format_is_refused() {
        let root = dir("newer");
        let path = root.join("s");
        let db = open(&path).unwrap();
        let mut w = db.env.write_txn().unwrap();
        db.version.put(&mut w, FORMAT_KEY, &(FORMAT_VERSION + 1).to_be_bytes()).unwrap();
        w.commit().unwrap();
        close(&root, db);

        let err = format!("{:#}", open(&path).err().expect("refused"));
        assert!(err.contains(&format!("format {}", FORMAT_VERSION + 1)), "{err}");
        std::fs::remove_dir_all(root).ok();
    }
}
