//! The key-value store's derived key spaces (doc "Store tables"):
//! maintained row by row inside every write transaction, they must
//! always equal what a rebuild from the primary data gives — after ordinary
//! writes, deletions, retypes and renames, and after the log's navigation,
//! which rewrites rows by the same primitives.

use metafolder_core::metarecord::{Field, TreeName, Value};
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::Writer;
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
fn reverting_keeps_the_derived_data_exact() {
    let (mut store, _dir) = open();
    let start = store.head().unwrap();
    let (_, _, file) = populate(&mut store);
    let populated = store.head().unwrap();

    let mut w = Writer::begin(&mut store, None).unwrap();
    w.set_field(file, "size", Value::Int(1)).unwrap();
    w.commit().unwrap();
    check(&store);

    // Back to the populated state, then before it, then that undone in turn.
    common::kv::revert_since(&mut store, populated);
    check(&store);
    let undone_all = store.head().unwrap();
    common::kv::revert_since(&mut store, start);
    check(&store);
    common::kv::revert_since(&mut store, undone_all);
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
            Field::new("loc", tree(Some(root), &"n\u{1}".repeat(300))),
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
/// (doc "Checking and reindexing a repository").
#[test]
fn check_finds_damaged_derived_data_and_reindex_repairs_it() {
    use metafolder_daemon::store::Begin;
    let (mut store, dir) = open();
    populate(&mut store);
    assert!(Begin::check(&store).unwrap().is_empty(), "a healthy store");
    drop(store);
    {
        // Damage: drop every set chunk (the universe, presence, parents).
        let env = common::kv::raw_env(dir.path());
        let sets = common::kv::open_raw(&env, "sets");
        let mut w = env.write_txn().unwrap();
        sets.clear(&mut w).unwrap();
        w.commit().unwrap();
    }
    let mut store = KvStore::open(dir.path()).unwrap();
    let problems = Begin::check(&store).unwrap();
    assert!(!problems.is_empty(), "the damage is reported");
    Begin::reindex(&mut store).unwrap();
    assert!(Begin::check(&store).unwrap().is_empty(), "reindex repairs it");
}

/// A forest's descendant bitmaps follow every change: a subtree moved under
/// another folder, a node deleted, those reverted — the file tree's, and any
/// other `tree_ref` field's (a tag tree).
#[test]
fn descendants_follow_moves_deletions_and_reverts() {
    for field in ["mfr_path", "path"] {
        descendants_follow_moves_deletions_and_reverts_in(field);
    }
}

fn descendants_follow_moves_deletions_and_reverts_in(field: &str) {
    let (mut store, _dir) = open();
    let path = |parent: Option<Uuid>, name: &str| {
        Field::new(field, Value::TreeRef { parent, name: TreeName::from(name) })
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
    w.set_field(sub, field, Value::TreeRef { parent: Some(b), name: "sub".into() }).unwrap();
    w.commit().unwrap();
    check(&store);

    let mut w = Writer::begin(&mut store, None).unwrap();
    w.delete_metarecord(file).unwrap();
    w.commit().unwrap();
    check(&store);

    common::kv::revert_since(&mut store, before_move);
    check(&store);
    store.reindex().unwrap();
    check(&store);
}

/// A store whose derived data predates the descendant bitmaps of every
/// forest (format 4: `mfr_path`'s only) is reindexed when it opens, and gains
/// the bitmaps of its other forests.
#[test]
fn a_store_of_the_previous_derived_format_is_reindexed_when_it_opens() {
    let (mut store, dir) = open();
    populate(&mut store);
    drop(store);
    {
        // As format 4 left it: no descendant bitmap outside `mfr_path` (the
        // populated forest is `loc`), and the format stamped 4.
        let env = common::kv::raw_env(dir.path());
        let sets = common::kv::open_raw(&env, "sets");
        let meta = common::kv::open_raw(&env, "meta");
        let mut w = env.write_txn().unwrap();
        let descendants: Vec<Vec<u8>> =
            sets.iter(&w).unwrap().map(|e| e.unwrap().0.to_vec()).filter(|k| k[0] == 6).collect();
        assert!(!descendants.is_empty(), "the `loc` forest has descendant bitmaps");
        for k in descendants {
            sets.delete(&mut w, &k).unwrap();
        }
        meta.put(&mut w, b"derived", &4u64.to_be_bytes()).unwrap();
        w.commit().unwrap();
    }
    let store = KvStore::open(dir.path()).unwrap();
    check(&store);
}

/// The dense ids, in id order, as the uuids they map.
fn in_id_order(store: &KvStore, uuids: &[Uuid]) -> Vec<Uuid> {
    use metafolder_daemon::index::Source;
    let src = store.source().unwrap();
    let mut by_id: Vec<(u32, Uuid)> = uuids.iter().map(|&u| (src.id(u).unwrap(), u)).collect();
    by_id.sort();
    by_id.into_iter().map(|(_, u)| u).collect()
}

/// A reindex numbers the metarecords in forest order — depth first, a
/// folder's children by name, the largest forest first — so that a subtree's
/// ids are consecutive however its records were discovered; the records
/// outside every forest come last (doc "Store tables").
#[test]
fn a_reindex_numbers_the_records_in_forest_order() {
    let (mut store, _dir) = open();
    let mut w = Writer::begin(&mut store, None).unwrap();
    let mut create = |fields: Vec<Field>| w.create_metarecord(fields).unwrap().uuid;
    // Discovered out of tree order: a loose record first, the deep file
    // before its siblings' folder.
    let loose = create(vec![Field::new("title", Value::String("loose".into()))]);
    let root = create(vec![Field::new("loc", tree(None, ""))]);
    let b = create(vec![Field::new("loc", tree(Some(root), "b"))]);
    let b_file = create(vec![Field::new("loc", tree(Some(b), "z.txt"))]);
    let a = create(vec![Field::new("loc", tree(Some(root), "a"))]);
    let a_file = create(vec![Field::new("loc", tree(Some(a), "y.txt"))]);
    // A smaller forest, and a record placed in both.
    let tag = create(vec![Field::new("hier", tree(None, "music"))]);
    let both = create(vec![
        Field::new("hier", tree(Some(tag), "rock")),
        Field::new("loc", tree(Some(a), "x.txt")),
    ]);
    w.commit().unwrap();

    store.reindex().unwrap();
    check(&store);
    let all = [loose, root, b, b_file, a, a_file, tag, both];
    assert_eq!(
        in_id_order(&store, &all),
        vec![root, a, both, a_file, b, b_file, tag, loose],
        "`loc` (6 positions) depth first by name, then `hier`, then the rest"
    );
}

/// The bytes the `sets` table holds, values only.
fn sets_bytes(dir: &std::path::Path) -> usize {
    let env = common::kv::raw_env(dir);
    let sets = common::kv::open_raw(&env, "sets");
    let r = env.read_txn().unwrap();
    sets.iter(&r).unwrap().map(|e| e.unwrap().1.len()).sum()
}

/// Twenty folders discovered interleaved — a file of each in turn — get
/// scattered ids; a reindex makes each folder's ids one run, and the stored
/// bitmaps keep runs as runs, so the `sets` table shrinks to a fraction
/// (doc "Store tables").
#[test]
fn a_reindex_gathers_a_scattered_folder_into_runs() {
    let (mut store, dir) = open();
    let mut w = Writer::begin(&mut store, None).unwrap();
    let root = w.create_metarecord(vec![Field::new("loc", tree(None, ""))]).unwrap().uuid;
    let dirs: Vec<Uuid> = (0..20)
        .map(|i| {
            w.create_metarecord(vec![Field::new("loc", tree(Some(root), &format!("d{i}")))])
                .unwrap()
                .uuid
        })
        .collect();
    for n in 0..4000 {
        w.create_metarecord(vec![Field::new("loc", tree(Some(dirs[n % 20]), &format!("f{n:04}")))])
            .unwrap();
    }
    w.commit().unwrap();
    drop(store);
    let before = sets_bytes(dir.path());
    let mut store = KvStore::open(dir.path()).unwrap();
    store.reindex().unwrap();
    check(&store);
    drop(store);
    let after = sets_bytes(dir.path());
    assert!(
        after * 10 < before,
        "the sets shrink from {before} bytes to a tenth at most, got {after}"
    );
}

/// A compact rewrites the store without its free pages — here those of a
/// bulk load mostly deleted again — and the store goes on serving and
/// writing, in its own file (doc "Compacting a repository").
#[test]
fn a_compact_shrinks_the_file_and_keeps_the_store_whole() {
    use metafolder_daemon::index::Source;
    let (mut store, dir) = open();
    let mut w = Writer::begin(&mut store, None).unwrap();
    let uuids: Vec<Uuid> = (0..3000)
        .map(|n| {
            let text = format!("a text long enough to fill pages, number {n:05}, ").repeat(8);
            w.create_metarecord(vec![Field::new("title", Value::String(text))]).unwrap().uuid
        })
        .collect();
    w.commit().unwrap();
    let mut w = Writer::begin(&mut store, None).unwrap();
    for &u in &uuids[100..] {
        w.delete_metarecord(u).unwrap();
    }
    w.commit().unwrap();
    let file = dir.path().join("data.mdb");
    let before = std::fs::metadata(&file).unwrap().len();

    let compacted = store.compact().unwrap();
    assert_eq!(compacted.before, before);
    assert_eq!(compacted.after, std::fs::metadata(&file).unwrap().len(), "the store's own file");
    // The log keeps the deleted records' snapshots: a third goes, not all.
    assert!(
        compacted.after * 3 < before * 2,
        "{before} bytes shrink by a third, to {}",
        compacted.after
    );
    check(&store);
    assert_eq!(store.source().unwrap().universe().len(), 100);
    assert!(store.metarecord(uuids[7]).unwrap().is_some());

    // Still a store: it writes, and reopens with what it wrote.
    let mut w = Writer::begin(&mut store, None).unwrap();
    let late =
        w.create_metarecord(vec![Field::new("title", Value::String("late".into()))]).unwrap().uuid;
    w.commit().unwrap();
    drop(store);
    let store = KvStore::open(dir.path()).unwrap();
    check(&store);
    assert!(store.metarecord(late).unwrap().is_some());
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| !["data.mdb", "lock.mdb", "daemon.lock"].contains(&n.as_str()))
        .collect();
    assert!(leftovers.is_empty(), "nothing left beside the store: {leftovers:?}");
}
