//! The key-value store's derived key spaces (docs/spec-storage.org, increment
//! 4 a): maintained row by row inside every write transaction, they must
//! always equal what a rebuild from the primary data gives — after ordinary
//! writes, deletions, retypes and renames, and after the log's navigation,
//! which rewrites rows by the same primitives.

use metafolder_core::metarecord::{Field, TreeName, Value};
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::{self, Writer};
use metafolder_daemon::store::{Log, Rows};
use uuid::Uuid;

mod common;
use common::TempDir;

fn open() -> (KvStore, TempDir) {
    let dir = TempDir::new("kv-derived");
    (KvStore::open(dir.path()).unwrap(), dir)
}

/// The derived key spaces' divergences from a rebuild — none expected.
fn check(store: &KvStore) {
    let diff = store.check_derived().unwrap();
    assert!(diff.is_empty(), "derived data diverges from a rebuild:\n{}", diff.join("\n"));
}

fn tree(parent: Option<Uuid>, name: &str) -> Value {
    Value::TreeRef { parent, name: TreeName::from(name) }
}

/// A repository holding every value type, multi-valued fields, a duplicated
/// value, a `Nothing` row and a small forest.
fn populate(store: &mut KvStore) -> (Uuid, Uuid, Uuid) {
    let mut w = Writer::begin(store, None).unwrap();
    let root = w.create_metarecord(vec![Field::new("loc", tree(None, ""))]).unwrap().uuid;
    let dir = w.create_metarecord(vec![Field::new("loc", tree(Some(root), "dir"))]).unwrap().uuid;
    let file = w
        .create_metarecord(vec![
            Field::new("loc", tree(Some(dir), "a.txt")),
            Field::new("title", Value::String("Hello".into())),
            Field::new("tag", Value::String("x".into())),
            Field::new("tag", Value::String("x".into())),
            Field::new("tag", Value::String("y".into())),
            Field::new("size", Value::Int(42)),
            Field::new("ratio", Value::Float(-0.5)),
            Field::new("seen", Value::Bool(true)),
            Field::new("at", Value::DateTime(1_700_000_000_000)),
            Field::new("link", Value::Ref(dir)),
            Field::new("gone", Value::Nothing),
        ])
        .unwrap()
        .uuid;
    w.commit().unwrap();
    (root, dir, file)
}

#[test]
fn created_records_are_derived_as_a_rebuild_derives_them() {
    let (mut store, _dir) = open();
    populate(&mut store);
    check(&store);
}

#[test]
fn edits_and_deletions_keep_the_derived_data_exact() {
    let (mut store, _dir) = open();
    let (root, dir, file) = populate(&mut store);

    let mut w = Writer::begin(&mut store, None).unwrap();
    w.set_field(file, "size", Value::Int(7)).unwrap();
    w.set_field_multi(file, "tag", vec![Value::String("z".into())]).unwrap();
    w.append_field(file, "size", Value::Int(9)).unwrap();
    w.set_field(file, "gone", Value::String("back".into())).unwrap();
    w.delete_fields_named(file, "seen").unwrap();
    // A move and a rename in the forest.
    w.set_field(file, "loc", tree(Some(root), "b.txt")).unwrap();
    w.set_field(dir, "loc", tree(Some(root), "renamed")).unwrap();
    w.commit().unwrap();
    check(&store);

    let mut w = Writer::begin(&mut store, None).unwrap();
    w.delete_metarecord(file).unwrap();
    w.commit().unwrap();
    check(&store);
}

#[test]
fn navigating_the_log_keeps_the_derived_data_exact() {
    let (mut store, _dir) = open();
    let start = store.head().unwrap();
    let (_, _, file) = populate(&mut store);
    let populated = store.head().unwrap();

    let mut w = Writer::begin(&mut store, None).unwrap();
    w.set_field(file, "size", Value::Int(1)).unwrap();
    w.commit().unwrap();
    check(&store);

    // Back to the populated state, then before it, then forward again.
    log::navigate(&mut store, populated).unwrap();
    check(&store);
    log::navigate(&mut store, start).unwrap();
    check(&store);
    let head = store.head().unwrap();
    assert_eq!(head, start);
    log::navigate(&mut store, populated).unwrap();
    check(&store);
}

#[test]
fn a_retype_and_a_field_rename_keep_the_derived_data_exact() {
    let (mut store, _dir) = open();
    let (_, _, file) = populate(&mut store);
    let title = Rows::rows_named(&store, file, "title").unwrap()[0].id;
    let mut w = Writer::begin(&mut store, None).unwrap();
    w.rename_field(file, title, "heading", Value::String("Hello".into())).unwrap();
    w.retype_field("size", metafolder_core::metarecord::FieldType::String).unwrap();
    w.commit().unwrap();
    check(&store);
}

/// A reindex — what a store from before the derived key spaces gets when it
/// opens — derives the same data as the writes maintained, and a reopened
/// store keeps it.
#[test]
fn a_reindex_and_a_reopen_derive_the_same_data() {
    let (mut store, dir) = open();
    populate(&mut store);
    store.reindex().unwrap();
    check(&store);
    drop(store);
    let store = KvStore::open(dir.path()).unwrap();
    check(&store);
}

/// LMDB refuses a key over 511 bytes, and a value key holds the value: a long
/// text is keyed by its first bytes and a hash of the rest, and still
/// derives exactly — two long texts sharing their first bytes included.
#[test]
fn long_texts_are_derived_too() {
    let (mut store, _dir) = open();
    let long = "a".repeat(10_000);
    let mut w = Writer::begin(&mut store, None).unwrap();
    let root = w.create_metarecord(vec![Field::new("loc", tree(None, ""))]).unwrap().uuid;
    let one = w
        .create_metarecord(vec![
            Field::new("note", Value::String(format!("{long}1"))),
            Field::new("note", Value::String(format!("{long}2"))),
            Field::new("loc", tree(Some(root), &"n\0".repeat(300))),
        ])
        .unwrap()
        .uuid;
    w.commit().unwrap();
    check(&store);
    let mut w = Writer::begin(&mut store, None).unwrap();
    w.set_field(one, "note", Value::String(long.clone())).unwrap();
    w.commit().unwrap();
    check(&store);
}

/// `check` finds derived data that no longer matches the primary data — here
/// a set chunk deleted behind the store's back — and `reindex` repairs it
/// (spec-storage increment 5).
#[test]
fn check_finds_damaged_derived_data_and_reindex_repairs_it() {
    use metafolder_daemon::store::Begin;
    let (mut store, dir) = open();
    populate(&mut store);
    assert!(Begin::check(&store).unwrap().is_empty(), "a healthy store");
    drop(store);
    {
        // Damage: drop every set chunk (the universe, presence, parents).
        let env =
            unsafe { heed::EnvOpenOptions::new().max_dbs(32).map_size(1 << 30).open(dir.path()) }
                .unwrap();
        let mut w = env.write_txn().unwrap();
        let sets: heed::Database<heed::types::Bytes, heed::types::Bytes> =
            env.open_database(&w, Some("sets")).unwrap().unwrap();
        sets.clear(&mut w).unwrap();
        w.commit().unwrap();
    }
    let mut store = KvStore::open(dir.path()).unwrap();
    let problems = Begin::check(&store).unwrap();
    assert!(!problems.is_empty(), "the damage is reported");
    Begin::reindex(&mut store).unwrap();
    assert!(Begin::check(&store).unwrap().is_empty(), "reindex repairs it");
}

/// The file tree's descendant bitmaps follow every change: a subtree moved
/// under another folder, a node deleted, the log navigated back and forth.
#[test]
fn descendants_follow_moves_deletions_and_navigation() {
    let (mut store, _dir) = open();
    let path = |parent: Option<Uuid>, name: &str| {
        Field::new("mfr_path", Value::TreeRef { parent, name: TreeName::from(name) })
    };
    let mut w = Writer::begin(&mut store, None).unwrap();
    let root = w.create_metarecord(vec![path(None, "")]).unwrap().uuid;
    let a = w.create_metarecord(vec![path(Some(root), "a")]).unwrap().uuid;
    let b = w.create_metarecord(vec![path(Some(root), "b")]).unwrap().uuid;
    let sub = w.create_metarecord(vec![path(Some(a), "sub")]).unwrap().uuid;
    let file = w.create_metarecord(vec![path(Some(sub), "f.txt")]).unwrap().uuid;
    w.create_metarecord(vec![path(Some(sub), "g.txt")]).unwrap();
    w.commit().unwrap();
    check(&store);
    let before_move = store.head().unwrap();

    // Move `sub` (and its files) from `a` to `b`.
    let mut w = Writer::begin(&mut store, None).unwrap();
    w.set_field(sub, "mfr_path", Value::TreeRef { parent: Some(b), name: "sub".into() }).unwrap();
    w.commit().unwrap();
    check(&store);

    let mut w = Writer::begin(&mut store, None).unwrap();
    w.delete_metarecord(file).unwrap();
    w.commit().unwrap();
    check(&store);

    log::navigate(&mut store, before_move).unwrap();
    check(&store);
    log::navigate(&mut store, None).unwrap();
    check(&store);
    store.reindex().unwrap();
    check(&store);
}
