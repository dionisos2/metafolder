//! Converting a repository from one storage backend to the other
//! (docs/spec-storage.org "Choosing the backend", increment 5).
//!
//! The copy is faithful: the metarecords and their versions, every row under
//! its own id, the whole history under its own ids (operations, snapshots,
//! revisions with their labels and origins, HEAD, the queued restorations),
//! and the counters, so the converted repository never hands out an id the
//! original already did. [`verify`] then reads both sides and compares them.
//!
//! [`convert_repository`] does it on disk: the new store is written beside
//! the old one, verified, put in place, and `config.json` — rewritten
//! atomically — is the switch. Until that write the repository is what it
//! was; after it, the old store is set aside, not deleted.

use std::collections::{BTreeSet, HashMap};
use std::hash::Hasher as _;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use xxhash_rust::xxh3::Xxh3;

use crate::config::{RepoConfig, Storage};
use crate::repo::{RepoLocator, DB_FILE, INTERNAL_DIR, KV_DIR};
use crate::store::{Begin, Store};

/// What a conversion copied, and where the old store went.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Report {
    pub metarecords: usize,
    pub rows: usize,
    pub operations: usize,
    pub revisions: usize,
    /// The former store, set aside (delete it once the repository is known
    /// to work).
    pub old_store: PathBuf,
}

/// Copies everything `from` holds into `to`, which must be empty, in one
/// transaction.
pub fn copy(from: &dyn Store, to: &mut dyn Begin) -> Result<()> {
    let tx = to.begin_write()?;
    let mut uuids = from.metarecords()?;
    uuids.sort();
    for uuid in uuids {
        let version = from.version(uuid)?.context("a metarecord without a version")?;
        tx.create_metarecord(uuid, version)?;
    }
    from.for_each_row(&mut |uuid, row| {
        tx.insert_row(uuid, &row.name, &row.value, Some(row.id))?;
        Ok(())
    })?;
    let mut ops = from.all_ops()?;
    ops.sort_by_key(|op| op.id);
    let revs: BTreeSet<i64> = ops.iter().map(|op| op.rev_id).collect();
    let metas = from.revisions(&revs.iter().copied().collect::<Vec<_>>())?;
    for id in &revs {
        tx.import_revision(*id, metas.get(id).context("an operation's revision is missing")?)?;
    }
    for op in &ops {
        tx.import_op(op, &from.snapshots(op.id, false)?, &from.snapshots(op.id, true)?)?;
    }
    tx.set_head(from.head()?)?;
    for (_, restoration) in from.restorations()? {
        tx.queue_restoration(&restoration)?;
    }
    tx.raise_counters(from.counters()?)?;
    tx.commit()
}

/// [`copy`] into a new KV store at `dir`, starting on a map of `map_size`.
///
/// The copy is one write transaction, and LMDB grows a map only between
/// transactions ([`KvStore`](crate::kvstore::KvStore) doubles it at each
/// one's start, to what the store already holds — nothing, here). So a
/// repository larger than the map would end in `MDB_MAP_FULL`: the store is
/// then thrown away and the copy starts again on a map twice as large. The
/// map reserves address space, not disk, so overshooting costs nothing.
pub fn copy_into_kv(from: &dyn Store, dir: &Path, map_size: usize) -> Result<()> {
    /// Past this, a full map is not the store being too small.
    const MAX_MAP: usize = 1 << 40;
    let mut map_size = map_size;
    loop {
        let result = {
            let mut target = crate::kvstore::KvStore::open_with_map_size(dir, map_size)?;
            copy(from, &mut target)
        };
        match result {
            Err(err) if map_full(&err) && map_size < MAX_MAP => {
                remove(dir)?;
                map_size = map_size.saturating_mul(2);
            }
            other => return other,
        }
    }
}

fn map_full(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<heed::Error>(),
            Some(heed::Error::Mdb(heed::MdbError::MapFull))
        )
    })
}

/// A digest of everything [`copy`] carries, with the counts of the report.
struct Digest {
    hash: u64,
    metarecords: usize,
    rows: usize,
    operations: usize,
    revisions: usize,
}

fn digest(store: &dyn Store) -> Result<Digest> {
    let mut h = Xxh3::new();
    let mut feed = |text: String| {
        h.write(text.as_bytes());
        h.write(&[0]);
    };
    let mut rows = 0usize;
    store.for_each_row(&mut |uuid, row| {
        rows += 1;
        feed(format!("row {uuid} {row:?}"));
        Ok(())
    })?;
    let mut uuids = store.metarecords()?;
    uuids.sort();
    for u in &uuids {
        feed(format!("version {u} {:?}", store.version(*u)?));
    }
    let mut ops = store.all_ops()?;
    ops.sort_by_key(|op| op.id);
    let revs: BTreeSet<i64> = ops.iter().map(|op| op.rev_id).collect();
    let metas: HashMap<_, _> = store.revisions(&revs.iter().copied().collect::<Vec<_>>())?;
    for id in &revs {
        feed(format!("revision {id} {:?}", metas.get(id)));
    }
    for op in &ops {
        feed(format!("op {op:?}"));
        feed(format!("before {:?}", store.snapshots(op.id, false)?));
        feed(format!("after {:?}", store.snapshots(op.id, true)?));
    }
    feed(format!("head {:?}", store.head()?));
    let queued: Vec<_> = store.restorations()?.into_iter().map(|(_, r)| r).collect();
    feed(format!("restorations {queued:?}"));
    feed(format!("counters {:?}", store.counters()?));
    Ok(Digest {
        hash: h.finish(),
        metarecords: uuids.len(),
        rows,
        operations: ops.len(),
        revisions: revs.len(),
    })
}

/// Reads both stores and fails unless they hold the same thing.
pub fn verify(from: &dyn Store, to: &dyn Store) -> Result<()> {
    let (a, b) = (digest(from)?, digest(to)?);
    if a.hash != b.hash {
        bail!(
            "the converted store differs from the original ({} metarecords / {} rows / {} \
             operations against {} / {} / {})",
            a.metarecords,
            a.rows,
            a.operations,
            b.metarecords,
            b.rows,
            b.operations
        );
    }
    Ok(())
}

/// Where a backend keeps its store inside `internal/`.
fn store_path(internal: &Path, storage: Storage) -> PathBuf {
    match storage {
        Storage::Sqlite => internal.join(DB_FILE),
        Storage::Kv => internal.join(KV_DIR),
    }
}

/// Removes a file or a directory, if there is one.
fn remove(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(_) => Ok(()),
        Ok(m) if m.is_dir() => {
            std::fs::remove_dir_all(path).with_context(|| format!("remove {}", path.display()))
        }
        Ok(_) => std::fs::remove_file(path).with_context(|| format!("remove {}", path.display())),
    }
}

/// Converts the repository at `locator` — which must not be loaded (its
/// store is held exclusively) — to `to`. See the module documentation for
/// the order of the steps, which is what makes an interruption harmless.
pub fn convert_repository(locator: RepoLocator, to: Storage) -> Result<Report> {
    let metafolder = locator.metafolder_dir()?;
    let mut config = RepoConfig::read(&metafolder)?;
    let from = config.storage;
    if from == to {
        bail!("the repository is already on {to:?}");
    }
    let internal = metafolder.join(INTERNAL_DIR);
    let old_path = store_path(&internal, from);
    let new_path = store_path(&internal, to);
    let temp =
        internal.join(format!("converting-{}", new_path.file_name().unwrap().to_string_lossy()));
    // Leftovers of an interrupted conversion: the config was not switched,
    // so they are not the repository's store.
    remove(&temp)?;
    remove(&new_path)?;
    if to == Storage::Sqlite {
        for suffix in ["-wal", "-shm"] {
            remove(&PathBuf::from(format!("{}{suffix}", new_path.display())))?;
        }
    }

    let report = {
        let source = crate::repo::open_store(&old_path, from, &config.name)?;
        if to == Storage::Kv {
            // Sized from the source to begin with (its bytes on disk, twice
            // over), and grown further if that was not enough.
            let held = std::fs::metadata(&old_path).map_or(0, |m| m.len() as usize);
            let start = held.saturating_mul(2).max(crate::kvstore::INITIAL_MAP);
            copy_into_kv(&*source, &temp, start).context("copy the repository")?;
        } else {
            let mut target = crate::repo::create_store(&temp, to, &config.name)?;
            copy(&*source, &mut *target).context("copy the repository")?;
        }
        if to == Storage::Kv {
            // The copy created the metarecords in uuid order; a reindex gives
            // the dense ids their discovery order back — their locality.
            crate::kvstore::KvStore::open(&temp)?.reindex()?;
        }
        let target = crate::repo::open_store(&temp, to, &config.name)?;
        verify(&*source, &*target).context("verify the converted store")?;
        if let Some(conn) = target.as_sqlite() {
            // Fold the write-ahead log into the file: only the file moves.
            let mode: String = conn.query_row("PRAGMA journal_mode = DELETE", [], |r| r.get(0))?;
            if mode != "delete" {
                bail!("could not fold the new store's write-ahead log (journal mode {mode})");
            }
        }
        let d = digest(&*target)?;
        (d.metarecords, d.rows, d.operations, d.revisions)
    };
    // Both stores are closed here. The new one takes its place, then the
    // config switches to it, then the old one is set aside.
    std::fs::rename(&temp, &new_path)
        .with_context(|| format!("move {} into place", temp.display()))?;
    config.storage = to;
    config.write(&metafolder).context("switch config.json to the new store")?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let old_store = internal
        .join(format!("pre-convert-{stamp}-{}", old_path.file_name().unwrap().to_string_lossy()));
    std::fs::rename(&old_path, &old_store)
        .with_context(|| format!("set {} aside", old_path.display()))?;
    if from == Storage::Sqlite {
        for suffix in ["-wal", "-shm"] {
            let side = PathBuf::from(format!("{}{suffix}", old_path.display()));
            remove(&side)?;
        }
    }
    let (metarecords, rows, operations, revisions) = report;
    Ok(Report { metarecords, rows, operations, revisions, old_store })
}
