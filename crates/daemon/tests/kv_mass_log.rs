//! A write to many records is one entry in the log, whatever the number of
//! records it changed (doc "Mass operations in the log"): the operations of
//! a record stay what they were to everything that reads the log — one per
//! record, with its own id and its rows before and after — and are made up
//! on reading, from the entry.

use std::collections::BTreeSet;

use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::Writer;
use metafolder_daemon::store::Log;
use uuid::Uuid;

mod common;
use common::TempDir;

fn open() -> (KvStore, TempDir) {
    let dir = TempDir::new("kv-mass-log");
    let store = KvStore::open_unsynced(dir.path()).unwrap();
    (store, dir)
}

fn s(text: &str) -> Value {
    Value::String(text.into())
}

fn write<T>(store: &mut KvStore, f: impl FnOnce(&mut Writer) -> T) -> T {
    let mut w = Writer::begin(store, None).unwrap();
    let out = f(&mut w);
    w.commit().unwrap();
    out
}

fn records(store: &mut KvStore, n: usize) -> Vec<Uuid> {
    write(store, |w| {
        (0..n)
            .map(|i| {
                w.create_metarecord(vec![Field::new("tag", s(["blue", "green"][i % 2]))])
                    .unwrap()
                    .uuid
            })
            .collect()
    })
}

#[test]
fn a_mass_add_is_one_entry_that_reads_as_one_operation_per_record() {
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 300);
    let (ops_before, _) = store.counts().unwrap();
    let head = store.head().unwrap().unwrap();
    write(&mut store, |w| w.mass_add("tag", s("jazz"), &uuids).unwrap());
    let written = store.last_write_keys();
    assert_eq!(written.get("ops"), Some(&1), "one entry: {written:?}");
    assert_eq!(written.get("snaps"), None, "no snapshot is written: {written:?}");
    assert_eq!(written.get("ops_by_label"), Some(&1), "{written:?}");
    assert_eq!(written.get("ops_by_entity"), Some(&1), "{written:?}");
    assert_eq!(written.get("records"), None, "{written:?}");
    // …and a record's operation each, read.
    assert_eq!(store.counts().unwrap().0, ops_before + 300);
    let ops = store.ops_after(head).unwrap();
    assert_eq!(ops.len(), 300);
    assert_eq!(ops.iter().map(|o| o.entity_uuid).collect::<BTreeSet<_>>().len(), 300);
    assert_eq!(
        ops.windows(2).map(|w| w[1].id - w[0].id).collect::<BTreeSet<_>>(),
        BTreeSet::from([1])
    );
    for op in &ops {
        assert_eq!(op.op_type, "append_field");
        assert_eq!(op.field_name.as_deref(), Some("tag"));
        assert!(uuids.contains(&op.entity_uuid));
        assert!(store.snapshots(op.id, false).unwrap().is_empty());
        let after = store.snapshots(op.id, true).unwrap();
        assert_eq!(
            (after.len(), &after[0].name, &after[0].value),
            (1, &"tag".to_string(), &s("jazz"))
        );
        // Each is found by its id, alone.
        assert_eq!(store.op(op.id).unwrap().unwrap().entity_uuid, op.entity_uuid);
    }
    assert_eq!(store.head().unwrap(), Some(head + 300));
}

#[test]
fn only_the_records_that_changed_have_an_operation() {
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 40);
    write(&mut store, |w| w.mass_add("tag", s("jazz"), &uuids[..30]).unwrap());
    let head = store.head().unwrap().unwrap();
    let changed = write(&mut store, |w| w.mass_add("tag", s("jazz"), &uuids).unwrap());
    assert_eq!(changed.len(), 10);
    let ops = store.ops_after(head).unwrap();
    assert_eq!(
        ops.iter().map(|o| o.entity_uuid).collect::<BTreeSet<_>>(),
        uuids[30..].iter().copied().collect::<BTreeSet<_>>()
    );
    // Nothing changed: nothing logged.
    let head = store.head().unwrap().unwrap();
    assert!(write(&mut store, |w| w.mass_add("tag", s("jazz"), &uuids).unwrap()).is_empty());
    assert_eq!(store.head().unwrap(), Some(head));
}

#[test]
fn a_mass_remove_reads_as_a_deletion_per_record() {
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 60);
    let head = store.head().unwrap().unwrap();
    write(&mut store, |w| w.mass_remove("tag", &s("blue"), &uuids).unwrap());
    let ops = store.ops_after(head).unwrap();
    assert_eq!(ops.len(), 30);
    for op in &ops {
        assert_eq!(op.op_type, "delete_field");
        let before = store.snapshots(op.id, false).unwrap();
        assert_eq!((before.len(), &before[0].value), (1, &s("blue")));
        assert!(store.snapshots(op.id, true).unwrap().is_empty());
    }
}

/// Writes under a label of its own; the label.
fn labelled<T>(store: &mut KvStore, f: impl FnOnce(&mut Writer) -> T) -> (i64, T) {
    let mut w = Writer::begin(store, Some("mass".into())).unwrap();
    let label = w.label_id();
    let out = f(&mut w);
    w.commit().unwrap();
    (label, out)
}

fn state(store: &KvStore, uuids: &[Uuid]) -> Vec<Vec<Value>> {
    use metafolder_daemon::store::Rows;
    uuids
        .iter()
        .map(|&u| {
            let mut v: Vec<Value> =
                store.rows_named(u, "tag").unwrap().into_iter().map(|r| r.value).collect();
            v.sort_by_key(|v| format!("{v:?}"));
            v
        })
        .collect()
}

#[test]
fn undoing_a_mass_write_gives_the_records_back() {
    use metafolder_daemon::revert;
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 80);
    let start = state(&store, &uuids);
    for remove in [false, true] {
        let (label, _) = labelled(&mut store, |w| {
            if remove {
                w.mass_remove("tag", &s("blue"), &uuids).unwrap()
            } else {
                w.mass_add("tag", s("jazz"), &uuids).unwrap()
            }
        });
        assert_ne!(state(&store, &uuids), start);
        let ops = revert::label_ops(&store, label).unwrap();
        assert_eq!(ops.len(), if remove { 40 } else { 80 });
        let mut w = Writer::begin(&mut store, None).unwrap();
        revert::apply(&mut w, &ops).unwrap();
        w.commit().unwrap();
        assert_eq!(state(&store, &uuids), start, "remove: {remove}");
    }
    let diff = store.check_derived().unwrap();
    assert!(diff.is_empty(), "{diff:?}");
}

#[test]
fn a_record_s_operations_include_the_mass_ones_that_named_it() {
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 50);
    let head = store.head().unwrap().unwrap();
    write(&mut store, |w| w.mass_add("tag", s("jazz"), &uuids[..25]).unwrap());
    write(&mut store, |w| w.set_field(uuids[3], "rank", Value::Int(1)).unwrap());
    write(&mut store, |w| w.mass_remove("tag", &s("jazz"), &uuids[..10]).unwrap());
    let ops = store.entity_ops_after(uuids[3], head).unwrap();
    let kinds: Vec<&str> = ops.iter().map(|o| o.op_type.as_str()).collect();
    assert_eq!(kinds.len(), 3, "{kinds:?}");
    assert_eq!(kinds[0], "append_field");
    assert_eq!(kinds[2], "delete_field");
    assert!(ops.windows(2).all(|w| w[0].id < w[1].id));
    assert!(store.entity_ops_after(uuids[40], head).unwrap().is_empty());
    // After the first of them: only the later ones.
    assert_eq!(store.entity_ops_after(uuids[3], ops[0].id).unwrap().len(), 2);
    assert_eq!(store.entity_ops_after(uuids[3], ops[2].id).unwrap().len(), 0);
}

#[test]
fn the_log_is_read_whole_through_its_other_doors() {
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 20);
    let (label, _) = labelled(&mut store, |w| w.mass_add("tag", s("jazz"), &uuids).unwrap());
    write(&mut store, |w| w.set_field(uuids[0], "rank", Value::Int(1)).unwrap());
    let all = store.all_ops().unwrap();
    let head = store.head().unwrap().unwrap();
    assert_eq!(all.len() as i64, store.counts().unwrap().0);
    // Newest first, bounded, from a point inside the entry.
    let back = store.ops_back(head, Some(5)).unwrap();
    assert_eq!(
        back.iter().map(|o| o.id).collect::<Vec<_>>(),
        (head - 4..=head).rev().collect::<Vec<_>>()
    );
    let inside = all.iter().find(|o| o.label_id == label).unwrap().id + 5;
    let back = store.ops_back(inside, Some(3)).unwrap();
    assert_eq!(back.iter().map(|o| o.id).collect::<Vec<_>>(), [inside, inside - 1, inside - 2]);
    // After an id inside it.
    assert_eq!(store.ops_after(inside).unwrap().iter().map(|o| o.id).next(), Some(inside + 1));
    assert_eq!(store.ops_after_count(inside, 1000).unwrap(), head - inside);
    assert_eq!(store.ops_after_count(inside, 3).unwrap(), 3);
    // A label's operations, its first.
    assert_eq!(store.label_ops(label).unwrap().len(), 20);
    assert_eq!(
        store.label_first_op(label).unwrap(),
        Some(all.iter().find(|o| o.label_id == label).unwrap().id)
    );
    // The time travel lands on the entry's last id.
    let ts = store.op(inside).unwrap().unwrap().timestamp;
    assert!(store.last_op_at_or_before(ts).unwrap().unwrap() >= inside);
}

#[test]
fn pruning_cuts_an_entry_and_retention_takes_it_whole() {
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 30);
    let (label, _) = labelled(&mut store, |w| w.mass_add("tag", s("jazz"), &uuids).unwrap());
    let ops = store.label_ops(label).unwrap();
    let target = ops[10].id;
    let before = store.counts().unwrap().0;
    let (pruned, _) = metafolder_daemon::log::prune(&mut store, target).unwrap();
    assert_eq!(store.counts().unwrap().0, before - pruned as i64);
    assert_eq!(store.first_op().unwrap(), Some(target));
    let rest = store.label_ops(label).unwrap();
    assert_eq!(rest.len(), 20);
    assert_eq!(rest[0].id, target);
    assert_eq!(store.op(target).unwrap().unwrap().entity_uuid, ops[10].entity_uuid);
    assert!(store.op(target - 1).unwrap().is_none());
    // The kept ones read as before.
    assert_eq!(store.snapshots(target, true).unwrap().len(), 1);
}

#[test]
fn retention_counts_an_entry_once_and_takes_it_whole() {
    use metafolder_daemon::log::Retention;
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 30);
    let keep = Retention { watcher_ops: 0, user_ops: 5 };
    let mut w = Writer::begin_with_retention(&mut store, Some("mass".into()), keep).unwrap();
    let label = w.label_id();
    w.mass_add("tag", s("jazz"), &uuids).unwrap();
    w.commit().unwrap();
    assert_eq!(store.label_ops(label).unwrap().len(), 30);
    for i in 0..(5 + Retention::slack(5) as usize + 2) {
        let mut w = Writer::begin_with_retention(&mut store, None, keep).unwrap();
        w.set_field(uuids[0], "rank", Value::Int(i as i64)).unwrap();
        w.commit().unwrap();
    }
    assert!(store.label_ops(label).unwrap().is_empty(), "the entry went whole");
    let all = store.all_ops().unwrap();
    assert_eq!(all.len() as i64, store.counts().unwrap().0);
    assert!(all.iter().all(|o| o.label_id != label));
}
