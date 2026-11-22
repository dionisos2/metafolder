//! What a log context says without its operations being read (doc "Log context summaries"):
//! how many it holds, which log contexts it reverts, which revert it.

use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::Writer;
use metafolder_daemon::revert;
use metafolder_daemon::store::Log;
use uuid::Uuid;

mod common;
use common::TempDir;

fn open() -> (KvStore, TempDir) {
    let dir = TempDir::new("kv-log-context-marks");
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

/// A labelled write: the log context.
fn labelled(store: &mut KvStore, name: &str, f: impl FnOnce(&mut Writer)) -> i64 {
    let mut w = Writer::begin(store, Some(name.into())).unwrap();
    let log_context = w.log_context_id();
    f(&mut w);
    w.commit().unwrap();
    log_context
}

#[test]
fn a_log_context_counts_its_operations_without_reading_them() {
    fn reads_of(n: usize) -> (i64, u64) {
        let (mut store, _dir) = open();
        let uuids = records(&mut store, n);
        let ids = store.dense_ids(&uuids).unwrap();
        let log_context = labelled(&mut store, "mass", |w| {
            w.mass_add_ids("tag", s("jazz"), &ids).unwrap();
        });
        let before = store.reads();
        let count = store.log_context_op_count(log_context).unwrap();
        (count, store.reads() - before)
    }
    let ((small, small_reads), (big, big_reads)) = (reads_of(100), reads_of(3000));
    assert_eq!((small, big), (100, 3000));
    assert!(big_reads <= small_reads + 2, "{small_reads} reads, then {big_reads}");
}

#[test]
fn an_ordinary_log_context_counts_one_per_operation_and_knows_its_last() {
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 5);
    let log_context = labelled(&mut store, "edit", |w| {
        for u in &uuids {
            w.set_field(*u, "rank", Value::Int(1)).unwrap();
        }
    });
    assert_eq!(store.log_context_op_count(log_context).unwrap(), 5);
    let ops = store.log_context_ops(log_context).unwrap();
    assert_eq!(store.log_context_last_op(log_context).unwrap(), Some(ops.last().unwrap().id));
    assert_eq!(store.log_context_op_count(log_context + 1000).unwrap(), 0);
    assert_eq!(store.log_context_last_op(log_context + 1000).unwrap(), None);
}

#[test]
fn a_revert_marks_the_log_contexts_it_reverts_and_is_marked_on_them() {
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 20);
    let one = labelled(&mut store, "one", |w| {
        w.mass_add("tag", s("jazz"), &uuids).unwrap();
    });
    let two = labelled(&mut store, "two", |w| {
        w.set_field(uuids[0], "rank", Value::Int(1)).unwrap();
    });

    // A whole log context undone.
    let ops = revert::log_context_ops(&store, one).unwrap();
    let mut w = Writer::begin(&mut store, Some("undo one".into())).unwrap();
    let undo = w.log_context_id();
    revert::apply(&mut w, &ops).unwrap();
    w.note_log_context_reverts(one, true).unwrap();
    w.commit().unwrap();
    assert_eq!(store.log_context_reverts(undo).unwrap(), vec![(one, true)]);
    assert_eq!(store.log_context_reverted_by(one).unwrap(), vec![(undo, true)]);
    assert!(store.log_context_reverted_by(two).unwrap().is_empty());

    // One operation of another: only part of it, and the log context it is in.
    let ops = revert::log_context_ops(&store, two).unwrap();
    let mut w = Writer::begin(&mut store, Some("undo an op".into())).unwrap();
    let part = w.log_context_id();
    revert::apply(&mut w, &ops).unwrap();
    w.commit().unwrap();
    assert_eq!(store.log_context_reverts(part).unwrap(), vec![(two, false)]);
    assert_eq!(store.log_context_reverted_by(two).unwrap(), vec![(part, false)]);
}

#[test]
fn a_log_context_emptied_by_a_prune_keeps_its_marks_until_a_reindex() {
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 6);
    let one = labelled(&mut store, "one", |w| {
        w.set_field(uuids[0], "rank", Value::Int(1)).unwrap();
    });
    let ops = revert::log_context_ops(&store, one).unwrap();
    let mut w = Writer::begin(&mut store, Some("undo".into())).unwrap();
    let undo = w.log_context_id();
    revert::apply(&mut w, &ops).unwrap();
    w.note_log_context_reverts(one, true).unwrap();
    w.commit().unwrap();
    assert!(!store.log_context_reverted_by(one).unwrap().is_empty());
    // The undo is the newest: prune everything before it. The log contexts stay
    // (doc "Log contexts"), and so do their marks.
    let first = store.log_context_first_op(undo).unwrap().unwrap();
    metafolder_daemon::log::prune(&mut store, first).unwrap();
    assert_eq!(store.log_context_reverts(undo).unwrap(), vec![(one, true)]);
    // A reindex removes the emptied log context, and what points at it.
    store.reindex().unwrap();
    assert!(store.log_context_reverted_by(one).unwrap().is_empty(), "one is gone");
    assert_eq!(store.log_context_reverts(undo).unwrap(), vec![], "and so is what points at it");
}
