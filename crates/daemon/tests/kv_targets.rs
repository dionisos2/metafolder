//! What points at a metarecord — a `ref`'s referent, a position's parent —
//! is filed in the indexes under the target's dense id (doc "Store tables"):
//! following a reference reads no uuid. A dense id is not a metarecord's for
//! life — one deleted and created again gets another, a reindex numbers them
//! all again — so what is filed under an id must follow.

use metafolder_core::metarecord::{Field, TreeName, Value};
use metafolder_core::query::{Aspect, FollowTarget, Query};
use metafolder_daemon::index::{PageStrategy, QueryRoots, SortBy};
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::Writer;
use uuid::Uuid;

mod common;
use common::TempDir;

fn open() -> (KvStore, TempDir) {
    let dir = TempDir::new("kv-targets");
    let store = KvStore::open_unsynced(dir.path()).unwrap();
    (store, dir)
}

fn add(store: &mut KvStore, fields: Vec<Field>) -> Uuid {
    let mut w = Writer::begin(store, None).unwrap();
    let uuid = w.create_metarecord(fields).unwrap().uuid;
    w.commit().unwrap();
    uuid
}

fn tree(parent: Option<Uuid>, name: &str) -> Field {
    Field::new("loc", Value::TreeRef { parent, name: TreeName::from(name) })
}

fn check(store: &KvStore) {
    let diff = store.check_derived().unwrap();
    assert!(diff.is_empty(), "derived data diverges from a rebuild:\n{}", diff.join("\n"));
}

/// The metarecords whose `field` points at one matching `target`, as the
/// oracle and the serving path both answer.
fn pointing_at(store: &KvStore, field: &str, target: Uuid) -> Vec<Uuid> {
    let q = Query::Follows {
        field: field.into(),
        target: FollowTarget::Condition(Box::new(Query::UuidIn { uuids: vec![target] })),
    };
    common::engines::both(store, &q, &[])
}

#[test]
fn a_reference_is_filed_once_per_key_space_it_sorts_in() {
    let (mut store, _dir) = open();
    let root = add(&mut store, vec![tree(None, "")]);
    // A position: its value and its name are postings, and no entry goes by
    // its parent, which the parent's referrers already answer.
    let folder = add(&mut store, vec![tree(Some(root), "dir")]);
    // A reference: its value, a posting.
    add(&mut store, vec![Field::new("link", Value::Ref(folder))]);
    check(&store);
}

#[test]
fn a_target_deleted_and_created_again_keeps_what_points_at_it() {
    let (mut store, _dir) = open();
    let target = add(&mut store, vec![Field::new("title", Value::String("target".into()))]);
    let other = add(&mut store, vec![Field::new("title", Value::String("other".into()))]);
    let pointing = add(&mut store, vec![Field::new("link", Value::Ref(target))]);
    add(&mut store, vec![Field::new("link", Value::Ref(other))]);
    assert_eq!(pointing_at(&store, "link", target), vec![pointing]);

    let mut w = Writer::begin(&mut store, None).unwrap();
    w.delete_metarecord(target).unwrap();
    w.commit().unwrap();
    assert!(pointing_at(&store, "link", target).is_empty(), "nothing to point at");
    check(&store);

    // Created again — what a revert of the deletion does — it has another
    // dense id, and the reference filed under the former one follows.
    let mut w = Writer::begin(&mut store, None).unwrap();
    w.create_metarecord_with_uuid(target, vec![Field::new("title", Value::String("back".into()))])
        .unwrap();
    w.commit().unwrap();
    assert_eq!(pointing_at(&store, "link", target), vec![pointing]);
    check(&store);

    // One more pointing at it, and again after a reindex.
    let late = add(&mut store, vec![Field::new("link", Value::Ref(target))]);
    let mut both = vec![pointing, late];
    both.sort();
    assert_eq!(pointing_at(&store, "link", target), both);
    store.reindex().unwrap();
    assert_eq!(pointing_at(&store, "link", target), both);
    check(&store);
}

#[test]
fn a_reference_to_a_metarecord_created_later_points_at_it() {
    let (mut store, _dir) = open();
    let later = Uuid::new_v4();
    let pointing = add(&mut store, vec![Field::new("link", Value::Ref(later))]);
    check(&store);
    assert!(pointing_at(&store, "link", later).is_empty());

    let mut w = Writer::begin(&mut store, None).unwrap();
    w.create_metarecord_with_uuid(later, vec![Field::new("n", Value::Int(1))]).unwrap();
    w.commit().unwrap();
    assert_eq!(pointing_at(&store, "link", later), vec![pointing]);
    // It is a metarecord like another: not one that the indexes of a field it
    // never held would name.
    let q = Query::IsPresent { field: "link".into(), aspect: Aspect::Raw };
    assert_eq!(common::engines::both(&store, &q, &[]), vec![pointing]);
    check(&store);
}

#[test]
fn a_folder_created_again_keeps_its_place_in_the_tree() {
    let (mut store, _dir) = open();
    let root = add(&mut store, vec![tree(None, "")]);
    let folder = add(&mut store, vec![tree(Some(root), "dir")]);
    let file = add(&mut store, vec![tree(Some(folder), "a.txt")]);
    // The file first, then its folder; then both back, the folder first.
    let mut w = Writer::begin(&mut store, None).unwrap();
    w.delete_metarecord(file).unwrap();
    w.delete_metarecord(folder).unwrap();
    w.commit().unwrap();
    let mut w = Writer::begin(&mut store, None).unwrap();
    w.create_metarecord_with_uuid(folder, vec![tree(Some(root), "dir")]).unwrap();
    w.create_metarecord_with_uuid(file, vec![tree(Some(folder), "a.txt")]).unwrap();
    w.commit().unwrap();
    check(&store);

    let below = |path: &str| Query::FollowsTransitive {
        field: "loc".into(),
        target: FollowTarget::Path(path.into()),
        inclusive: false,
    };
    assert_eq!(common::engines::both(&store, &below("/dir"), &[]), vec![file]);
    let mut all = vec![folder, file];
    all.sort();
    assert_eq!(common::engines::both(&store, &below("/"), &[]), all);
}

/// A cursor names metarecords by dense id (doc "Sorting and postings"), and
/// a reindex numbers them again: a cursor from before it is refused, not
/// read as naming whichever metarecords hold those ids now.
#[test]
fn a_cursor_from_before_a_reindex_is_refused() {
    let (mut store, _dir) = open();
    for i in 0..30 {
        add(&mut store, vec![Field::new("n", Value::Int(i))]);
    }
    let q = Query::IsPresent { field: "n".into(), aspect: Aspect::Raw };
    let sort = [SortBy { field: "n".into(), ascending: true }];
    let page = |store: &KvStore, cursor: Option<&str>| {
        common::kv::with_kv(store, PageStrategy::Auto, |e, _| {
            e.evaluate_page_with_roots(&q, &sort, Some(10), cursor, &QueryRoots::new())
        })
    };
    let (_, cursor) = page(&store, None).unwrap();
    let cursor = cursor.expect("more pages");
    assert!(page(&store, Some(&cursor)).is_ok());

    store.reindex().unwrap();
    let refused = page(&store, Some(&cursor)).expect_err("a cursor of the former numbering");
    assert!(refused.is_cursor(), "{refused}");
}
