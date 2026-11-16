//! A deletion of many records is one entry in the log (doc "Mass operations in
//! the log"): the records stay where they are until an idle fold, only the
//! universe changes, and the log reads one `delete_metarecord` operation per
//! record with the rows it had.

use std::collections::BTreeSet;

use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::Writer;
use metafolder_daemon::store::{Begin, Log, Rows};
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

fn tree(parent: Option<Uuid>, name: &str) -> Field {
    Field::new(
        "loc",
        Value::TreeRef { parent, name: metafolder_core::metarecord::TreeName::from(name) },
    )
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

/// Undoing a deletion still pending is the universe given back: the records
/// never left the store, so they keep their dense ids, nothing is written to
/// their rows or their index entries, and the log gets one operation per
/// record reverting the deletion's.
#[test]
fn undoing_a_pending_mass_delete_only_gives_the_universe_back() {
    use metafolder_daemon::revert;
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 80);
    let ids_before = store.dense_ids(&uuids).unwrap();
    let mut w = Writer::begin(&mut store, Some("delete".into())).unwrap();
    let label = w.label_id();
    let ids = w.dense_ids(&uuids[..50]).unwrap();
    w.mass_delete_ids(&ids).unwrap();
    w.commit().unwrap();
    assert_eq!(Rows::metarecord_count(&store).unwrap(), 30);

    let ops = revert::label_ops(&store, label).unwrap();
    let mut w = Writer::begin(&mut store, None).unwrap();
    revert::apply(&mut w, &ops).unwrap();
    w.commit().unwrap();

    let written = store.last_write_keys();
    for table in ["records", "ids", "uuids", "metarecords", "parts", "kids", "forest", "snaps"] {
        assert_eq!(written.get(table), None, "{table} untouched: {written:?}");
    }
    assert_eq!(Rows::metarecord_count(&store).unwrap(), 80);
    assert_eq!(store.dense_ids(&uuids).unwrap(), ids_before, "the records keep their ids");
    let undone = store.ops_after(ops.last().unwrap().id).unwrap();
    assert_eq!(undone.len(), 50, "one operation per record");
    for (op, reverted) in undone.iter().zip(ops.iter().rev()) {
        assert_eq!(op.op_type, "create_metarecord");
        assert_eq!(op.reverts_op_id, Some(reverted.id));
        assert_eq!(op.entity_uuid, reverted.entity_uuid);
    }
    // Nothing of the deletion is left to fold, and the rows are the ones it took.
    while store.fold_step().unwrap() {}
    assert_eq!(Rows::metarecord_count(&store).unwrap(), 80);
    for &u in &uuids {
        assert_eq!(Rows::rows(&store, u).unwrap().len(), 2);
    }
    sound(&store);
    assert!(store.check().unwrap().is_empty());
}

/// Undoing a deletion the fold already wrote out of the store puts the records
/// back in one pass: their dense ids are still theirs, so the indexes that
/// name them are right as they stand, and no id is made, per record, for a
/// uuid that had one.
#[test]
fn undoing_a_folded_mass_delete_keeps_the_dense_ids() {
    use metafolder_daemon::revert;
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 80);
    let ids_before = store.dense_ids(&uuids).unwrap();
    let mut w = Writer::begin(&mut store, Some("delete".into())).unwrap();
    let label = w.label_id();
    let ids = w.dense_ids(&uuids[..50]).unwrap();
    w.mass_delete_ids(&ids).unwrap();
    w.commit().unwrap();
    while store.fold_step().unwrap() {}
    assert_eq!(Rows::metarecord_count(&store).unwrap(), 30);

    let ops = revert::label_ops(&store, label).unwrap();
    let mut w = Writer::begin(&mut store, None).unwrap();
    revert::apply(&mut w, &ops).unwrap();
    w.commit().unwrap();

    let written = store.last_write_keys();
    for table in ["ids", "uuids", "kids", "forest"] {
        assert_eq!(written.get(table), None, "{table} untouched: {written:?}");
    }
    assert_eq!(Rows::metarecord_count(&store).unwrap(), 80);
    assert_eq!(store.dense_ids(&uuids).unwrap(), ids_before, "the records keep their ids");
    let undone = store.ops_after(ops.last().unwrap().id).unwrap();
    assert_eq!(undone.len(), 50, "one operation per record");
    for (op, reverted) in undone.iter().zip(ops.iter().rev()) {
        assert_eq!(op.op_type, "create_metarecord");
        assert_eq!(op.reverts_op_id, Some(reverted.id));
    }
    for &u in &uuids {
        assert_eq!(Rows::rows(&store, u).unwrap().len(), 2);
    }
    // The rows the deletion took are still the log's to say.
    assert!(store.before_known(ops[0].id).unwrap());
    while store.fold_step().unwrap() {}
    assert_eq!(Rows::metarecord_count(&store).unwrap(), 80);
    sound(&store);
    assert!(store.check().unwrap().is_empty());
}

/// A reindex numbers the ids again: the ids a folded deletion named mean
/// nothing after it, and the records come back one by one, whole.
#[test]
fn undoing_a_folded_mass_delete_after_a_reindex_gives_the_records_back_one_by_one() {
    use metafolder_daemon::revert;
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 60);
    let mut w = Writer::begin(&mut store, Some("delete".into())).unwrap();
    let label = w.label_id();
    let ids = w.dense_ids(&uuids[..40]).unwrap();
    w.mass_delete_ids(&ids).unwrap();
    w.commit().unwrap();
    while store.fold_step().unwrap() {}
    store.reindex().unwrap();

    let ops = revert::label_ops(&store, label).unwrap();
    let mut w = Writer::begin(&mut store, None).unwrap();
    revert::apply(&mut w, &ops).unwrap();
    w.commit().unwrap();

    assert_eq!(Rows::metarecord_count(&store).unwrap(), 60);
    for &u in &uuids {
        assert_eq!(Rows::rows(&store, u).unwrap().len(), 2);
    }
    sound(&store);
}

/// A position a live record took since the fold is not given back: the
/// records are put back one at a time, and the one in conflict is refused as
/// any revert onto a taken position is.
#[test]
fn undoing_a_folded_mass_delete_whose_position_was_taken_is_not_done_in_bulk() {
    use metafolder_daemon::revert;
    let (mut store, _dir) = open();
    let (root, kids) = write(&mut store, |w| {
        let root = w.create_metarecord(vec![tree(None, "root")]).unwrap().uuid;
        let kids: Vec<Uuid> = (0..30)
            .map(|i| w.create_metarecord(vec![tree(Some(root), &format!("k{i}"))]).unwrap().uuid)
            .collect();
        (root, kids)
    });
    let mut w = Writer::begin(&mut store, Some("delete".into())).unwrap();
    let label = w.label_id();
    let ids = w.dense_ids(&kids).unwrap();
    w.mass_delete_ids(&ids).unwrap();
    w.commit().unwrap();
    while store.fold_step().unwrap() {}
    let taker =
        write(&mut store, |w| w.create_metarecord(vec![tree(Some(root), "k3")]).unwrap().uuid);

    let ops = revert::label_ops(&store, label).unwrap();
    let mut w = Writer::begin(&mut store, None).unwrap();
    assert!(revert::apply(&mut w, &ops).is_err(), "k3 is taken");
    drop(w);
    assert!(Rows::exists(&store, taker).unwrap());
    assert_eq!(Rows::metarecord_count(&store).unwrap(), 2);
    sound(&store);
}

/// The giving back leaves the indexes as a rebuild would: the type counts of
/// a field whose every holder was deleted, the parents of a folder's children,
/// and what is left pending of the deletion still folds.
#[test]
fn giving_a_pending_deletion_back_keeps_the_counts_the_parents_and_the_rest_pending() {
    use metafolder_daemon::revert;
    let (mut store, _dir) = open();
    let (root, folder, kids) = write(&mut store, |w| {
        let root = w.create_metarecord(vec![tree(None, "")]).unwrap().uuid;
        let folder = w.create_metarecord(vec![tree(Some(root), "dir")]).unwrap().uuid;
        let kids: Vec<Uuid> = (0..40)
            .map(|i| {
                w.create_metarecord(vec![
                    tree(Some(folder), &format!("k{i}")),
                    Field::new("solo", Value::Int(i)),
                ])
                .unwrap()
                .uuid
            })
            .collect();
        (root, folder, kids)
    });
    let mut all = kids.clone();
    all.push(folder);
    let mut w = Writer::begin(&mut store, Some("delete".into())).unwrap();
    let label = w.label_id();
    let ids = w.dense_ids(&all).unwrap();
    w.mass_delete_ids(&ids).unwrap();
    w.commit().unwrap();
    assert_eq!(Rows::metarecord_count(&store).unwrap(), 1);
    sound(&store);

    // Give back only some of it: the first thirty operations of the stub (the
    // folder, whose id is the lowest, and twenty-nine of its children).
    let ops = revert::label_ops(&store, label).unwrap();
    assert_eq!(ops.len(), 41);
    let some: Vec<_> = ops[..30].to_vec();
    let mut w = Writer::begin(&mut store, None).unwrap();
    revert::apply(&mut w, &some).unwrap();
    w.commit().unwrap();
    assert_eq!(Rows::metarecord_count(&store).unwrap(), 31);
    sound(&store);
    // The rest is still pending, and folds without touching what came back.
    while store.fold_step().unwrap() {}
    assert_eq!(Rows::metarecord_count(&store).unwrap(), 31);
    for u in &kids[..29] {
        assert_eq!(Rows::rows(&store, *u).unwrap().len(), 2);
    }
    for u in &kids[29..] {
        assert!(!Rows::exists(&store, *u).unwrap());
    }
    sound(&store);
    assert!(store.check().unwrap().is_empty());
    assert!(Rows::exists(&store, root).unwrap());
    // The deletion's rows are still the log's to say, folded or given back.
    for op in &ops[30..] {
        assert_eq!(store.snapshots(op.id, false).unwrap().len(), 2, "{op:?}");
    }
    for op in &ops[..30] {
        assert!(store.before_known(op.id).unwrap());
        assert!(!store.snapshots(op.id, false).unwrap().is_empty(), "{op:?}");
    }
}

/// What a deletion deleted is known while nothing came after it but its own
/// revert: a record given back and written to since has lost the rows the
/// deletion took, and the log says it does not know them rather than read the
/// rows of today.
#[test]
fn the_rows_a_deletion_took_are_unknown_once_the_record_was_written_to() {
    use metafolder_daemon::revert;
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 40);
    let mut w = Writer::begin(&mut store, Some("delete".into())).unwrap();
    let label = w.label_id();
    let ids = w.dense_ids(&uuids).unwrap();
    w.mass_delete_ids(&ids).unwrap();
    w.commit().unwrap();
    let ops = revert::label_ops(&store, label).unwrap();
    assert!(ops.iter().all(|o| store.before_known(o.id).unwrap()), "known while pending");

    let mut w = Writer::begin(&mut store, None).unwrap();
    revert::apply(&mut w, &ops).unwrap();
    w.commit().unwrap();
    assert!(ops.iter().all(|o| store.before_known(o.id).unwrap()), "known after its own revert");

    let touched = uuids[7];
    write(&mut store, |w| {
        w.set_field(touched, "tag", s("purple")).unwrap();
    });
    for op in &ops {
        let known = store.before_known(op.id).unwrap();
        assert_eq!(known, op.entity_uuid != touched, "{op:?}");
    }
    // Reverting a deletion whose rows are unknown is refused, not guessed.
    write(&mut store, |w| w.delete_metarecord(touched).unwrap());
    let unknown: Vec<_> = ops.iter().filter(|o| o.entity_uuid == touched).cloned().collect();
    let mut w = Writer::begin(&mut store, None).unwrap();
    let err = revert::apply(&mut w, &unknown).unwrap_err().to_string();
    assert!(err.contains("no longer known"), "{err}");
}

/// A field whose every holder is deleted has no type left to count: giving
/// them back is then done one by one, and the field is whole again.
#[test]
fn a_field_every_holder_of_which_was_deleted_comes_back_with_its_type() {
    use metafolder_daemon::revert;
    let (mut store, _dir) = open();
    let uuids = write(&mut store, |w| {
        (0..20)
            .map(|i| w.create_metarecord(vec![Field::new("solo", Value::Int(i))]).unwrap().uuid)
            .collect::<Vec<_>>()
    });
    let mut w = Writer::begin(&mut store, Some("delete".into())).unwrap();
    let label = w.label_id();
    let ids = w.dense_ids(&uuids).unwrap();
    w.mass_delete_ids(&ids).unwrap();
    w.commit().unwrap();
    let ops = revert::label_ops(&store, label).unwrap();
    let mut w = Writer::begin(&mut store, None).unwrap();
    revert::apply(&mut w, &ops).unwrap();
    w.commit().unwrap();
    assert_eq!(Rows::metarecord_count(&store).unwrap(), 20);
    sound(&store);
    assert!(store.check().unwrap().is_empty());
}

/// Anything written since the deletion keeps the old way, which is slower
/// and as right.
#[test]
fn a_write_since_the_deletion_gives_the_records_back_one_by_one() {
    use metafolder_daemon::revert;
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 60);
    let mut w = Writer::begin(&mut store, Some("delete".into())).unwrap();
    let label = w.label_id();
    let ids = w.dense_ids(&uuids[..40]).unwrap();
    w.mass_delete_ids(&ids).unwrap();
    w.commit().unwrap();
    write(&mut store, |w| {
        w.create_metarecord(vec![Field::new("other", s("x"))]).unwrap();
    });
    let ops = revert::label_ops(&store, label).unwrap();
    let mut w = Writer::begin(&mut store, None).unwrap();
    revert::apply(&mut w, &ops).unwrap();
    w.commit().unwrap();
    assert_eq!(Rows::metarecord_count(&store).unwrap(), 61);
    sound(&store);
    assert!(store.check().unwrap().is_empty());
}

/// A deletion given back can be done again: undoing the undo.
#[test]
fn undoing_the_giving_back_deletes_the_records_again() {
    use metafolder_daemon::revert;
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 60);
    let mut w = Writer::begin(&mut store, Some("delete".into())).unwrap();
    let label = w.label_id();
    let ids = w.dense_ids(&uuids[..40]).unwrap();
    w.mass_delete_ids(&ids).unwrap();
    w.commit().unwrap();
    let ops = revert::label_ops(&store, label).unwrap();
    let mut w = Writer::begin(&mut store, None).unwrap();
    let back = w.label_id();
    revert::apply(&mut w, &ops).unwrap();
    w.commit().unwrap();
    assert_eq!(Rows::metarecord_count(&store).unwrap(), 60);

    let ops = revert::label_ops(&store, back).unwrap();
    let mut w = Writer::begin(&mut store, None).unwrap();
    revert::apply(&mut w, &ops).unwrap();
    w.commit().unwrap();
    assert_eq!(Rows::metarecord_count(&store).unwrap(), 20);
    sound(&store);
    while store.fold_step().unwrap() {}
    assert_eq!(Rows::metarecord_count(&store).unwrap(), 20);
    sound(&store);
    assert!(store.check().unwrap().is_empty());
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

/// What an earlier write of the same transaction changed in the sets — records
/// created, a value added — is read as it stands, in the order of the writes.
#[test]
fn a_deletion_after_other_writes_of_the_transaction_is_still_one_entry() {
    let (mut store, _dir) = open();
    let tree = |parent, name: &str| Field::new("loc", Value::TreeRef { parent, name: name.into() });
    let root = write(&mut store, |w| w.create_metarecord(vec![tree(None, "")]).unwrap().uuid);
    let kids = {
        let mut w = Writer::begin(&mut store, None).unwrap();
        let kids: Vec<Uuid> = (0..5)
            .map(|i| w.create_metarecord(vec![tree(Some(root), &format!("f{i}"))]).unwrap().uuid)
            .collect();
        w.mass_add("tag", s("jazz"), &kids).unwrap();
        let ids = w.dense_ids(&kids).unwrap();
        assert_eq!(w.mass_delete_ids(&ids).unwrap().len(), 5);
        w.commit().unwrap();
        kids
    };
    // 5 creations, the add and the deletion: not an operation per deleted record.
    assert_eq!(store.last_write_keys().get("ops"), Some(&7), "{:?}", store.last_write_keys());
    for u in &kids {
        assert!(!Rows::exists(&store, *u).unwrap());
    }
    sound(&store);
}

#[test]
fn a_folder_deleted_with_all_its_children_is_one_entry() {
    let (mut store, _dir) = open();
    let tree = |parent, name: &str| Field::new("loc", Value::TreeRef { parent, name: name.into() });
    let (all, root) = write(&mut store, |w| {
        let root = w.create_metarecord(vec![tree(None, "")]).unwrap().uuid;
        let dir = w.create_metarecord(vec![tree(Some(root), "d")]).unwrap().uuid;
        let sub = w.create_metarecord(vec![tree(Some(dir), "s")]).unwrap().uuid;
        let a = w.create_metarecord(vec![tree(Some(dir), "a")]).unwrap().uuid;
        let b = w.create_metarecord(vec![tree(Some(sub), "b")]).unwrap().uuid;
        (vec![dir, sub, a, b], root)
    });
    delete(&mut store, &all);
    assert_eq!(store.last_write_keys().get("ops"), Some(&1), "{:?}", store.last_write_keys());
    assert!(Rows::exists(&store, root).unwrap());
    sound(&store);
    while store.fold_step().unwrap() {}
    sound(&store);
}

#[test]
fn a_folder_deleted_with_a_child_left_is_refused_as_ever() {
    let (mut store, _dir) = open();
    let tree = |parent, name: &str| Field::new("loc", Value::TreeRef { parent, name: name.into() });
    let (dir, _a) = write(&mut store, |w| {
        let root = w.create_metarecord(vec![tree(None, "")]).unwrap().uuid;
        let dir = w.create_metarecord(vec![tree(Some(root), "d")]).unwrap().uuid;
        let a = w.create_metarecord(vec![tree(Some(dir), "a")]).unwrap().uuid;
        (dir, a)
    });
    let ids = store.dense_ids(&[dir]).unwrap();
    let mut w = Writer::begin(&mut store, None).unwrap();
    w.mass_delete_ids(&ids).unwrap();
    assert!(w.commit().is_err(), "a child would be left under a folder that is gone");
}

#[test]
fn undoing_the_deletion_of_a_folder_with_its_children_puts_the_tree_back() {
    use metafolder_daemon::revert;
    for folded in [false, true] {
        let (mut store, _dir) = open();
        let tree =
            |parent, name: &str| Field::new("loc", Value::TreeRef { parent, name: name.into() });
        let (all, root) = write(&mut store, |w| {
            let root = w.create_metarecord(vec![tree(None, "")]).unwrap().uuid;
            let dir = w.create_metarecord(vec![tree(Some(root), "d")]).unwrap().uuid;
            let sub = w.create_metarecord(vec![tree(Some(dir), "s")]).unwrap().uuid;
            let a = w.create_metarecord(vec![tree(Some(dir), "a")]).unwrap().uuid;
            let b = w.create_metarecord(vec![tree(Some(sub), "b")]).unwrap().uuid;
            (vec![dir, sub, a, b], root)
        });
        let mut w = Writer::begin(&mut store, Some("delete".into())).unwrap();
        let label = w.label_id();
        let ids = w.dense_ids(&all).unwrap();
        assert_eq!(w.mass_delete_ids(&ids).unwrap().len(), 4);
        w.commit().unwrap();
        if folded {
            while store.fold_step().unwrap() {}
        }
        let ops = revert::label_ops(&store, label).unwrap();
        let mut w = Writer::begin(&mut store, None).unwrap();
        revert::apply(&mut w, &ops).unwrap();
        w.commit().unwrap();
        for u in all.iter().chain([&root]) {
            assert!(Rows::exists(&store, *u).unwrap(), "folded: {folded}");
        }
        assert_eq!(Rows::metarecord_count(&store).unwrap(), 5);
        while store.fold_step().unwrap() {}
        sound(&store);
    }
}

/// Taking back a mass add is one entry of the log, whose operations each name
/// the one they revert; taking that back puts the value on again the same way.
#[test]
fn reverting_a_mass_add_is_one_entry_naming_what_it_reverts() {
    use metafolder_daemon::revert;
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 60);
    let mut w = Writer::begin(&mut store, Some("tag".into())).unwrap();
    let label = w.label_id();
    let ids = w.dense_ids(&uuids[..40]).unwrap();
    w.mass_add_ids("mark", s("m"), &ids).unwrap();
    w.commit().unwrap();
    let holders = |store: &KvStore| {
        uuids
            .iter()
            .filter(|&&u| Rows::rows(store, u).unwrap().iter().any(|r| r.name == "mark"))
            .count()
    };
    assert_eq!(holders(&store), 40);

    let ops = revert::label_ops(&store, label).unwrap();
    assert_eq!(ops.len(), 40);
    let mut w = Writer::begin(&mut store, None).unwrap();
    let back = w.label_id();
    revert::apply(&mut w, &ops).unwrap();
    w.commit().unwrap();
    let written = store.last_write_keys();
    assert_eq!(written.get("ops"), Some(&1), "one entry: {written:?}");
    assert_eq!(written.get("records"), None, "{written:?}");
    assert_eq!(holders(&store), 0);
    let undone = store.ops_after(ops.last().unwrap().id).unwrap();
    assert_eq!(undone.len(), 40);
    for (op, reverted) in undone.iter().zip(&ops) {
        assert_eq!(op.op_type, "delete_field");
        assert_eq!(op.reverts_op_id, Some(reverted.id));
        assert_eq!(op.entity_uuid, reverted.entity_uuid);
    }
    sound(&store);

    // And back: the value is on the same records again.
    let ops = revert::label_ops(&store, back).unwrap();
    let mut w = Writer::begin(&mut store, None).unwrap();
    revert::apply(&mut w, &ops).unwrap();
    w.commit().unwrap();
    assert_eq!(store.last_write_keys().get("ops"), Some(&1));
    assert_eq!(holders(&store), 40);
    sound(&store);
    assert!(store.check().unwrap().is_empty());
}

/// A record that lost the value since is no longer as the add left it: the
/// revert goes record by record, and leaves it alone.
#[test]
fn reverting_a_mass_add_a_record_lost_the_value_of_goes_one_by_one() {
    use metafolder_daemon::revert;
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 40);
    let mut w = Writer::begin(&mut store, Some("tag".into())).unwrap();
    let label = w.label_id();
    let ids = w.dense_ids(&uuids).unwrap();
    w.mass_add_ids("mark", s("m"), &ids).unwrap();
    w.commit().unwrap();
    write(&mut store, |w| {
        w.delete_fields_valued(uuids[3], "mark", &s("m")).unwrap();
    });
    let ops = revert::label_ops(&store, label).unwrap();
    let mut w = Writer::begin(&mut store, None).unwrap();
    revert::apply(&mut w, &ops).unwrap();
    w.commit().unwrap();
    for &u in &uuids {
        assert!(!Rows::rows(&store, u).unwrap().iter().any(|r| r.name == "mark"));
    }
    sound(&store);
    assert!(store.check().unwrap().is_empty());
}

/// What a prune cuts of a stub that reverts another leaves no mark behind.
#[test]
fn pruning_into_a_stub_that_reverts_another_keeps_the_log_consistent() {
    use metafolder_daemon::revert;
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 40);
    let mut w = Writer::begin(&mut store, Some("tag".into())).unwrap();
    let label = w.label_id();
    let ids = w.dense_ids(&uuids).unwrap();
    w.mass_add_ids("mark", s("m"), &ids).unwrap();
    w.commit().unwrap();
    let ops = revert::label_ops(&store, label).unwrap();
    let mut w = Writer::begin(&mut store, None).unwrap();
    revert::apply(&mut w, &ops).unwrap();
    w.commit().unwrap();
    let undone = store.ops_after(ops.last().unwrap().id).unwrap();
    metafolder_daemon::log::prune(&mut store, undone[9].id).unwrap();
    sound(&store);
    assert!(store.check().unwrap().is_empty());
    let left = store.ops_after(0).unwrap();
    assert!(left.iter().all(|o| o.reverts_op_id.is_none_or(|r| r >= ops[0].id)));
}

/// Delete, undo, redo, then undo again — what ctrl+z, ctrl+shift+z, ctrl+z do:
/// the deletion's records come back whole, never as empty shells.
#[test]
fn undoing_again_after_a_redo_brings_the_records_back_whole() {
    use metafolder_daemon::revert;
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 60);
    let start: Vec<usize> = uuids.iter().map(|&u| Rows::rows(&store, u).unwrap().len()).collect();
    let mut w = Writer::begin(&mut store, Some("delete".into())).unwrap();
    let label = w.label_id();
    let ids = w.dense_ids(&uuids[..40]).unwrap();
    w.mass_delete_ids(&ids).unwrap();
    w.commit().unwrap();
    let mut labels = vec![label];
    for _ in 0..3 {
        let ops = revert::label_ops(&store, *labels.last().unwrap()).unwrap();
        let mut w = Writer::begin(&mut store, None).unwrap();
        labels.push(w.label_id());
        revert::apply(&mut w, &ops).unwrap();
        w.commit().unwrap();
        while store.fold_step().unwrap() {}
        sound(&store);
    }
    // After delete, undo, redo, undo: the records are there with their rows.
    assert_eq!(Rows::metarecord_count(&store).unwrap(), 60);
    let now: Vec<usize> = uuids.iter().map(|&u| Rows::rows(&store, u).unwrap().len()).collect();
    assert_eq!(now, start);
}

/// ctrl+z, ctrl+shift+z, ctrl+z target the deletion's label each time: its
/// operations are reverted, then the revert's, then its own again.
#[test]
fn reverting_the_same_deletion_again_after_its_redo_brings_the_records_back() {
    use metafolder_daemon::revert;
    let (mut store, _dir) = open();
    let uuids: Vec<Uuid> = (0..60)
        .map(|i| {
            write(&mut store, |w| {
                w.create_metarecord(vec![
                    Field::new("note", s("m")),
                    Field::new("n", s(&format!("v{i}"))),
                ])
                .unwrap()
                .uuid
            })
        })
        .collect();
    let mut w = Writer::begin(&mut store, Some("delete".into())).unwrap();
    let label = w.label_id();
    let ids = w.dense_ids(&uuids[..40]).unwrap();
    w.mass_delete_ids(&ids).unwrap();
    w.commit().unwrap();
    let d_ops = revert::label_ops(&store, label).unwrap();
    // undo
    let mut w = Writer::begin(&mut store, None).unwrap();
    let undo = w.label_id();
    revert::apply(&mut w, &d_ops).unwrap();
    w.commit().unwrap();
    // redo: the undo's operations taken back
    let u_ops = revert::label_ops(&store, undo).unwrap();
    let mut w = Writer::begin(&mut store, None).unwrap();
    revert::apply(&mut w, &u_ops).unwrap();
    w.commit().unwrap();
    assert_eq!(Rows::metarecord_count(&store).unwrap(), 20);
    while store.fold_step().unwrap() {}
    // undo again: the deletion's operations once more
    let mut w = Writer::begin(&mut store, None).unwrap();
    revert::apply(&mut w, &d_ops).unwrap();
    w.commit().unwrap();
    assert_eq!(Rows::metarecord_count(&store).unwrap(), 60);
    for &u in &uuids {
        assert_eq!(Rows::rows(&store, u).unwrap().len(), 2, "{u}");
    }
    sound(&store);
}
