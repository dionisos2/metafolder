//! The storage boundary's contract (docs/spec-storage.org "Increment 2,
//! concretely"): what any backend of `metafolder_daemon::store::Store` must
//! answer, asked only through the traits. SQLite is the one backend today; a
//! second one runs this same file.

use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::db;
use metafolder_daemon::log::Writer;
use metafolder_daemon::store::Store;
use rusqlite::Connection;
use uuid::Uuid;

fn tref(parent: Option<Uuid>, name: &str) -> Field {
    Field::new("loc", Value::TreeRef { parent, name: name.into() })
}

/// A root, a child with two string rows, and a later revision rewriting one
/// field: three operations over two revisions.
fn fixture() -> (Connection, Uuid, Uuid) {
    let mut conn = db::open_in_memory().unwrap();
    db::init_schema(&conn).unwrap();
    let mut w = Writer::begin(&mut conn, None).unwrap();
    let root = w.create_metarecord(vec![tref(None, "root")]).unwrap().uuid;
    let child = w
        .create_metarecord(vec![
            tref(Some(root), "child"),
            Field::new("tag", Value::String("a".into())),
            Field::new("tag", Value::String("b".into())),
        ])
        .unwrap()
        .uuid;
    w.commit().unwrap();
    let mut w = Writer::begin(&mut conn, None).unwrap();
    w.set_field(child, "rating", Value::Int(3)).unwrap();
    w.commit().unwrap();
    (conn, root, child)
}

#[test]
fn rows_answer_what_was_written() {
    let (conn, root, child) = fixture();
    let store: &dyn Store = &conn;

    let rows = store.rows(child).unwrap();
    let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["loc", "tag", "tag", "rating"], "in row-id order");
    let tags = store.rows_named(child, "tag").unwrap();
    assert_eq!(
        tags.iter().map(|r| r.value.clone()).collect::<Vec<_>>(),
        [Value::String("a".into()), Value::String("b".into())]
    );
    assert!(store.rows_named(child, "none").unwrap().is_empty());
    assert!(store.rows(Uuid::new_v4()).unwrap().is_empty());

    // A row by its id, and whose it is.
    let id = tags[1].id;
    assert_eq!(store.row(id).unwrap().map(|r| r.value), Some(Value::String("b".into())));
    assert_eq!(store.owner_of_row(id).unwrap(), Some(child));
    assert_eq!(store.row(i64::MAX).unwrap().map(|r| r.id), None);

    assert!(store.version(child).unwrap().is_some());
    assert_eq!(store.version(Uuid::new_v4()).unwrap(), None);

    let mut all = store.metarecords().unwrap();
    all.sort();
    let mut want = vec![root, child];
    want.sort();
    assert_eq!(all, want);
}

#[test]
fn scans_cover_every_row_in_id_order() {
    let (conn, root, child) = fixture();
    let store: &dyn Store = &conn;
    let mut seen = Vec::new();
    store
        .for_each_row(&mut |uuid, row| {
            seen.push((row.id, uuid));
            Ok(())
        })
        .unwrap();
    assert_eq!(seen.len(), 5);
    assert!(seen.windows(2).all(|w| w[0].0 < w[1].0), "id order");
    assert_eq!(store.max_row_id().unwrap(), seen.last().unwrap().0);

    let forest = store.forest().unwrap();
    let mut placed: Vec<(Uuid, Option<Uuid>)> = forest.iter().map(|t| (t.uuid, t.parent)).collect();
    placed.sort();
    let mut want = vec![(root, None), (child, Some(root))];
    want.sort();
    assert_eq!(placed, want);
}

#[test]
fn the_log_walks_back_from_head() {
    let (conn, _, child) = fixture();
    let store: &dyn Store = &conn;
    let head = store.head().unwrap().expect("a head after three writes");
    let last = store.op(head).unwrap().expect("the head operation");
    assert_eq!(last.entity_uuid, child);
    assert_eq!(last.field_name.as_deref(), Some("rating"));
    let after = store.snapshots(head, true).unwrap();
    assert_eq!(after.iter().map(|r| r.value.clone()).collect::<Vec<_>>(), [Value::Int(3)]);
    assert!(store.snapshots(head, false).unwrap().is_empty(), "no rating before");

    // The walk back from HEAD to the first operation: the two between.
    let first = store.op(last.parent_id.unwrap()).unwrap().unwrap().parent_id.unwrap();
    let delta = store.ops_until(head, first, 10).unwrap().expect("first is an ancestor");
    assert_eq!(delta.len(), 2);
    assert_eq!(delta[0].id, head, "newest first");
    assert_eq!(store.ops_until(head, first, 1).unwrap().map(|d| d.len()), None, "over budget");
}
