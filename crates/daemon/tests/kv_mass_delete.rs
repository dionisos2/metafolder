//! A deletion of many records is one entry in the log (doc "Mass operations in
//! the log"): the records stay where they are until an idle fold, only the
//! universe changes, and the log reads one `delete_metarecord` operation per
//! record with the rows it had.

use std::collections::BTreeSet;

use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::Writer;
use metafolder_daemon::store::{Log, Rows};
use uuid::Uuid;

mod common;
use common::TempDir;

fn open() -> (KvStore, TempDir) {
    let dir = TempDir::new("kv-mass-delete");
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
                w.create_metarecord(vec![
                    Field::new("tag", s(["blue", "green"][i % 2])),
                    Field::new("rank", Value::Int(i as i64 % 7)),
                ])
                .unwrap()
                .uuid
            })
            .collect()
    })
}

fn delete(store: &mut KvStore, uuids: &[Uuid]) -> Vec<Uuid> {
    let ids = store.dense_ids(uuids).unwrap();
    let done = write(store, |w| w.mass_delete_ids(&ids).unwrap());
    store.uuids_of(&done).unwrap()
}

fn sound(store: &KvStore) {
    let diff = store.check_derived().unwrap();
    assert!(diff.is_empty(), "the indexes differ from the rows: {diff:#?}");
}

#[test]
fn a_mass_delete_is_one_entry_and_the_records_stay_until_the_fold() {
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 300);
    let (gone, kept) = uuids.split_at(200);
    let deleted = delete(&mut store, gone);
    assert_eq!(deleted.len(), 200);
    let written = store.last_write_keys();
    assert_eq!(written.get("ops"), Some(&1), "one entry: {written:?}");
    assert_eq!(written.get("snaps"), None, "{written:?}");
    assert_eq!(written.get("records"), None, "no record is written: {written:?}");
    assert_eq!(written.get("metarecords"), None, "{written:?}");

    for u in gone {
        assert!(!Rows::exists(&store, *u).unwrap());
        assert!(Rows::rows(&store, *u).unwrap().is_empty());
    }
    for u in kept {
        assert!(Rows::exists(&store, *u).unwrap());
    }
    assert_eq!(Rows::metarecord_count(&store).unwrap(), 100);
    assert_eq!(Rows::metarecords(&store).unwrap().len(), 100);
    sound(&store);
}

/// What the deletion reads is the same whatever the number of records.
#[test]
fn a_mass_delete_reads_nothing_per_record() {
    fn reads_of(n: usize) -> u64 {
        let (mut store, _dir) = open();
        let uuids = records(&mut store, n);
        let ids = store.dense_ids(&uuids).unwrap();
        let before = store.reads();
        let done = write(&mut store, |w| w.mass_delete_ids(&ids).unwrap());
        assert_eq!(done.len() as usize, n);
        store.reads() - before
    }
    let (small, big) = (reads_of(100), reads_of(3000));
    assert!(big <= small + 4, "{small} reads for 100 records, {big} for 3000");
}

#[test]
fn the_log_reads_a_deletion_per_record_with_the_rows_it_had() {
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 60);
    let head = store.head().unwrap().unwrap();
    delete(&mut store, &uuids[..40]);
    let ops = store.ops_after(head).unwrap();
    assert_eq!(ops.len(), 40);
    assert_eq!(
        ops.iter().map(|o| o.entity_uuid).collect::<BTreeSet<_>>(),
        uuids[..40].iter().copied().collect()
    );
    for op in &ops {
        assert_eq!(op.op_type, "delete_metarecord");
        let before = store.snapshots(op.id, false).unwrap();
        assert_eq!(before.len(), 2, "its two rows: {before:?}");
        assert!(before.iter().any(|r| r.name == "tag"));
        assert!(before.iter().any(|r| r.name == "rank"));
        assert!(store.snapshots(op.id, true).unwrap().is_empty());
    }
}

#[test]
fn a_fold_deletes_the_records_and_the_log_keeps_their_rows() {
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 100);
    let head = store.head().unwrap().unwrap();
    delete(&mut store, &uuids[..60]);
    let rows_of = |store: &KvStore| -> Vec<(Uuid, Vec<String>)> {
        store
            .ops_after(head)
            .unwrap()
            .iter()
            .map(|o| {
                let mut rows: Vec<String> = store
                    .snapshots(o.id, false)
                    .unwrap()
                    .iter()
                    .map(|r| format!("{}={:?}", r.name, r.value))
                    .collect();
                rows.sort();
                (o.entity_uuid, rows)
            })
            .collect()
    };
    let pending = rows_of(&store);
    assert_eq!(store.wear().unwrap().mass_entries, 1, "the deletion is pending");
    while store.fold_step().unwrap() {}
    assert_eq!(store.wear().unwrap().mass_entries, 0);
    assert_eq!(rows_of(&store), pending, "what the log says is what it said");
    assert_eq!(Rows::metarecord_count(&store).unwrap(), 40);
    for u in &uuids[..60] {
        assert!(!Rows::exists(&store, *u).unwrap());
    }
    sound(&store);
}

#[test]
fn a_record_created_again_before_the_fold_is_not_deleted_by_it() {
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 30);
    delete(&mut store, &uuids[..10]);
    // What a revert of the deletion does: the same uuid, its rows again.
    write(&mut store, |w| {
        w.create_metarecord_with_uuid(uuids[0], vec![Field::new("tag", s("blue"))]).unwrap();
    });
    assert!(Rows::exists(&store, uuids[0]).unwrap());
    while store.fold_step().unwrap() {}
    assert!(Rows::exists(&store, uuids[0]).unwrap(), "the fold left it");
    let rows = Rows::rows(&store, uuids[0]).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(Rows::metarecord_count(&store).unwrap(), 21);
    sound(&store);
}

#[test]
fn a_folder_that_loses_its_last_child_is_no_parent_any_more() {
    let (mut store, _dir) = open();
    let tree = |parent, name: &str| Field::new("loc", Value::TreeRef { parent, name: name.into() });
    let (root, kids) = write(&mut store, |w| {
        let root = w.create_metarecord(vec![tree(None, "")]).unwrap().uuid;
        let kids: Vec<Uuid> = (0..5)
            .map(|i| w.create_metarecord(vec![tree(Some(root), &format!("f{i}"))]).unwrap().uuid)
            .collect();
        (root, kids)
    });
    delete(&mut store, &kids[..3]);
    sound(&store);
    delete(&mut store, &kids[3..]);
    sound(&store);
    assert!(Rows::exists(&store, root).unwrap());
    while store.fold_step().unwrap() {}
    sound(&store);
}

#[test]
fn a_deletion_that_names_a_folder_and_its_children_is_still_a_deletion() {
    let (mut store, _dir) = open();
    let tree = |parent, name: &str| Field::new("loc", Value::TreeRef { parent, name: name.into() });
    let (all, root) = write(&mut store, |w| {
        let root = w.create_metarecord(vec![tree(None, "")]).unwrap().uuid;
        let dir = w.create_metarecord(vec![tree(Some(root), "d")]).unwrap().uuid;
        let a = w.create_metarecord(vec![tree(Some(dir), "a")]).unwrap().uuid;
        let b = w.create_metarecord(vec![tree(Some(dir), "b")]).unwrap().uuid;
        (vec![dir, a, b], root)
    });
    let deleted = delete(&mut store, &all);
    assert_eq!(deleted.len(), 3);
    assert!(Rows::exists(&store, root).unwrap());
    for u in &all {
        assert!(!Rows::exists(&store, *u).unwrap());
    }
    sound(&store);
}

/// Undoing the deletion gives the records back, with their rows, whether the
/// fold has written them out of the store or not.
#[test]
fn undoing_a_mass_delete_gives_the_records_back() {
    use metafolder_daemon::revert;
    for folded in [false, true] {
        let (mut store, _dir) = open();
        let uuids = records(&mut store, 80);
        let rows = |store: &KvStore, u: Uuid| {
            let mut rows: Vec<String> = Rows::rows(store, u)
                .unwrap()
                .iter()
                .map(|r| format!("{}={:?}", r.name, r.value))
                .collect();
            rows.sort();
            rows
        };
        let start: Vec<_> = uuids.iter().map(|&u| rows(&store, u)).collect();
        let mut w = Writer::begin(&mut store, Some("delete".into())).unwrap();
        let label = w.label_id();
        let ids = w.dense_ids(&uuids[..50]).unwrap();
        w.mass_delete_ids(&ids).unwrap();
        w.commit().unwrap();
        if folded {
            while store.fold_step().unwrap() {}
        }
        assert_eq!(Rows::metarecord_count(&store).unwrap(), 30);

        let ops = revert::label_ops(&store, label).unwrap();
        assert_eq!(ops.len(), 50);
        let mut w = Writer::begin(&mut store, None).unwrap();
        revert::apply(&mut w, &ops).unwrap();
        w.commit().unwrap();

        assert_eq!(Rows::metarecord_count(&store).unwrap(), 80, "folded: {folded}");
        let after: Vec<_> = uuids.iter().map(|&u| rows(&store, u)).collect();
        assert_eq!(after, start, "folded: {folded}");
        while store.fold_step().unwrap() {}
        let again: Vec<_> = uuids.iter().map(|&u| rows(&store, u)).collect();
        assert_eq!(again, start, "folded: {folded}, after a fold");
        sound(&store);
    }
}

#[test]
fn a_reindex_folds_a_pending_deletion_and_keeps_what_the_log_says() {
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 60);
    let head = store.head().unwrap().unwrap();
    delete(&mut store, &uuids[..40]);
    let says = |store: &KvStore| -> Vec<(Uuid, usize)> {
        store
            .ops_after(head)
            .unwrap()
            .iter()
            .map(|o| (o.entity_uuid, store.snapshots(o.id, false).unwrap().len()))
            .collect()
    };
    let before = says(&store);
    assert_eq!(before.len(), 40);
    store.reindex().unwrap();
    assert_eq!(says(&store), before);
    assert_eq!(store.wear().unwrap().mass_entries, 0);
    assert_eq!(Rows::metarecord_count(&store).unwrap(), 20);
    sound(&store);
}

#[test]
fn a_prune_in_the_middle_of_a_pending_deletion_leaves_the_rest_its_rows() {
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 30);
    let head = store.head().unwrap().unwrap();
    delete(&mut store, &uuids);
    let ops = store.ops_after(head).unwrap();
    let target = ops[10].id;
    metafolder_daemon::log::prune(&mut store, target).unwrap();
    while store.fold_step().unwrap() {}
    let left = store.ops_after(head).unwrap();
    assert_eq!(left.len(), 20);
    for op in &left {
        assert_eq!(store.snapshots(op.id, false).unwrap().len(), 2, "{op:?}");
    }
    sound(&store);
}
