//! The store folds what writes left pending one list at a time, the biggest
//! first (doc "Folding in idle time"): each step is one transaction, so a
//! read or a write waits for one list, never for all of them.

use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::Writer;
use metafolder_daemon::store::Rows;
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
fn a_fold_step_folds_one_list_and_the_biggest_first() {
    let dir = TempDir::new("kv-fold-step");
    let mut store = KvStore::open_unsynced(dir.path()).unwrap();
    let uuids: Vec<Uuid> = write(&mut store, |w| {
        (0..300)
            .map(|_| w.create_metarecord(vec![Field::new("tag", s("blue"))]).unwrap().uuid)
            .collect()
    });
    // Every other record: ids no run compresses, so that each list weighs
    // what it names (the entries are small, doc "Value encoding in the store").
    let uuids: Vec<Uuid> = uuids.into_iter().step_by(2).collect();
    write(&mut store, |w| {
        // Sets: a write to many records that waits for the fold (an add to
        // a field with nothing pending goes into its posting at once).
        for v in ["jazz", "rock", "folk"] {
            w.mass_set("tag", vec![s(v)], &uuids).unwrap();
        }
        w.mass_set("mood", vec![s("calm")], &uuids).unwrap();
    });
    // Three lists: the entries of the part (the biggest), the index writes of `tag`,
    // those of `mood`.
    assert_eq!(store.pending_writes().unwrap(), 4);
    assert_eq!(store.pending_index_writes("tag").unwrap(), 3);
    assert_eq!(store.pending_index_writes("mood").unwrap(), 1);

    assert!(store.fold_step().unwrap());
    assert_eq!(store.pending_writes().unwrap(), 0, "the part first");
    assert_eq!(store.pending_index_writes("tag").unwrap(), 3);
    assert_eq!(store.pending_index_writes("mood").unwrap(), 1);

    assert!(store.fold_step().unwrap());
    assert_eq!(store.pending_index_writes("tag").unwrap(), 0, "then the longer events list");
    assert_eq!(store.pending_index_writes("mood").unwrap(), 1);

    assert!(store.fold_step().unwrap());
    assert_eq!(store.pending_index_writes("mood").unwrap(), 0);

    // Last, the history of the records the log holds (doc "Log storage").
    assert!(store.fold_step().unwrap());
    assert!(!store.fold_step().unwrap(), "nothing left");
    // What was written reads the same folded.
    for u in &uuids {
        let rows = store.rows(*u).unwrap();
        // Each set replaced the one before: the last is what is left.
        let tags: Vec<&Value> = rows.iter().filter(|r| r.name == "tag").map(|r| &r.value).collect();
        assert_eq!(tags, [&s("folk")]);
        assert!(rows.iter().any(|r| r.name == "mood" && r.value == s("calm")));
    }
}
