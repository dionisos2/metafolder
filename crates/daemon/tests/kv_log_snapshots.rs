//! What the log keeps of the rows an operation touched (doc "Log storage"):
//! the rows it replaced or removed, the *before*; and, for an append, the value
//! it added, which a revert takes off. What a write left behind — its *after* —
//! is not written: nothing reads it, and a write costs a key less.

use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::Writer;
use uuid::Uuid;

mod common;
use common::TempDir;

fn s(text: &str) -> Value {
    Value::String(text.into())
}

fn record(store: &mut KvStore) -> Uuid {
    let mut w = Writer::begin(store, None).unwrap();
    let uuid = w
        .create_metarecord(vec![Field::new("a", s("one")), Field::new("tag", s("x"))])
        .unwrap()
        .uuid;
    w.commit().unwrap();
    uuid
}

/// The number of snapshot keys the last write put.
fn snapshot_keys(store: &KvStore) -> u64 {
    store.last_write_keys().get("snaps").copied().unwrap_or(0)
}

#[test]
fn a_replacement_writes_the_rows_it_replaced_and_not_the_ones_it_left() {
    let dir = TempDir::new("kv-log-snapshots");
    let mut store = KvStore::open_unsynced(dir.path()).unwrap();
    let uuid = record(&mut store);
    let mut w = Writer::begin(&mut store, None).unwrap();
    w.set_field(uuid, "a", s("two")).unwrap();
    w.commit().unwrap();
    assert_eq!(snapshot_keys(&store), 1, "the one row it replaced");
}

#[test]
fn a_creation_writes_no_snapshot() {
    let dir = TempDir::new("kv-log-snapshots-create");
    let mut store = KvStore::open_unsynced(dir.path()).unwrap();
    record(&mut store);
    assert_eq!(snapshot_keys(&store), 0, "nothing was there before");
}

#[test]
fn an_append_keeps_the_value_it_added_and_a_removal_the_rows_it_took() {
    let dir = TempDir::new("kv-log-snapshots-append");
    let mut store = KvStore::open_unsynced(dir.path()).unwrap();
    let uuid = record(&mut store);
    let mut w = Writer::begin(&mut store, None).unwrap();
    w.append_field(uuid, "tag", s("y")).unwrap();
    w.commit().unwrap();
    assert_eq!(snapshot_keys(&store), 1, "the value added");
    let mut w = Writer::begin(&mut store, None).unwrap();
    w.delete_fields_valued(uuid, "tag", &s("y")).unwrap();
    w.commit().unwrap();
    assert_eq!(snapshot_keys(&store), 1, "the row removed");
}

/// Undo, then redo: the redo writes the cell as the first operation left it,
/// so the first operation did not "change since" — the log keeps no state it
/// left, and the revert chain says it (doc "Reverting onto the current state").
#[test]
fn a_redo_restores_what_an_operation_left_and_changes_nothing_since() {
    use metafolder_daemon::revert::{self, Changed};
    let dir = TempDir::new("kv-log-snapshots-redo");
    let mut store = KvStore::open_unsynced(dir.path()).unwrap();
    let uuid = record(&mut store);
    let mut w = Writer::begin(&mut store, Some("set".into())).unwrap();
    let label = w.label_id();
    w.set_field(uuid, "a", s("two")).unwrap();
    w.commit().unwrap();
    let set = revert::label_ops(&store, label).unwrap();
    let changed = |store: &KvStore, op: &metafolder_daemon::log::OpRow| {
        matches!(revert::changed_since(store, op).unwrap(), Changed::No)
    };
    assert!(changed(&store, &set[0]), "nothing wrote the cell since");

    // The undo: the cell is back as it was, the operation's write is gone.
    let mut w = Writer::begin(&mut store, Some("undo".into())).unwrap();
    let undo_label = w.label_id();
    revert::apply(&mut w, &set).unwrap();
    w.commit().unwrap();
    assert!(!changed(&store, &set[0]), "the undo wrote the cell since");

    // The redo reverts the undo: the cell holds what the operation left.
    let undone = revert::label_ops(&store, undo_label).unwrap();
    let mut w = Writer::begin(&mut store, Some("redo".into())).unwrap();
    revert::apply(&mut w, &undone).unwrap();
    w.commit().unwrap();
    assert!(changed(&store, &set[0]), "the redo put the cell back as it was left");
}
