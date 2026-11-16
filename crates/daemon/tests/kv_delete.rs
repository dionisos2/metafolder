//! Deleting a metarecord in the key-value store (doc "Deleting a metarecord
//! in the store"): the deletion takes it out of the universe and removes its
//! rows, and leaves what the indexes hold about it where it is — every answer
//! is read through the universe. What it writes is counted by table, and
//! what it answers afterwards is held to the oracle and to a rebuild.

use metafolder_core::metarecord::{Field, TreeName, Value};
use metafolder_core::query::{Aspect, FollowTarget, Query};
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::Writer;
use metafolder_daemon::query_result::{SortKey, SortOrder};
use metafolder_daemon::store::{Log, Questions, Rows};
use uuid::Uuid;

mod common;
use common::TempDir;

fn tree(parent: Option<Uuid>, name: &str) -> Value {
    Value::TreeRef { parent, name: TreeName::from(name) }
}

struct Repo {
    store: KvStore,
    dir: TempDir,
    root: Uuid,
    folder: Uuid,
    /// The file the tests delete: the first of the folder's three.
    file: Uuid,
    others: Vec<Uuid>,
}

fn file_fields(folder: Uuid, i: usize) -> Vec<Field> {
    vec![
        Field::new("loc", tree(Some(folder), &format!("file{i}.txt"))),
        Field::new("title", Value::String(format!("Hello number {i}"))),
        Field::new("tag", Value::String("common".into())),
        Field::new("tag", Value::String(format!("own{i}"))),
        Field::new("size", Value::Int(10 * i as i64)),
        Field::new("at", Value::DateTime(1_700_000_000_000 + i as i64)),
        Field::new("link", Value::Ref(folder)),
        Field::new("gone", Value::Nothing),
    ]
}

/// A root, a folder, and three files in it carrying every kind of index
/// entry: a position, texts, a shared and an own value, numbers, a reference
/// and a `Nothing`.
fn repo() -> Repo {
    let dir = TempDir::new("kv-delete");
    let mut store = KvStore::open(dir.path()).unwrap();
    let mut w = Writer::begin(&mut store, None).unwrap();
    let root = w.create_metarecord(vec![Field::new("loc", tree(None, ""))]).unwrap().uuid;
    let folder =
        w.create_metarecord(vec![Field::new("loc", tree(Some(root), "dir"))]).unwrap().uuid;
    let mut files = Vec::new();
    for i in 0..3 {
        files.push(w.create_metarecord(file_fields(folder, i)).unwrap().uuid);
    }
    w.commit().unwrap();
    Repo { store, dir, root, folder, file: files[0], others: files[1..].to_vec() }
}

fn delete(store: &mut KvStore, uuid: Uuid) {
    let mut w = Writer::begin(store, None).unwrap();
    w.delete_metarecord(uuid).unwrap();
    w.commit().unwrap();
}

fn check(store: &KvStore) {
    let diff = store.check_derived().unwrap();
    assert!(diff.is_empty(), "derived data diverges from a rebuild:\n{}", diff.join("\n"));
}

fn raw() -> Aspect {
    Aspect::Raw
}

/// A query per kind of index, each answered by the oracle and by the serving
/// path, which must agree (`common::engines::both`).
fn every_answer(store: &KvStore) -> Vec<Vec<Uuid>> {
    let eq = |field: &str, value: Value| Query::Eq { field: field.into(), value, aspect: raw() };
    let present = |field: &str| Query::IsPresent { field: field.into(), aspect: raw() };
    let queries = vec![
        eq("tag", Value::String("common".into())),
        eq("tag", Value::String("own0".into())),
        present("size"),
        Query::IsAbsent { field: "gone".into(), aspect: raw() },
        Query::IsUnknown { field: "title".into() },
        Query::Not { operand: Box::new(present("title")) },
        Query::Not { operand: Box::new(eq("size", Value::Int(10))) },
        Query::Gte { field: "size".into(), value: Value::Int(0), aspect: raw() },
        Query::Matches { field: "title".into(), pattern: "(?i)hello".into(), aspect: raw() },
        Query::Matches { field: "loc".into(), pattern: "file".into(), aspect: Aspect::Value },
        Query::Follows { field: "loc".into(), target: FollowTarget::Path("/dir".into()) },
        Query::FollowsTransitive {
            field: "loc".into(),
            target: FollowTarget::Path("/".into()),
            inclusive: true,
        },
        Query::Follows {
            field: "link".into(),
            target: FollowTarget::Condition(Box::new(present("loc"))),
        },
        Query::SameAs {
            field: "tag".into(),
            target: Box::new(eq("tag", Value::String("own1".into()))),
        },
    ];
    let mut out: Vec<Vec<Uuid>> =
        queries.iter().map(|q| common::engines::both(store, q, &[])).collect();
    for order in [SortOrder::Asc, SortOrder::Desc] {
        for field in ["size", "tag", "loc"] {
            let sort = [SortKey { field: field.into(), order }];
            out.push(common::engines::both(store, &present("loc"), &sort));
        }
    }
    out
}

#[test]
fn a_deletion_writes_nothing_to_the_indexes_but_the_universe() {
    let mut r = repo();
    delete(&mut r.store, r.others[1]);
    let written = r.store.last_write_keys();
    for table in ["forest", "ids", "uuids", "parts", "kids", "grams"] {
        assert_eq!(written.get(table), None, "a deletion wrote to {table}: {written:?}");
    }
    assert_eq!(written.get("records"), Some(&1), "the record's one key: {written:?}");
    // The universe, and nothing else: the folder still has children.
    assert_eq!(written.get("sets"), Some(&1), "{written:?}");
    check(&r.store);
}

#[test]
fn a_deleted_metarecord_is_gone_from_every_answer() {
    let mut r = repo();
    delete(&mut r.store, r.file);
    for answer in every_answer(&r.store) {
        assert!(!answer.contains(&r.file), "a deleted metarecord in an answer");
    }
    check(&r.store);

    // The store's own reads.
    let names: Vec<String> =
        r.store.children("loc", r.folder).unwrap().into_iter().map(|(_, n)| n).collect();
    assert_eq!(names, ["file1.txt", "file2.txt"]);
    assert_eq!(r.store.child_by_bytes("loc", Some(r.folder), b"file0.txt").unwrap(), None);
    assert_eq!(r.store.children_page("loc", r.folder, None, false, 10).unwrap().len(), 2);
    assert!(r.store.forest().unwrap().iter().all(|t| t.uuid != r.file));
    assert!(r.store.field_rows("tag").unwrap().iter().all(|(u, _)| *u != r.file));
    assert!(!r.store.holders("tag").unwrap().contains(&r.file));
    let mut seen = Vec::new();
    r.store
        .for_each_row(&mut |u, _| {
            seen.push(u);
            Ok(())
        })
        .unwrap();
    assert!(!seen.contains(&r.file));
    // The schema's questions.
    assert!(!r.store.count_over("tag", 1, 100).unwrap().contains(&r.file));
    assert!(!r.store.wrong_type("size", "string", 100).unwrap().contains(&r.file));
    assert!(!r.store.missing("nope", 100).unwrap().contains(&r.file));
}

#[test]
fn a_new_metarecord_takes_the_place_of_a_deleted_one() {
    let mut r = repo();
    delete(&mut r.store, r.file);
    let mut w = Writer::begin(&mut r.store, None).unwrap();
    let again = w.create_metarecord(file_fields(r.folder, 0)).unwrap().uuid;
    w.commit().unwrap();
    assert_eq!(r.store.child_by_bytes("loc", Some(r.folder), b"file0.txt").unwrap(), Some(again));
    assert_eq!(r.store.children("loc", r.folder).unwrap().len(), 3);
    for answer in every_answer(&r.store) {
        assert!(!answer.contains(&r.file));
    }
    check(&r.store);
}

#[test]
fn reverting_a_deletion_brings_the_metarecord_back() {
    let mut r = repo();
    let before = every_answer(&r.store);
    let head = r.store.head().unwrap();
    delete(&mut r.store, r.file);
    common::kv::revert_since(&mut r.store, head);
    assert_eq!(every_answer(&r.store), before);
    assert_eq!(r.store.children("loc", r.folder).unwrap().len(), 3);
    check(&r.store);

    // And deleted again, then put back again: what the first deletion left
    // behind must not come back with it.
    let head = r.store.head().unwrap();
    delete(&mut r.store, r.file);
    check(&r.store);
    common::kv::revert_since(&mut r.store, head);
    assert_eq!(every_answer(&r.store), before);
    check(&r.store);
}

#[test]
fn a_folder_whose_last_child_is_deleted_is_no_longer_a_parent() {
    let mut r = repo();
    for uuid in [r.file, r.others[0], r.others[1]] {
        delete(&mut r.store, uuid);
        every_answer(&r.store);
        check(&r.store);
    }
    assert!(r.store.children("loc", r.folder).unwrap().is_empty());
    // The folder itself, now childless, goes the short way too.
    delete(&mut r.store, r.folder);
    let written = r.store.last_write_keys();
    assert_eq!(written.get("parts"), None, "{written:?}");
    every_answer(&r.store);
    check(&r.store);
}

/// The `Writer` refuses a write that leaves metarecords under a deleted
/// parent, but not one that deletes the parent first, then its children:
/// while it lasts, the parent is removed with children that exist.
#[test]
fn a_folder_deleted_before_its_files_leaves_the_answers_exact() {
    let mut r = repo();
    let before = every_answer(&r.store);
    let head = r.store.head().unwrap();
    let mut w = Writer::begin(&mut r.store, None).unwrap();
    w.delete_metarecord(r.folder).unwrap();
    for uuid in [r.file, r.others[0], r.others[1]] {
        w.delete_metarecord(uuid).unwrap();
    }
    w.commit().unwrap();
    for answer in every_answer(&r.store) {
        assert!(answer.is_empty() || answer == [r.root], "{answer:?}");
    }
    check(&r.store);
    common::kv::revert_since(&mut r.store, head);
    assert_eq!(every_answer(&r.store), before);
    check(&r.store);
}

/// How many entries the raw table `name` holds.
fn entries(dir: &std::path::Path, name: &str) -> u64 {
    let env = common::kv::raw_env(dir);
    let table = common::kv::open_raw(&env, name);
    let r = env.read_txn().unwrap();
    table.len(&r).unwrap()
}

#[test]
fn a_reindex_removes_what_the_deletions_left_behind() {
    let mut r = repo();
    delete(&mut r.store, r.file);
    r.store.reindex().unwrap();
    every_answer(&r.store);
    check(&r.store);
    let Repo { store, dir, .. } = r;
    drop(store);
    // One key per record: the root, the folder and the two files left.
    assert_eq!(entries(dir.path(), "records"), 4);
    // One position each: the root, the folder and the two files left.
    assert_eq!(entries(dir.path(), "forest"), 4);
    assert_eq!(entries(dir.path(), "ids"), 4);
    assert_eq!(entries(dir.path(), "uuids"), 4);

    // And the store opens whole again.
    let store = KvStore::open(dir.path()).unwrap();
    every_answer(&store);
    check(&store);
}

// A record keeps its dense id between two reindexes (roadmap): what the
// entries and the stubs name by id keeps naming it when it is given back.

#[test]
fn a_record_given_back_by_a_revert_keeps_its_dense_id() {
    let mut r = repo();
    let id = r.store.dense_ids(&[r.file]).unwrap();
    assert_eq!(id.len(), 1);
    let head = r.store.head().unwrap();
    delete(&mut r.store, r.file);
    common::kv::revert_since(&mut r.store, head);
    assert_eq!(r.store.dense_ids(&[r.file]).unwrap(), id);
    check(&r.store);
    // Deleted and given back a second time.
    let head = r.store.head().unwrap();
    delete(&mut r.store, r.file);
    common::kv::revert_since(&mut r.store, head);
    assert_eq!(r.store.dense_ids(&[r.file]).unwrap(), id);
    check(&r.store);
}

#[test]
fn a_creation_under_a_uuid_the_store_knows_keeps_its_dense_id() {
    let mut r = repo();
    let id = r.store.dense_ids(&[r.file]).unwrap();
    delete(&mut r.store, r.file);
    let mut w = Writer::begin(&mut r.store, None).unwrap();
    w.create_metarecord_with_uuid(r.file, file_fields(r.folder, 0)).unwrap();
    w.commit().unwrap();
    assert_eq!(r.store.dense_ids(&[r.file]).unwrap(), id);
    assert_eq!(r.store.children("loc", r.folder).unwrap().len(), 3);
    check(&r.store);
}

#[test]
fn a_mass_add_over_a_record_given_back_is_undone_in_one_pass() {
    use metafolder_daemon::revert;
    let (mut store, _dir) = common::kv::store();
    let uuids: Vec<Uuid> = {
        let mut w = Writer::begin(&mut store, None).unwrap();
        let v = (0..40)
            .map(|i| w.create_metarecord(vec![Field::new("n", Value::Int(i))]).unwrap().uuid)
            .collect();
        w.commit().unwrap();
        v
    };
    let mut w = Writer::begin(&mut store, Some("mass".into())).unwrap();
    let label = w.label_id();
    w.mass_add("tag", Value::String("x".into()), &uuids).unwrap();
    w.commit().unwrap();
    // One of the records is deleted and given back after the mass add, which
    // names it by its dense id.
    let head = store.head().unwrap();
    delete(&mut store, uuids[7]);
    common::kv::revert_since(&mut store, head);
    let ops = revert::label_ops(&store, label).unwrap();
    let mut w = Writer::begin(&mut store, None).unwrap();
    assert!(w.revert_mass_run(&ops).unwrap(), "the store refused the one-pass revert");
    w.commit().unwrap();
    assert!(store.check_derived().unwrap().is_empty());
}
