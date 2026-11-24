//! How worn the store is (doc "Folding in idle time"): what writes left pending
//! and the ids no metarecord holds, so that a client can say when a fold, or a
//! reindex, is worth it.

use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::Writer;
use uuid::Uuid;

mod common;
use common::TempDir;

fn s(text: &str) -> Value {
    Value::String(text.into())
}

fn write<T>(store: &mut KvStore, f: impl FnOnce(&mut Writer) -> T) -> T {
    let mut w = Writer::begin(store, None).unwrap();
    let out = f(&mut w);
    w.commit().unwrap();
    out
}

#[test]
fn the_wear_counts_what_is_pending_and_the_ids_nobody_holds() {
    let dir = TempDir::new("kv-wear");
    let mut store = KvStore::open_unsynced(dir.path()).unwrap();
    let clean = store.wear().unwrap();
    assert_eq!((clean.pending_writes, clean.index_writes), (0, 0));
    assert_eq!(clean.pending_bytes(), 0);
    assert_eq!(clean.dead_ids(), 0);

    let uuids: Vec<Uuid> = write(&mut store, |w| {
        (0..50)
            .map(|_| w.create_metarecord(vec![Field::new("tag", s("a"))]).unwrap().uuid)
            .collect()
    });
    write(&mut store, |w| {
        // Sets: what waits for the fold (an add to a field with nothing
        // pending goes into its posting at once).
        w.mass_set("tag", vec![s("b")], &uuids).unwrap();
        w.mass_set("tag", vec![s("c")], &uuids).unwrap();
    });
    let worn = store.wear().unwrap();
    assert_eq!((worn.pending_writes, worn.pending_write_parts), (2, 1));
    assert_eq!((worn.index_writes, worn.index_write_fields), (2, 1));
    assert!(worn.pending_bytes() > 0);
    assert_eq!(worn.ids_allocated, 50);

    write(&mut store, |w| w.delete_metarecord(uuids[0]).unwrap());
    assert_eq!(store.wear().unwrap().dead_ids(), 1);
    assert!(store.wear().unwrap().dead_fraction() > 0.0);

    while store.fold_step().unwrap() {}
    let folded = store.wear().unwrap();
    assert_eq!((folded.pending_writes, folded.index_writes, folded.pending_bytes()), (0, 0, 0));
    assert_eq!(folded.dead_ids(), 1, "a fold does not renumber: a reindex does");
}
