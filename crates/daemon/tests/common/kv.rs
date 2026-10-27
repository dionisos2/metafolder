//! The key-value store the low-level tests write into, and the evaluator over
//! its derived key spaces — the serving path the oracle is held to.

use metafolder_daemon::index::{Eval, PageStrategy};
use metafolder_daemon::kvstore::{KvSource, KvStore};
use metafolder_daemon::store::Begin;
use roaring::RoaringBitmap;
use uuid::Uuid;

use super::TempDir;

/// A fresh, empty key-value store in a directory that removes itself — the
/// repository every low-level test writes into, without an `fsync` per
/// commit (nothing needs it back after a crash). Keep the directory bound as
/// long as the store: `let (mut conn, _dir) = common::kv::store();`.
pub fn store() -> (KvStore, TempDir) {
    let dir = TempDir::new("kv");
    let kv = KvStore::open_unsynced(dir.path()).unwrap();
    (kv, dir)
}

/// Reverts every operation newer than `after` (all of them for `None`), as
/// one write — the log's only way back (doc "The operation log").
pub fn revert_since(kv: &mut KvStore, after: Option<i64>) {
    use metafolder_daemon::store::Log;
    let ops = kv.ops_after(after.unwrap_or(0)).unwrap();
    let mut w = metafolder_daemon::log::Writer::begin(kv, None).unwrap();
    metafolder_daemon::revert::apply(&mut w, &ops).unwrap();
    w.commit().unwrap();
}

/// Deletes `uuid`'s rows of `field` directly, behind the `Writer`'s back —
/// the state an older daemon can leave, which no write makes.
pub fn delete_rows(kv: &mut KvStore, uuid: Uuid, field: &str) {
    let txn = kv.begin_write().unwrap();
    txn.delete_rows(uuid, Some(field)).unwrap();
    txn.commit().unwrap();
}

/// Runs `f` with an evaluator over `kv`, failing on a read error.
pub fn with_kv<R>(
    kv: &KvStore,
    strategy: PageStrategy,
    f: impl FnOnce(&Eval, &KvSource) -> R,
) -> R {
    let src = kv.source().unwrap();
    let out = f(&Eval { src: &src, strategy }, &src);
    if let Some(e) = src.take_error() {
        panic!("the KV source failed to read: {e:#}");
    }
    out
}

/// The uuids of a bitmap of `src`'s ids.
pub fn uuids(src: &KvSource, bm: &RoaringBitmap) -> Vec<Uuid> {
    use metafolder_daemon::index::Source;
    bm.iter().map(|id| src.uuid(id).expect("an id of the store")).collect()
}

/// The LMDB environment of the store in `dir`, opened raw — to damage it, or
/// to set it back to an older format. The store itself must be closed.
pub fn raw_env(dir: &std::path::Path) -> heed::Env {
    // SAFETY: the test opens the file once, with no `KvStore` on it, and
    // without WRITEMAP: nothing else maps it meanwhile.
    unsafe { heed::EnvOpenOptions::new().max_dbs(32).map_size(1 << 30).open(dir).unwrap() }
}

/// One of a store's raw tables, as a test damages or downgrades it.
pub type Raw = heed::Database<'static, heed::types::Bytes, heed::types::Bytes>;

/// The store's existing table `name`, held for the test's duration. heed
/// opens a database in a transaction of its own — so before the test's write
/// transaction, not inside it.
pub fn open_raw(env: &heed::Env, name: &str) -> Raw {
    let r = env.read_txn().unwrap();
    env.open_databases_and_commit(r, |dbo, r| -> heed::Result<_> {
        Ok(heed::CommitOrAbort::<_, ()>::Commit(dbo.open_database(r, Some(name))?))
    })
    .unwrap()
    .commit()
    .unwrap()
    .unwrap_or_else(|| panic!("no table {name}"))
}

/// [`open_raw`], creating the table if the store has none.
pub fn create_raw(env: &heed::Env, name: &str) -> Raw {
    let w = env.write_txn().unwrap();
    env.create_databases_and_commit(w, |dbo, w| -> heed::Result<_> {
        Ok(heed::CommitOrAbort::<_, ()>::Commit(dbo.create_database(w, Some(name))?))
    })
    .unwrap()
    .commit()
    .unwrap()
}
