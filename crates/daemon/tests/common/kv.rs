//! The key-value store as a third engine (spec-storage "Three engines, one
//! answer"): the tests' SQLite data copied into a `KvStore`, and queries
//! evaluated over its derived key spaces by the same evaluator the resident
//! index runs.

use metafolder_daemon::index::{Eval, PageStrategy};
use metafolder_daemon::kvstore::{KvSource, KvStore};
use metafolder_daemon::store::{Begin, Store};
use roaring::RoaringBitmap;
use uuid::Uuid;

use super::TempDir;

/// A KV store holding what `store` holds: the same metarecords, rows and row
/// ids (the log is not copied — no query reads it).
pub fn kv_mirror(store: &dyn Store) -> (KvStore, TempDir) {
    let dir = TempDir::new("kv-mirror");
    let mut kv = KvStore::open(dir.path()).unwrap();
    let txn = kv.begin_write().unwrap();
    for uuid in store.metarecords().unwrap() {
        txn.create_metarecord(uuid, store.version(uuid).unwrap().unwrap()).unwrap();
    }
    store
        .for_each_row(&mut |uuid, row| {
            txn.insert_row(uuid, &row.name, &row.value, Some(row.id))?;
            Ok(())
        })
        .unwrap();
    txn.commit().unwrap();
    (kv, dir)
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
