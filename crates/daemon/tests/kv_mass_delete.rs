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
    assert_eq!(written.get("log_entries"), Some(&1), "one entry: {written:?}");
    assert_eq!(written.get("log_snaps"), None, "{written:?}");
    assert_eq!(written.get("metarecords"), None, "no record is written: {written:?}");
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

/// What the deletion reads is the same whatever the number of records, but
/// for the uuid of each, which the log names its targets by (doc "Log
/// storage").
#[test]
fn a_mass_delete_reads_one_uuid_per_record() {
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
    assert!(big <= small + 2900 + 4, "{small} reads for 100 records, {big} for 3000");
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
        let before = store.snapshots(op.id).unwrap();
        assert_eq!(before.len(), 2, "its two rows: {before:?}");
        assert!(before.iter().any(|r| r.name == "tag"));
        assert!(before.iter().any(|r| r.name == "rank"));
        assert!(store.added(op.id).unwrap().is_empty());
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
                    .snapshots(o.id)
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
    assert_eq!(store.wear().unwrap().pending_writes, 1, "the deletion is pending");
    while store.fold_step().unwrap() {}
    assert_eq!(store.wear().unwrap().pending_writes, 0);
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
        let log_context = w.log_context_id();
        let ids = w.dense_ids(&uuids[..50]).unwrap();
        w.mass_delete_ids(&ids).unwrap();
        w.commit().unwrap();
        if folded {
            while store.fold_step().unwrap() {}
        }
        assert_eq!(Rows::metarecord_count(&store).unwrap(), 30);

        let ops = revert::log_context_ops(&store, log_context).unwrap();
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
    let log_context = w.log_context_id();
    let ids = w.dense_ids(&uuids[..50]).unwrap();
    w.mass_delete_ids(&ids).unwrap();
    w.commit().unwrap();
    assert_eq!(Rows::metarecord_count(&store).unwrap(), 30);

    let ops = revert::log_context_ops(&store, log_context).unwrap();
    let mut w = Writer::begin(&mut store, None).unwrap();
    revert::apply(&mut w, &ops).unwrap();
    w.commit().unwrap();

    let written = store.last_write_keys();
    for table in ["metarecords", "ids", "uuids", "index_raw_holder"] {
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
    let log_context = w.log_context_id();
    let ids = w.dense_ids(&uuids[..50]).unwrap();
    w.mass_delete_ids(&ids).unwrap();
    w.commit().unwrap();
    while store.fold_step().unwrap() {}
    assert_eq!(Rows::metarecord_count(&store).unwrap(), 30);

    let ops = revert::log_context_ops(&store, log_context).unwrap();
    let mut w = Writer::begin(&mut store, None).unwrap();
    revert::apply(&mut w, &ops).unwrap();
    w.commit().unwrap();

    let written = store.last_write_keys();
    for table in ["ids", "uuids", "index_raw_holder"] {
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
    assert_eq!(store.snapshots(ops[0].id).unwrap().len(), 2);
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
    let log_context = w.log_context_id();
    let ids = w.dense_ids(&uuids[..40]).unwrap();
    w.mass_delete_ids(&ids).unwrap();
    w.commit().unwrap();
    while store.fold_step().unwrap() {}
    store.reindex().unwrap();

    let ops = revert::log_context_ops(&store, log_context).unwrap();
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
    let log_context = w.log_context_id();
    let ids = w.dense_ids(&kids).unwrap();
    w.mass_delete_ids(&ids).unwrap();
    w.commit().unwrap();
    while store.fold_step().unwrap() {}
    let taker =
        write(&mut store, |w| w.create_metarecord(vec![tree(Some(root), "k3")]).unwrap().uuid);

    let ops = revert::log_context_ops(&store, log_context).unwrap();
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
    let log_context = w.log_context_id();
    let ids = w.dense_ids(&all).unwrap();
    w.mass_delete_ids(&ids).unwrap();
    w.commit().unwrap();
    assert_eq!(Rows::metarecord_count(&store).unwrap(), 1);
    sound(&store);

    // Give back only some of it: the first thirty operations of the stub (the
    // folder, whose id is the lowest, and twenty-nine of its children).
    let ops = revert::log_context_ops(&store, log_context).unwrap();
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
        assert_eq!(store.snapshots(op.id).unwrap().len(), 2, "{op:?}");
    }
    for op in &ops[..30] {
        assert!(!store.snapshots(op.id).unwrap().is_empty(), "{op:?}");
    }
}

/// What a deletion deleted is known while nothing came after it but its own
/// revert: a record given back and written to since has lost the rows the
/// deletion took, and the log says it does not know them rather than read the
/// rows of today.
/// The rows a deletion took are still the log's after its records were given
/// back and one was written to: the write keeps them first — in the chunk of
/// the deletion's operation — and only for the record it writes, the rest
/// staying where they are.
#[test]
fn the_rows_a_deletion_took_are_kept_when_a_record_given_back_is_written_to() {
    use metafolder_daemon::revert;
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 40);
    let mut w = Writer::begin(&mut store, Some("delete".into())).unwrap();
    let log_context = w.log_context_id();
    let ids = w.dense_ids(&uuids).unwrap();
    w.mass_delete_ids(&ids).unwrap();
    w.commit().unwrap();
    let ops = revert::log_context_ops(&store, log_context).unwrap();

    let mut w = Writer::begin(&mut store, None).unwrap();
    revert::apply(&mut w, &ops).unwrap();
    w.commit().unwrap();
    // One chunk per creation it logs; the deletion's are not written.
    assert_eq!(store.last_write_keys().get("log_targets"), Some(&40), "giving back copies nothing");

    let touched = uuids[7];
    write(&mut store, |w| {
        w.set_field(touched, "tag", s("purple")).unwrap();
    });
    // The write's own chunk, and the deletion's that takes that one record's
    // rows: no more.
    assert_eq!(
        store.last_write_keys().get("log_targets"),
        Some(&2),
        "{:?}",
        store.last_write_keys()
    );
    let rows_of = |store: &KvStore, op_id: i64| {
        let mut rows: Vec<_> = store
            .snapshots(op_id)
            .unwrap()
            .into_iter()
            .map(|r| (r.name, format!("{:?}", r.value)))
            .collect();
        rows.sort();
        rows
    };
    for op in &ops {
        assert_eq!(store.snapshots(op.id).unwrap().len(), 2, "{op:?}");
    }
    let op = ops.iter().find(|o| o.entity_uuid == touched).unwrap();
    assert_eq!(
        rows_of(&store, op.id),
        vec![
            ("rank".to_string(), format!("{:?}", Value::Int(0))),
            ("tag".to_string(), format!("{:?}", s("green"))),
        ],
        "the rows the deletion took, not today's"
    );
    // Reverting that deletion is no longer refused: it puts back what it took.
    write(&mut store, |w| w.delete_metarecord(touched).unwrap());
    let one = vec![op.clone()];
    let mut w = Writer::begin(&mut store, None).unwrap();
    revert::apply(&mut w, &one).unwrap();
    w.commit().unwrap();
    let mut rows: Vec<_> =
        Rows::rows(&store, touched).unwrap().into_iter().map(|r| (r.name, r.value)).collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(rows, vec![("rank".to_string(), Value::Int(0)), ("tag".to_string(), s("green"))]);
    sound(&store);
    assert!(store.check().unwrap().is_empty());
}

/// The rows a deletion took, in the records it gave back, are not today's once a
/// write to many records names them: the entry copies them first, for the
/// records it names, and the others keep waiting.
#[test]
fn a_write_to_many_records_keeps_the_rows_a_deletion_took() {
    use metafolder_daemon::revert;
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 40);
    let mut w = Writer::begin(&mut store, Some("delete".into())).unwrap();
    let log_context = w.log_context_id();
    let ids = w.dense_ids(&uuids).unwrap();
    w.mass_delete_ids(&ids).unwrap();
    w.commit().unwrap();
    let ops = revert::log_context_ops(&store, log_context).unwrap();
    let mut w = Writer::begin(&mut store, None).unwrap();
    revert::apply(&mut w, &ops).unwrap();
    w.commit().unwrap();

    let some = store.dense_ids(&uuids[..10]).unwrap();
    write(&mut store, |w| w.mass_add_ids("tag", s("jazz"), &some).unwrap());
    // The write's chunk, and the deletion's, which takes the ten records' rows.
    assert_eq!(
        store.last_write_keys().get("log_targets"),
        Some(&2),
        "{:?}",
        store.last_write_keys()
    );
    for op in &ops {
        let i = uuids.iter().position(|u| *u == op.entity_uuid).unwrap();
        let rows = store.snapshots(op.id).unwrap();
        let tags: Vec<_> = rows.iter().filter(|r| r.name == "tag").map(|r| &r.value).collect();
        assert_eq!(tags, vec![&s(["blue", "green"][i % 2])], "the rows it took, record {i}");
    }
    sound(&store);
    assert!(store.check().unwrap().is_empty());
}

/// Deleting again records a deletion has given back, then a fold: the first
/// deletion keeps its rows too, the records having left the store.
#[test]
fn a_deletion_given_back_and_made_again_keeps_the_rows_of_the_first() {
    use metafolder_daemon::revert;
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 40);
    let mut w = Writer::begin(&mut store, Some("first".into())).unwrap();
    let first = w.log_context_id();
    let ids = w.dense_ids(&uuids).unwrap();
    w.mass_delete_ids(&ids).unwrap();
    w.commit().unwrap();
    let ops = revert::log_context_ops(&store, first).unwrap();
    let mut w = Writer::begin(&mut store, None).unwrap();
    revert::apply(&mut w, &ops).unwrap();
    w.commit().unwrap();
    let mut w = Writer::begin(&mut store, Some("second".into())).unwrap();
    let second = w.log_context_id();
    let ids = w.dense_ids(&uuids).unwrap();
    w.mass_delete_ids(&ids).unwrap();
    w.commit().unwrap();
    let again = revert::log_context_ops(&store, second).unwrap();
    while store.fold_step().unwrap() {}
    assert_eq!(Rows::metarecord_count(&store).unwrap(), 0);
    for op in ops.iter().chain(&again) {
        let rows = store.snapshots(op.id).unwrap();
        assert_eq!(rows.len(), 2, "{op:?}");
    }
    sound(&store);
    assert!(store.check().unwrap().is_empty());
}

/// A reindex numbers the ids again, which a deletion given back still names its
/// records by: it keeps the rows of the records it took first, all of them.
#[test]
fn a_reindex_keeps_the_rows_of_the_records_a_deletion_gave_back() {
    use metafolder_daemon::revert;
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 40);
    let mut w = Writer::begin(&mut store, Some("delete".into())).unwrap();
    let log_context = w.log_context_id();
    let ids = w.dense_ids(&uuids).unwrap();
    w.mass_delete_ids(&ids).unwrap();
    w.commit().unwrap();
    let ops = revert::log_context_ops(&store, log_context).unwrap();
    let mut w = Writer::begin(&mut store, None).unwrap();
    revert::apply(&mut w, &ops).unwrap();
    w.commit().unwrap();
    write(&mut store, |w| {
        w.set_field(uuids[3], "tag", s("purple")).unwrap();
    });
    store.reindex().unwrap();
    for op in &ops {
        let rows = store.snapshots(op.id).unwrap();
        assert_eq!(rows.len(), 2, "{op:?}");
        let tag = rows.iter().find(|r| r.name == "tag").unwrap();
        let i = uuids.iter().position(|u| *u == op.entity_uuid).unwrap();
        assert_eq!(tag.value, s(["blue", "green"][i % 2]), "{op:?}");
    }
    sound(&store);
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
    let log_context = w.log_context_id();
    let ids = w.dense_ids(&uuids).unwrap();
    w.mass_delete_ids(&ids).unwrap();
    w.commit().unwrap();
    let ops = revert::log_context_ops(&store, log_context).unwrap();
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
    let log_context = w.log_context_id();
    let ids = w.dense_ids(&uuids[..40]).unwrap();
    w.mass_delete_ids(&ids).unwrap();
    w.commit().unwrap();
    write(&mut store, |w| {
        w.create_metarecord(vec![Field::new("other", s("x"))]).unwrap();
    });
    let ops = revert::log_context_ops(&store, log_context).unwrap();
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
    let log_context = w.log_context_id();
    let ids = w.dense_ids(&uuids[..40]).unwrap();
    w.mass_delete_ids(&ids).unwrap();
    w.commit().unwrap();
    let ops = revert::log_context_ops(&store, log_context).unwrap();
    let mut w = Writer::begin(&mut store, None).unwrap();
    let back = w.log_context_id();
    revert::apply(&mut w, &ops).unwrap();
    w.commit().unwrap();
    assert_eq!(Rows::metarecord_count(&store).unwrap(), 60);

    let ops = revert::log_context_ops(&store, back).unwrap();
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
            .map(|o| (o.entity_uuid, store.snapshots(o.id).unwrap().len()))
            .collect()
    };
    let before = says(&store);
    assert_eq!(before.len(), 40);
    store.reindex().unwrap();
    assert_eq!(says(&store), before);
    assert_eq!(store.wear().unwrap().pending_writes, 0);
    assert_eq!(Rows::metarecord_count(&store).unwrap(), 20);
    sound(&store);
}

#[test]
fn a_prune_in_the_middle_of_a_pending_deletion_keeps_it_whole_with_its_rows() {
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 30);
    let head = store.head().unwrap().unwrap();
    delete(&mut store, &uuids);
    let ops = store.ops_after(head).unwrap();
    let target = ops[10].id;
    // An entry goes whole or not at all (doc "Pruning the log").
    metafolder_daemon::log::prune(&mut store, target).unwrap();
    while store.fold_step().unwrap() {}
    let left = store.ops_after(head).unwrap();
    assert_eq!(left.len(), 30);
    for op in &left {
        assert_eq!(store.snapshots(op.id).unwrap().len(), 2, "{op:?}");
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
    assert_eq!(
        store.last_write_keys().get("log_entries"),
        Some(&7),
        "{:?}",
        store.last_write_keys()
    );
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
    assert_eq!(
        store.last_write_keys().get("log_entries"),
        Some(&1),
        "{:?}",
        store.last_write_keys()
    );
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
        let log_context = w.log_context_id();
        let ids = w.dense_ids(&all).unwrap();
        assert_eq!(w.mass_delete_ids(&ids).unwrap().len(), 4);
        w.commit().unwrap();
        if folded {
            while store.fold_step().unwrap() {}
        }
        let ops = revert::log_context_ops(&store, log_context).unwrap();
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
    let log_context = w.log_context_id();
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

    let ops = revert::log_context_ops(&store, log_context).unwrap();
    assert_eq!(ops.len(), 40);
    let mut w = Writer::begin(&mut store, None).unwrap();
    let back = w.log_context_id();
    revert::apply(&mut w, &ops).unwrap();
    w.commit().unwrap();
    let written = store.last_write_keys();
    assert_eq!(written.get("log_entries"), Some(&1), "one entry: {written:?}");
    assert_eq!(written.get("metarecords"), None, "{written:?}");
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
    let ops = revert::log_context_ops(&store, back).unwrap();
    let mut w = Writer::begin(&mut store, None).unwrap();
    revert::apply(&mut w, &ops).unwrap();
    w.commit().unwrap();
    assert_eq!(store.last_write_keys().get("log_entries"), Some(&1));
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
    let log_context = w.log_context_id();
    let ids = w.dense_ids(&uuids).unwrap();
    w.mass_add_ids("mark", s("m"), &ids).unwrap();
    w.commit().unwrap();
    write(&mut store, |w| {
        w.delete_fields_valued(uuids[3], "mark", &s("m")).unwrap();
    });
    let ops = revert::log_context_ops(&store, log_context).unwrap();
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
    let log_context = w.log_context_id();
    let ids = w.dense_ids(&uuids).unwrap();
    w.mass_add_ids("mark", s("m"), &ids).unwrap();
    w.commit().unwrap();
    let ops = revert::log_context_ops(&store, log_context).unwrap();
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
    let log_context = w.log_context_id();
    let ids = w.dense_ids(&uuids[..40]).unwrap();
    w.mass_delete_ids(&ids).unwrap();
    w.commit().unwrap();
    let mut log_contexts = vec![log_context];
    for _ in 0..3 {
        let ops = revert::log_context_ops(&store, *log_contexts.last().unwrap()).unwrap();
        let mut w = Writer::begin(&mut store, None).unwrap();
        log_contexts.push(w.log_context_id());
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

/// ctrl+z, ctrl+shift+z, ctrl+z target the deletion's log context each time: its
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
    let log_context = w.log_context_id();
    let ids = w.dense_ids(&uuids[..40]).unwrap();
    w.mass_delete_ids(&ids).unwrap();
    w.commit().unwrap();
    let d_ops = revert::log_context_ops(&store, log_context).unwrap();
    // undo
    let mut w = Writer::begin(&mut store, None).unwrap();
    let undo = w.log_context_id();
    revert::apply(&mut w, &d_ops).unwrap();
    w.commit().unwrap();
    // redo: the undo's operations taken back
    let u_ops = revert::log_context_ops(&store, undo).unwrap();
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

/// A record a write to one record changed after a write to many is read with
/// the later write winning. Folding a deletion of it, then undoing that, must
/// not let the older entry win again: the record is put back as the deletion
/// took it.
#[test]
fn undoing_a_folded_deletion_keeps_a_later_write_over_a_pending_mass_write() {
    use metafolder_daemon::revert;
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 40);
    let all = store.dense_ids(&uuids).unwrap();
    // A write to many records, left pending on the part…
    write(&mut store, |w| {
        w.mass_add_ids("tag", s("red"), &all).unwrap();
        w.mass_set_ids("rank", vec![Value::Int(99)], &all).unwrap();
    });
    // …then a write to one record that undoes it for that record.
    write(&mut store, |w| {
        w.delete_fields_valued(uuids[0], "tag", &s("red")).unwrap();
        w.set_field_multi(uuids[0], "rank", vec![Value::Int(5)]).unwrap();
    });
    let rows_of = |store: &KvStore, u: Uuid| {
        let mut rows: Vec<String> = Rows::rows(store, u)
            .unwrap()
            .iter()
            .map(|r| format!("{}={:?}", r.name, r.value))
            .collect();
        rows.sort();
        rows
    };
    let start: Vec<_> = uuids.iter().map(|&u| rows_of(&store, u)).collect();
    assert!(!start[0].iter().any(|r| r.contains("red")), "{:?}", start[0]);

    let mut w = Writer::begin(&mut store, Some("delete".into())).unwrap();
    let log_context = w.log_context_id();
    w.mass_delete_ids(&all).unwrap();
    w.commit().unwrap();
    // One step of the idle fold: the deletion, which goes first, and the
    // part's other entries with it — a deletion is a pending write of its
    // part (doc "Mass writes in the store"), so none older is left to win
    // over the record once given back.
    assert!(store.fold_step().unwrap());
    assert_eq!(store.wear().unwrap().pending_writes, 0, "the part folded whole");

    let ops = revert::log_context_ops(&store, log_context).unwrap();
    let mut w = Writer::begin(&mut store, None).unwrap();
    revert::apply(&mut w, &ops).unwrap();
    w.commit().unwrap();

    let after: Vec<_> = uuids.iter().map(|&u| rows_of(&store, u)).collect();
    assert_eq!(after[0], start[0], "the record given back is the one deleted");
    assert_eq!(after, start);
    while store.fold_step().unwrap() {}
    let again: Vec<_> = uuids.iter().map(|&u| rows_of(&store, u)).collect();
    assert_eq!(again, start, "after the fold");
    sound(&store);
}

/// Records a deletion gave back, deleted again by another deletion: until a
/// write changes them, the rows the first one took are still the records' own,
/// so the log knows them — before the second deletion is folded as after.
#[test]
fn a_deletion_given_back_then_deleted_again_keeps_its_rows() {
    use metafolder_daemon::revert;
    for fold in [false, true] {
        let (mut store, _dir) = open();
        let uuids = records(&mut store, 30);
        let all = store.dense_ids(&uuids).unwrap();
        let mut w = Writer::begin(&mut store, Some("delete".into())).unwrap();
        let log_context = w.log_context_id();
        w.mass_delete_ids(&all).unwrap();
        w.commit().unwrap();
        let first = revert::log_context_ops(&store, log_context).unwrap();
        let mut w = Writer::begin(&mut store, None).unwrap();
        revert::apply(&mut w, &first).unwrap();
        w.commit().unwrap();
        // Deleted again, by a deletion that is no redo of the first.
        write(&mut store, |w| w.mass_delete_ids(&all).unwrap());
        if fold {
            while store.fold_step().unwrap() {}
        }
        for op in &first {
            assert_eq!(store.snapshots(op.id).unwrap().len(), 2, "fold: {fold}");
        }
    }
}

/// A prune takes a deletion with whatever the log kept of it: the rows the
/// fold froze, and those copied when a record given back was written to.
#[test]
fn a_prune_takes_the_rows_kept_of_the_deletions_it_removes() {
    use metafolder_daemon::revert;
    for given_back in [false, true] {
        let (mut store, _dir) = open();
        let uuids = records(&mut store, 30);
        let all = store.dense_ids(&uuids).unwrap();
        let mut w = Writer::begin(&mut store, Some("delete".into())).unwrap();
        let log_context = w.log_context_id();
        w.mass_delete_ids(&all).unwrap();
        w.commit().unwrap();
        let ops = revert::log_context_ops(&store, log_context).unwrap();
        if given_back {
            // Given back, then written to: the rows the deletion took are copied.
            let mut w = Writer::begin(&mut store, None).unwrap();
            revert::apply(&mut w, &ops).unwrap();
            w.commit().unwrap();
            write(&mut store, |w| {
                for &u in &uuids {
                    w.append_field(u, "tag", s("red")).unwrap();
                }
            });
        } else {
            while store.fold_step().unwrap() {}
        }
        for op in &ops {
            assert_eq!(store.snapshots(op.id).unwrap().len(), 2, "given back: {given_back}");
        }
        // Inside the deletion, it is kept whole; past it, it goes whole.
        metafolder_daemon::log::prune(&mut store, ops[10].id).unwrap();
        for op in &ops {
            assert_eq!(store.snapshots(op.id).unwrap().len(), 2, "given back: {given_back}");
        }
        let after = records(&mut store, 1);
        let target = store.head().unwrap().unwrap();
        metafolder_daemon::log::prune(&mut store, target).unwrap();
        for op in &ops {
            assert!(
                store.snapshots(op.id).unwrap().is_empty(),
                "given back: {given_back}, operation {} pruned",
                op.id
            );
        }
        assert_eq!(store.op(target).unwrap().unwrap().entity_uuid, after[0]);
        sound(&store);
    }
}

/// A write that fails is as if it never was, to the store's flags too: a revert
/// that folded the deletions pending before it failed leaves them pending,
/// and their records deleted.
#[test]
fn a_failed_revert_leaves_the_pending_deletions_deleted() {
    use metafolder_daemon::revert;
    let (mut store, _dir) = open();
    let author =
        write(&mut store, |w| w.create_metarecord(vec![tree(None, "author")]).unwrap().uuid);
    let xs: Vec<Uuid> = write(&mut store, |w| {
        (0..20)
            .map(|i| {
                w.create_metarecord(vec![
                    tree(None, &format!("x{i}")),
                    Field::new("by", Value::Ref(author)),
                ])
                .unwrap()
                .uuid
            })
            .collect()
    });
    let mut w = Writer::begin(&mut store, Some("delete".into())).unwrap();
    let log_context = w.log_context_id();
    let ids = w.dense_ids(&xs).unwrap();
    w.mass_delete_ids(&ids).unwrap();
    w.commit().unwrap();
    // The record they point at goes too, and the first one's place is taken.
    delete(&mut store, &[author]);
    write(&mut store, |w| w.create_metarecord(vec![tree(None, "x0")]).unwrap());
    let count = Rows::metarecord_count(&store).unwrap();

    let ops = revert::log_context_ops(&store, log_context).unwrap();
    let mut w = Writer::begin(&mut store, None).unwrap();
    assert!(revert::apply(&mut w, &ops).is_err(), "x0's place is taken");
    drop(w);

    assert!(!Rows::exists(&store, author).unwrap(), "still deleted");
    for &x in &xs {
        assert!(!Rows::exists(&store, x).unwrap(), "still deleted");
    }
    assert_eq!(Rows::metarecord_count(&store).unwrap(), count);
    sound(&store);
}

/// Records holding an ordinary reference are given back in one pass, whether
/// the record they point at is alive or not: a reference is no position, its
/// target no parent.
#[test]
fn giving_back_records_that_hold_a_reference_makes_no_parent_of_its_target() {
    use metafolder_daemon::revert;
    for target_alive in [true, false] {
        let (mut store, _dir) = open();
        let target =
            write(&mut store, |w| w.create_metarecord(vec![tree(None, "t")]).unwrap().uuid);
        let xs: Vec<Uuid> = write(&mut store, |w| {
            (0..20)
                .map(|i| {
                    w.create_metarecord(vec![
                        Field::new("rank", Value::Int(i)),
                        Field::new("by", Value::Ref(target)),
                    ])
                    .unwrap()
                    .uuid
                })
                .collect()
        });
        if !target_alive {
            write(&mut store, |w| w.delete_metarecord(target).unwrap());
        }
        let mut w = Writer::begin(&mut store, Some("delete".into())).unwrap();
        let log_context = w.log_context_id();
        let ids = w.dense_ids(&xs).unwrap();
        w.mass_delete_ids(&ids).unwrap();
        w.commit().unwrap();
        let ops = revert::log_context_ops(&store, log_context).unwrap();
        let mut w = Writer::begin(&mut store, None).unwrap();
        revert::apply(&mut w, &ops).unwrap();
        w.commit().unwrap();
        let written = store.last_write_keys();
        assert_eq!(
            written.get("metarecords"),
            None,
            "alive: {target_alive}, in one pass: {written:?}"
        );
        for &x in &xs {
            assert!(Rows::exists(&store, x).unwrap());
        }
        sound(&store);
    }
}

/// Every record holding a position deleted at once, then given back: the field
/// has no type counted any more, and its folders are parents again all the same.
#[test]
fn giving_back_every_holder_of_a_position_makes_the_folders_parents_again() {
    use metafolder_daemon::revert;
    let (mut store, _dir) = open();
    let all: Vec<Uuid> = write(&mut store, |w| {
        let mut out = Vec::new();
        for f in 0..3 {
            let folder = w.create_metarecord(vec![tree(None, &format!("f{f}"))]).unwrap().uuid;
            out.push(folder);
            for k in 0..8 {
                out.push(
                    w.create_metarecord(vec![tree(Some(folder), &format!("k{k}"))]).unwrap().uuid,
                );
            }
        }
        out
    });
    let mut w = Writer::begin(&mut store, Some("delete".into())).unwrap();
    let log_context = w.log_context_id();
    let ids = w.dense_ids(&all).unwrap();
    w.mass_delete_ids(&ids).unwrap();
    w.commit().unwrap();
    assert_eq!(Rows::metarecord_count(&store).unwrap(), 0);
    let ops = revert::log_context_ops(&store, log_context).unwrap();
    let mut w = Writer::begin(&mut store, None).unwrap();
    revert::apply(&mut w, &ops).unwrap();
    w.commit().unwrap();
    assert_eq!(Rows::metarecord_count(&store).unwrap(), all.len());
    sound(&store);
}

/// A deletion of many records is a pending write of kind `delete` (doc "Mass
/// writes in the store"): its undo leaves it, the records it gave back still
/// holding its before; a write to one of them takes that one off it first.
#[test]
fn a_deletion_is_a_pending_write_that_its_undo_leaves() {
    use metafolder_daemon::revert;
    let (mut store, _dir) = open();
    let uuids = records(&mut store, 40);
    let mut w = Writer::begin(&mut store, Some("delete".into())).unwrap();
    let log_context = w.log_context_id();
    let ids = w.dense_ids(&uuids).unwrap();
    w.mass_delete_ids(&ids).unwrap();
    w.commit().unwrap();
    assert_eq!(store.last_write_keys().get("pending_writes"), Some(&1), "one part");
    assert_eq!(store.wear().unwrap().pending_writes, 1);
    let ops = revert::log_context_ops(&store, log_context).unwrap();

    let mut w = Writer::begin(&mut store, None).unwrap();
    revert::apply(&mut w, &ops).unwrap();
    w.commit().unwrap();
    assert_eq!(store.last_write_keys().get("pending_writes"), None, "the undo leaves it");
    assert_eq!(store.wear().unwrap().pending_writes, 1);

    write(&mut store, |w| w.set_field(uuids[7], "tag", s("purple")).unwrap());
    assert_eq!(store.last_write_keys().get("pending_writes"), Some(&1), "the record taken off");
    for op in &ops {
        assert_eq!(store.snapshots(op.id).unwrap().len(), 2, "{op:?}");
    }
    store.fold().unwrap();
    assert_eq!(store.wear().unwrap().pending_writes, 0);
    for op in &ops {
        assert_eq!(store.snapshots(op.id).unwrap().len(), 2, "{op:?}");
    }
    sound(&store);
}
