//! The log in one form of entry (doc "Log storage"): an operation on a list of
//! metarecords, its header in `log_entries`, its targets and what each held
//! before — its *before* — in `log_targets`. An operation on one metarecord is
//! a list of one: two keys. What a write left behind is not kept, but for the
//! parameters of its effect (the value an add added, which a revert takes off).

use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::Writer;
use metafolder_daemon::rows::FieldRow;
use metafolder_daemon::store::Log;
use uuid::Uuid;

mod common;
use common::TempDir;

fn s(text: &str) -> Value {
    Value::String(text.into())
}

fn row(name: &str, value: Value) -> FieldRow {
    FieldRow { name: name.into(), value }
}

fn write<T>(store: &mut KvStore, f: impl FnOnce(&mut Writer) -> T) -> T {
    let mut w = Writer::begin(store, None).unwrap();
    let out = f(&mut w);
    w.commit().unwrap();
    out
}

fn record(store: &mut KvStore) -> Uuid {
    write(store, |w| {
        w.create_metarecord(vec![Field::new("a", s("one")), Field::new("tag", s("x"))])
            .unwrap()
            .uuid
    })
}

/// The keys of the log the last write put, by table.
fn log_keys(store: &KvStore) -> Vec<(String, u64)> {
    let mut out: Vec<(String, u64)> = store
        .last_write_keys()
        .into_iter()
        .filter(|(t, _)| matches!(*t, "log_entries" | "log_targets" | "log_snaps"))
        .map(|(t, n)| (t.to_string(), n))
        .collect();
    out.sort();
    out
}

fn two_keys() -> Vec<(String, u64)> {
    vec![("log_entries".into(), 1), ("log_targets".into(), 1)]
}

fn last_op(store: &KvStore) -> metafolder_daemon::log::OpRow {
    let head = store.head().unwrap().unwrap();
    store.op(head).unwrap().unwrap()
}

#[test]
fn an_operation_on_one_metarecord_is_two_keys() {
    let dir = TempDir::new("kv-log-entries");
    let mut store = KvStore::open_unsynced(dir.path()).unwrap();
    let uuid = record(&mut store);
    assert_eq!(log_keys(&store), two_keys(), "a creation");
    let op = last_op(&store);
    assert_eq!((op.op_type.as_str(), op.entity_uuid), ("create_metarecord", uuid));
    assert!(store.snapshots(op.id).unwrap().is_empty(), "nothing was there before");

    write(&mut store, |w| w.set_field(uuid, "a", s("two")).unwrap());
    assert_eq!(log_keys(&store), two_keys(), "a replacement");
    let op = last_op(&store);
    assert_eq!((op.op_type.as_str(), op.field_name.as_deref()), ("set_field", Some("a")));
    assert_eq!(store.snapshots(op.id).unwrap(), vec![row("a", s("one"))], "what it replaced");
}

#[test]
fn an_append_keeps_the_value_it_added_and_a_removal_the_value_it_took() {
    let dir = TempDir::new("kv-log-entries-append");
    let mut store = KvStore::open_unsynced(dir.path()).unwrap();
    let uuid = record(&mut store);
    write(&mut store, |w| w.append_field(uuid, "tag", s("y")).unwrap());
    assert_eq!(log_keys(&store), two_keys());
    let op = last_op(&store);
    assert_eq!(op.op_type, "append_field");
    assert_eq!(store.added(op.id).unwrap(), vec![row("tag", s("y"))]);
    assert!(store.snapshots(op.id).unwrap().is_empty());

    write(&mut store, |w| w.delete_fields_valued(uuid, "tag", &s("y")).unwrap());
    let op = last_op(&store);
    assert_eq!(op.op_type, "delete_field");
    assert_eq!(store.snapshots(op.id).unwrap(), vec![row("tag", s("y"))], "the value removed");
    assert!(store.added(op.id).unwrap().is_empty());

    // Every row of a name: the field unset, each row its before.
    write(&mut store, |w| w.append_field(uuid, "tag", s("z")).unwrap());
    write(&mut store, |w| w.delete_fields_named(uuid, "tag").unwrap());
    let op = last_op(&store);
    let mut before = store.snapshots(op.id).unwrap();
    before.sort_by_key(|r| format!("{r:?}"));
    assert_eq!(before, vec![row("tag", s("x")), row("tag", s("z"))]);
}

fn records(store: &mut KvStore, n: usize) -> Vec<Uuid> {
    write(store, |w| {
        (0..n)
            .map(|i| w.create_metarecord(vec![Field::new("n", Value::Int(i as i64))]).unwrap().uuid)
            .collect()
    })
}

/// A write to many records names them by uuid, so a reindex, which numbers
/// the ids again, has nothing of the log to rewrite.
#[test]
fn a_write_to_many_records_names_its_targets_by_uuid_through_a_reindex() {
    let dir = TempDir::new("kv-log-entries-mass");
    let mut store = KvStore::open_unsynced(dir.path()).unwrap();
    let uuids = records(&mut store, 5000);
    let ids = store.dense_ids(&uuids).unwrap();
    write(&mut store, |w| w.mass_add_ids("tag", s("blue"), &ids).unwrap());
    let written = log_keys(&store);
    assert_eq!(
        written,
        vec![("log_entries".into(), 1), ("log_targets".into(), 2)],
        "one header, a chunk of 4096 targets and one of the rest"
    );
    let head = store.head().unwrap().unwrap();
    let ops = store.ops_back(head, None).unwrap();
    let added: Vec<Uuid> = ops.iter().take(5000).map(|o| o.entity_uuid).rev().collect();
    assert_eq!(added, uuids, "operation first + k on the k-th target");
    store.reindex().unwrap();
    let after: Vec<Uuid> =
        store.ops_back(head, Some(5000)).unwrap().iter().map(|o| o.entity_uuid).rev().collect();
    assert_eq!(after, uuids, "the same targets after a reindex");
}

#[test]
fn a_prune_keeps_an_entry_that_holds_its_target_whole() {
    let dir = TempDir::new("kv-log-entries-prune");
    let mut store = KvStore::open_unsynced(dir.path()).unwrap();
    let uuids = records(&mut store, 50);
    let first_mass = store.head().unwrap().unwrap() + 1;
    let ids = store.dense_ids(&uuids).unwrap();
    write(&mut store, |w| w.mass_add_ids("tag", s("blue"), &ids).unwrap());
    // The creations go, the write to many records holds the target: it stays.
    let (pruned, _) = metafolder_daemon::log::prune(&mut store, first_mass + 10).unwrap();
    assert_eq!(pruned, 50, "the creations, and none of the entry");
    assert_eq!(store.first_op().unwrap(), Some(first_mass));
    assert_eq!(store.ops_after(first_mass - 1).unwrap().len(), 50);
}

/// A deletion of many records keeps each record as it was until the fold,
/// which copies its rows into the chunks: the before reads the same on
/// either side.
#[test]
fn a_pending_deletion_reads_its_before_in_the_records_then_in_its_chunks() {
    let dir = TempDir::new("kv-log-entries-delete");
    let mut store = KvStore::open_unsynced(dir.path()).unwrap();
    let uuids = records(&mut store, 300);
    let ids = store.dense_ids(&uuids).unwrap();
    write(&mut store, |w| w.mass_delete_ids(&ids).unwrap());
    let head = store.head().unwrap().unwrap();
    let ops = store.ops_back(head, Some(300)).unwrap();
    let before = |store: &KvStore| -> Vec<Vec<FieldRow>> {
        ops.iter().map(|o| store.snapshots(o.id).unwrap()).collect()
    };
    let pending = before(&store);
    for (k, rows) in pending.iter().enumerate() {
        assert_eq!(rows, &vec![row("n", Value::Int(299 - k as i64))], "op {k}");
    }
    store.fold().unwrap();
    assert_eq!(before(&store), pending, "the fold wrote them into the chunks");
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

// ── Compaction (doc "Log storage") ──────────────────────────────────────────

use metafolder_daemon::log::{Retention, WriteContext};

/// A separate write in an existing context.
fn write_in<T>(store: &mut KvStore, context: i64, f: impl FnOnce(&mut Writer) -> T) -> T {
    let mut w = Writer::begin_in(store, WriteContext::In(context), Retention::UNLIMITED).unwrap();
    let out = f(&mut w);
    w.commit().unwrap();
    out
}

fn context(store: &mut KvStore) -> i64 {
    use metafolder_daemon::store::{Begin, LabelMeta};
    let tx = store.begin_write().unwrap();
    let meta = LabelMeta { name: "loop".into(), description: None, parent: None, created: 0 };
    let id = tx.create_label(&meta).unwrap();
    tx.commit().unwrap();
    id
}

/// What a loop of `mf … field add` over many files writes, one command each,
/// inside one context.
#[test]
fn a_loop_of_writes_in_one_context_is_one_entry() {
    let dir = TempDir::new("kv-log-entries-compact");
    let mut store = KvStore::open_unsynced(dir.path()).unwrap();
    let uuids = records(&mut store, 5000);
    let ctx = context(&mut store);
    let entries = store.counts().unwrap().0;
    for &u in &uuids {
        write_in(&mut store, ctx, |w| w.append_field(u, "tag", s("x")).unwrap());
    }
    assert_eq!(store.counts().unwrap().0, entries + 1, "one entry");
    assert_eq!(log_keys(&store), two_keys(), "the header and the last chunk rewritten");
    let ops = metafolder_daemon::revert::label_ops(&store, ctx).unwrap();
    assert_eq!(ops.iter().map(|o| o.entity_uuid).collect::<Vec<_>>(), uuids);
    assert!(ops.iter().all(|o| o.op_type == "append_field"));
    assert_eq!(store.added(ops[4500].id).unwrap(), vec![row("tag", s("x"))]);
    let history = store.entity_ops_after(uuids[4200], 0).unwrap();
    assert_eq!(history.iter().filter(|o| o.op_type == "append_field").count(), 1);

    // Undone as a whole, as if each were an entry.
    let mut w = Writer::begin(&mut store, Some("undo".into())).unwrap();
    metafolder_daemon::revert::apply(&mut w, &ops).unwrap();
    w.commit().unwrap();
    for &u in &uuids {
        let tags = metafolder_daemon::store::Rows::rows_named(&store, u, "tag").unwrap();
        assert!(tags.is_empty(), "{u}");
    }
}

#[test]
fn an_entry_is_extended_only_by_the_same_operation_on_a_new_target_last_in_the_log() {
    let dir = TempDir::new("kv-log-entries-compact-not");
    let mut store = KvStore::open_unsynced(dir.path()).unwrap();
    let uuids = records(&mut store, 4);
    let ctx = context(&mut store);
    let other = context(&mut store);
    let entries = |store: &KvStore| store.counts().unwrap().0;
    let start = entries(&store);
    let add = |store: &mut KvStore, ctx, u, v: &str| {
        write_in(store, ctx, |w| w.append_field(u, "tag", s(v)).unwrap());
    };
    add(&mut store, ctx, uuids[0], "x");
    add(&mut store, ctx, uuids[1], "y");
    assert_eq!(entries(&store), start + 2, "another value");
    add(&mut store, other, uuids[2], "y");
    assert_eq!(entries(&store), start + 3, "another context");
    write_in(&mut store, ctx, |w| w.set_field(uuids[3], "n", Value::Int(7)).unwrap());
    add(&mut store, ctx, uuids[3], "y");
    assert_eq!(entries(&store), start + 5, "not the last entry");
    // The same target twice: a set of the same field and value to it again.
    write_in(&mut store, ctx, |w| w.set_field(uuids[0], "m", Value::Int(1)).unwrap());
    write_in(&mut store, ctx, |w| w.set_field(uuids[0], "m", Value::Int(2)).unwrap());
    write_in(&mut store, ctx, |w| w.set_field(uuids[0], "m", Value::Int(2)).unwrap());
    let head = store.head().unwrap().unwrap();
    let ops = store.ops_back(head, Some(3)).unwrap();
    let same: Vec<_> = ops.iter().filter(|o| o.field_name.as_deref() == Some("m")).collect();
    assert!(same.iter().all(|o| o.entity_uuid == uuids[0]));
    assert_eq!(entries(&store) as usize, start as usize + 5 + same.len(), "one entry each");
}
