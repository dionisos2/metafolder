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
    assert_eq!(written.get("log_entries"), Some(&1), "one entry: {written:?}");
    assert_eq!(written.get("log_snaps"), None, "no snapshot is written: {written:?}");
    assert_eq!(written.get("log_entries_by_context"), Some(&1), "{written:?}");
    assert_eq!(written.get("log_entries_by_metarecord"), Some(&1), "{written:?}");
    assert_eq!(written.get("metarecords"), None, "{written:?}");
    // The log is counted by entry (doc "Log storage"); it reads as a record's
    // operation each.
    assert_eq!(store.counts().unwrap().0, ops_before + 1);
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
        assert!(store.snapshots(op.id).unwrap().is_empty());
        let after = store.added(op.id).unwrap();
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
        let before = store.snapshots(op.id).unwrap();
        assert_eq!((before.len(), &before[0].value), (1, &s("blue")));
        assert!(store.added(op.id).unwrap().is_empty());
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
    assert_eq!(all.len(), 20 + 20 + 1, "the creations, the entry's twenty, the set");
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
    assert!(pruned >= 10);
    // Counted by entry: the cut entry stays one.
    assert!(store.counts().unwrap().0 <= before);
    assert_eq!(store.first_op().unwrap(), Some(target));
    let rest = store.label_ops(label).unwrap();
    assert_eq!(rest.len(), 20);
    assert_eq!(rest[0].id, target);
    assert_eq!(store.op(target).unwrap().unwrap().entity_uuid, ops[10].entity_uuid);
    assert!(store.op(target - 1).unwrap().is_none());
    // The kept ones read as before.
    assert_eq!(store.added(target).unwrap().len(), 1);
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
    assert_eq!(all.len() as i64, store.counts().unwrap().0, "one operation per entry left");
    assert!(all.iter().all(|o| o.label_id != label));
}

/// What a write by id reads is the same whatever the number of records.
#[test]
fn a_mass_write_by_id_reads_nothing_per_record() {
    fn reads_of(n: usize, remove: bool) -> u64 {
        let (mut store, _dir) = open();
        let uuids = records(&mut store, n);
        let ids = store.dense_ids(&uuids).unwrap();
        let before = store.reads();
        let changed = write(&mut store, |w| {
            if remove {
                w.mass_remove_ids("tag", &s("blue"), &ids).unwrap()
            } else {
                w.mass_add_ids("tag", s("jazz"), &ids).unwrap()
            }
        });
        assert_eq!(changed.len() as usize, if remove { n / 2 } else { n });
        store.reads() - before
    }
    for remove in [false, true] {
        let (small, big) = (reads_of(100, remove), reads_of(3000, remove));
        assert!(big <= small + 4, "remove {remove}: {small} reads for 100 records, {big} for 3000");
    }
}

#[test]
fn a_reindex_keeps_what_a_stub_names() {
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 60);
    let ids = store.dense_ids(&uuids[..40]).unwrap();
    let head = store.head().unwrap().unwrap();
    // One record is deleted after the write, as the ids still name it.
    write(&mut store, |w| w.mass_add_ids("tag", s("jazz"), &ids).unwrap());
    write(&mut store, |w| w.delete_metarecord(uuids[5]).unwrap());
    let before: Vec<_> = store
        .ops_after(head)
        .unwrap()
        .into_iter()
        .filter(|o| o.field_name.as_deref() == Some("tag"))
        .map(|o| (o.id, o.entity_uuid))
        .collect();
    assert_eq!(before.len(), 40);
    store.reindex().unwrap();
    let after: Vec<_> = store
        .ops_after(head)
        .unwrap()
        .into_iter()
        .filter(|o| o.field_name.as_deref() == Some("tag"))
        .map(|o| (o.id, o.entity_uuid))
        .collect();
    assert_eq!(before, after, "the same operations, on the same records, after the ids changed");
    assert_eq!(store.entity_ops_after(uuids[7], head).unwrap().len(), 1);
}

fn tags_of(store: &KvStore, uuid: Uuid) -> Vec<Value> {
    use metafolder_daemon::store::Rows;
    let mut v: Vec<Value> =
        store.rows_named(uuid, "tag").unwrap().into_iter().map(|r| r.value).collect();
    v.sort_by_key(|v| format!("{v:?}"));
    v
}

#[test]
fn a_mass_replace_is_one_entry_that_reads_with_what_it_replaced() {
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 40);
    let head = store.head().unwrap().unwrap();
    let ids = store.dense_ids(&uuids).unwrap();
    write(&mut store, |w| w.mass_set_ids("tag", vec![s("x"), s("y")], &ids).unwrap());
    let written = store.last_write_keys();
    assert_eq!(written.get("log_entries"), Some(&1), "{written:?}");
    assert_eq!(written.get("log_snaps"), None, "{written:?}");
    assert_eq!(written.get("metarecords"), None, "{written:?}");
    let ops = store.ops_after(head).unwrap();
    assert_eq!(ops.len(), 40);
    for op in &ops {
        assert_eq!(op.op_type, "set_field");
        let i = uuids.iter().position(|u| *u == op.entity_uuid).unwrap();
        let before = store.snapshots(op.id).unwrap();
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].value, s(["blue", "green"][i % 2]));
    }
    let diff = store.check_derived().unwrap();
    assert!(diff.is_empty(), "{diff:?}");
}

#[test]
fn a_mass_unset_logs_only_the_records_that_held_the_field() {
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 30);
    // Ten of them have none.
    write(&mut store, |w| w.mass_unset("tag", &uuids[..10]).unwrap());
    let head = store.head().unwrap().unwrap();
    let ids = store.dense_ids(&uuids).unwrap();
    let changed = write(&mut store, |w| w.mass_unset_ids("tag", &ids).unwrap());
    assert_eq!(changed.len(), 20);
    let ops = store.ops_after(head).unwrap();
    assert_eq!(ops.len(), 20);
    for op in &ops {
        assert_eq!(op.op_type, "delete_field");
        assert_eq!(store.snapshots(op.id).unwrap().len(), 1);
        assert!(store.added(op.id).unwrap().is_empty());
    }
    assert!(uuids.iter().all(|&u| tags_of(&store, u).is_empty()));
    let diff = store.check_derived().unwrap();
    assert!(diff.is_empty(), "{diff:?}");
}

#[test]
fn what_a_replacement_replaced_survives_the_writes_that_came_after() {
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 30);
    let ids = store.dense_ids(&uuids).unwrap();
    let head = store.head().unwrap().unwrap();
    let start = state(&store, &uuids);
    let (label, _) = labelled(&mut store, |w| {
        w.mass_set_ids("tag", vec![s("x")], &ids).unwrap();
    });
    // After it: by hand, in bulk, another replacement, a deletion.
    write(&mut store, |w| w.append_field(uuids[1], "tag", s("hand")).unwrap());
    write(&mut store, |w| w.mass_add("tag", s("later"), &uuids[..15]).unwrap());
    write(&mut store, |w| w.mass_set("tag", vec![s("z")], &uuids[..5]).unwrap());
    write(&mut store, |w| w.delete_metarecord(uuids[2]).unwrap());
    let ops = store.label_ops(label).unwrap();
    assert_eq!(ops.len(), 30);
    for op in &ops {
        let i = uuids.iter().position(|u| *u == op.entity_uuid).unwrap();
        let before = store.snapshots(op.id).unwrap();
        assert_eq!(before.len(), 1, "record {i}");
        assert_eq!(before[0].value, s(["blue", "green"][i % 2]), "record {i}");
    }
    // Undoing the replacement alone, on what is there now.
    let live: Vec<_> = ops.into_iter().filter(|o| o.entity_uuid != uuids[2]).collect();
    let mut w = Writer::begin(&mut store, None).unwrap();
    metafolder_daemon::revert::apply(&mut w, &live).unwrap();
    w.commit().unwrap();
    for (i, &u) in uuids.iter().enumerate().filter(|(i, _)| *i != 2 && *i >= 15) {
        assert_eq!(tags_of(&store, u), [s(["blue", "green"][i % 2])], "record {i}");
    }
    let _ = (head, start);
    let diff = store.check_derived().unwrap();
    assert!(diff.is_empty(), "{diff:?}");
}

#[test]
fn a_mass_replace_by_id_reads_nothing_per_record() {
    fn reads_of(n: usize, unset: bool) -> u64 {
        let (mut store, _dir) = open();
        let uuids = records(&mut store, n);
        let ids = store.dense_ids(&uuids).unwrap();
        let before = store.reads();
        write(&mut store, |w| {
            if unset {
                w.mass_unset_ids("tag", &ids).unwrap()
            } else {
                w.mass_set_ids("tag", vec![s("x"), s("y")], &ids).unwrap()
            }
        });
        store.reads() - before
    }
    for unset in [false, true] {
        let (small, big) = (reads_of(100, unset), reads_of(3000, unset));
        assert!(big <= small + 4, "unset {unset}: {small} reads for 100 records, {big} for 3000");
    }
}

#[test]
fn a_replacement_knows_what_the_same_write_put_before_it() {
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 200);
    let ids = store.dense_ids(&uuids[..120]).unwrap();
    let head = store.head().unwrap().unwrap();
    let (label, _) = labelled(&mut store, |w| {
        // A posting made in this write, one record changed by hand, a value
        // that is nothing, then the replacement.
        w.mass_add("tag", s("jazz"), &uuids).unwrap();
        w.append_field(uuids[3], "tag", s("hand")).unwrap();
        w.mass_set("tag", vec![Value::Nothing], &uuids[100..110]).unwrap();
        w.mass_set_ids("tag", vec![s("z")], &ids).unwrap();
    });
    let _ = head;
    let ops = store.label_ops(label).unwrap();
    let set_ops: Vec<_> = ops.iter().filter(|o| o.op_type == "set_field").collect();
    // The log keeps no state a write left: the replacement by "z" is the
    // newest of the replacements, after the ten that wrote nothing.
    assert_eq!(set_ops.len(), 130);
    let replacement: Vec<_> = set_ops.iter().rev().take(120).collect();
    for op in replacement {
        let i = uuids.iter().position(|u| *u == op.entity_uuid).unwrap();
        let mut before: Vec<Value> =
            store.snapshots(op.id).unwrap().into_iter().map(|r| r.value).collect();
        before.sort_by_key(|v| format!("{v:?}"));
        let mut want = if (100..110).contains(&i) {
            vec![Value::Nothing]
        } else {
            vec![s(["blue", "green"][i % 2]), s("jazz")]
        };
        if i == 3 && !(100..110).contains(&i) {
            want.push(s("hand"));
        }
        want.sort_by_key(|v| format!("{v:?}"));
        assert_eq!(before, want, "record {i}");
    }
    let diff = store.check_derived().unwrap();
    assert!(diff.is_empty(), "{diff:?}");
}

/// A prune in the middle of a replacement, or of a stub a reindex turned into
/// uuids, leaves each operation kept with its own record and what it replaced.
#[test]
fn pruning_into_a_replacement_keeps_each_operation_what_it_replaced() {
    for reindex in [false, true] {
        let (mut store, _dir) = open();
        let uuids = records(&mut store, 40);
        let head = store.head().unwrap().unwrap();
        write(&mut store, |w| w.mass_set("tag", vec![s("red")], &uuids).unwrap());
        if reindex {
            store.reindex().unwrap();
        }
        let ops = store.ops_after(head).unwrap();
        assert_eq!(ops.len(), 40);
        let before: Vec<_> =
            ops.iter().map(|o| (o.entity_uuid, store.snapshots(o.id).unwrap())).collect();
        metafolder_daemon::log::prune(&mut store, ops[13].id).unwrap();
        let left = store.ops_after(head).unwrap();
        assert_eq!(left.len(), 27, "reindex: {reindex}");
        for (op, (uuid, rows)) in left.iter().zip(&before[13..]) {
            assert_eq!(op.entity_uuid, *uuid, "reindex: {reindex}");
            assert_eq!(&store.snapshots(op.id).unwrap(), rows, "reindex: {reindex}, {op:?}");
            assert_eq!(store.added(op.id).unwrap().len(), 0);
        }
    }
}

/// A replacement takes off long texts as well as short ones (their keys are
/// hashed, and they are filed apart): the log says it took them, and an undo
/// gives them back.
#[test]
fn a_replacement_of_long_texts_is_undone_whole() {
    use metafolder_daemon::revert;
    use metafolder_daemon::store::Rows;
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 30);
    let long = "x".repeat(2000);
    write(&mut store, |w| {
        for (i, &u) in uuids.iter().enumerate() {
            let text = if i % 3 == 0 { format!("{long}{i}") } else { format!("short {i}") };
            w.append_field(u, "note", s(&text)).unwrap();
        }
    });
    let notes = |store: &KvStore| -> Vec<Vec<Value>> {
        uuids
            .iter()
            .map(|&u| {
                Rows::rows(store, u)
                    .unwrap()
                    .into_iter()
                    .filter(|r| r.name == "note")
                    .map(|r| r.value)
                    .collect()
            })
            .collect()
    };
    let start = notes(&store);
    let (label, _) = labelled(&mut store, |w| w.mass_set("note", vec![s("same")], &uuids).unwrap());
    let ops = revert::label_ops(&store, label).unwrap();
    assert_eq!(ops.len(), 30);
    for (op, before) in ops.iter().zip(&start) {
        let held: Vec<Value> =
            store.snapshots(op.id).unwrap().into_iter().map(|r| r.value).collect();
        assert_eq!(&held, before, "{op:?}");
    }
    let mut w = Writer::begin(&mut store, None).unwrap();
    revert::apply(&mut w, &ops).unwrap();
    w.commit().unwrap();
    assert_eq!(notes(&store), start);
    store.fold().unwrap();
    assert!(store.check_derived().unwrap().is_empty());
}

/// The history of a record reads each newer stub once for all the records it is
/// asked about, not once per record: what a revert plan asks of each of its
/// operations (`changed_since` reads what came after it on its record), over a
/// mass write followed by others.
#[test]
fn the_stubs_after_an_operation_are_read_once_for_a_whole_plan() {
    use metafolder_daemon::revert;
    fn plan_reads(later: usize) -> u64 {
        let (mut store, _dir) = open();
        let uuids = records(&mut store, 400);
        let ids = store.dense_ids(&uuids).unwrap();
        let (label, _) = labelled(&mut store, |w| w.mass_add_ids("tag", s("x"), &ids).unwrap());
        for j in 0..later {
            write(&mut store, |w| w.mass_add_ids("tag", s(&format!("y{j}")), &ids).unwrap());
        }
        // What the writes left pending is not what is measured.
        store.fold().unwrap();
        let ops = revert::label_ops(&store, label).unwrap();
        assert_eq!(ops.len(), 400);
        let before = store.reads();
        for op in &ops {
            let after = store.entity_ops_after(op.entity_uuid, op.id).unwrap();
            assert_eq!(after.len(), later, "{op:?}");
            assert!(after.iter().all(|o| o.entity_uuid == op.entity_uuid));
        }
        store.reads() - before
    }
    let (none, some) = (plan_reads(0), plan_reads(20));
    // Each of the 20 stubs read about once, not 400 times.
    assert!(some <= none + 20 * 4, "{none} keys with no stub after, {some} with 20");
}
