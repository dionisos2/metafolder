//! What a label says without its operations being read (doc "Log context summaries"):
//! how many it holds, which labels it reverts, which revert it.

use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::Writer;
use metafolder_daemon::revert;
use metafolder_daemon::store::Log;
use uuid::Uuid;

mod common;
use common::TempDir;

fn open() -> (KvStore, TempDir) {
    let dir = TempDir::new("kv-label-marks");
    let store = KvStore::open_unsynced(dir.path()).unwrap();
    (store, dir)
}

fn s(text: &str) -> Value {
    Value::String(text.into())
}

fn records(store: &mut KvStore, n: usize) -> Vec<Uuid> {
    let mut w = Writer::begin(store, None).unwrap();
    let out = (0..n)
        .map(|_| w.create_metarecord(vec![Field::new("tag", s("blue"))]).unwrap().uuid)
        .collect();
    w.commit().unwrap();
    out
}

/// A labelled write: the label.
fn labelled(store: &mut KvStore, name: &str, f: impl FnOnce(&mut Writer)) -> i64 {
    let mut w = Writer::begin(store, Some(name.into())).unwrap();
    let label = w.label_id();
    f(&mut w);
    w.commit().unwrap();
    label
}

#[test]
fn a_label_counts_its_operations_without_reading_them() {
    fn reads_of(n: usize) -> (i64, u64) {
        let (mut store, _dir) = open();
        let uuids = records(&mut store, n);
        let ids = store.dense_ids(&uuids).unwrap();
        let label = labelled(&mut store, "mass", |w| {
            w.mass_add_ids("tag", s("jazz"), &ids).unwrap();
        });
        let before = store.reads();
        let count = store.label_op_count(label).unwrap();
        (count, store.reads() - before)
    }
    let ((small, small_reads), (big, big_reads)) = (reads_of(100), reads_of(3000));
    assert_eq!((small, big), (100, 3000));
    assert!(big_reads <= small_reads + 2, "{small_reads} reads, then {big_reads}");
}

#[test]
fn an_ordinary_label_counts_one_per_operation_and_knows_its_last() {
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 5);
    let label = labelled(&mut store, "edit", |w| {
        for u in &uuids {
            w.set_field(*u, "rank", Value::Int(1)).unwrap();
        }
    });
    assert_eq!(store.label_op_count(label).unwrap(), 5);
    let ops = store.label_ops(label).unwrap();
    assert_eq!(store.label_last_op(label).unwrap(), Some(ops.last().unwrap().id));
    assert_eq!(store.label_op_count(label + 1000).unwrap(), 0);
    assert_eq!(store.label_last_op(label + 1000).unwrap(), None);
}

#[test]
fn a_revert_marks_the_labels_it_reverts_and_is_marked_on_them() {
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 20);
    let one = labelled(&mut store, "one", |w| {
        w.mass_add("tag", s("jazz"), &uuids).unwrap();
    });
    let two = labelled(&mut store, "two", |w| {
        w.set_field(uuids[0], "rank", Value::Int(1)).unwrap();
    });

    // A whole label undone.
    let ops = revert::label_ops(&store, one).unwrap();
    let mut w = Writer::begin(&mut store, Some("undo one".into())).unwrap();
    let undo = w.label_id();
    revert::apply(&mut w, &ops).unwrap();
    w.note_label_reverts(one, true).unwrap();
    w.commit().unwrap();
    assert_eq!(store.label_reverts(undo).unwrap(), vec![(one, true)]);
    assert_eq!(store.label_reverted_by(one).unwrap(), vec![(undo, true)]);
    assert!(store.label_reverted_by(two).unwrap().is_empty());

    // One operation of another: only part of it, and the label it is in.
    let ops = revert::label_ops(&store, two).unwrap();
    let mut w = Writer::begin(&mut store, Some("undo an op".into())).unwrap();
    let part = w.label_id();
    revert::apply(&mut w, &ops).unwrap();
    w.commit().unwrap();
    assert_eq!(store.label_reverts(part).unwrap(), vec![(two, false)]);
    assert_eq!(store.label_reverted_by(two).unwrap(), vec![(part, false)]);
}

#[test]
fn pruning_a_label_takes_its_marks_with_it() {
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 6);
    let one = labelled(&mut store, "one", |w| {
        w.set_field(uuids[0], "rank", Value::Int(1)).unwrap();
    });
    let ops = revert::label_ops(&store, one).unwrap();
    let mut w = Writer::begin(&mut store, Some("undo".into())).unwrap();
    let undo = w.label_id();
    revert::apply(&mut w, &ops).unwrap();
    w.note_label_reverts(one, true).unwrap();
    w.commit().unwrap();
    assert!(!store.label_reverted_by(one).unwrap().is_empty());
    // The undo is the newest: prune everything before it.
    let first = store.label_first_op(undo).unwrap().unwrap();
    metafolder_daemon::log::prune(&mut store, first).unwrap();
    assert!(store.label_reverted_by(one).unwrap().is_empty(), "one is gone");
    assert_eq!(store.label_reverts(undo).unwrap(), vec![], "and so is what points at it");
}
