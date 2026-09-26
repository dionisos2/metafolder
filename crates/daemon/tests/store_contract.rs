//! The storage boundary's contract (docs/spec-storage.org "Increment 2,
//! concretely"): what any backend of `metafolder_daemon::store::Store` must
//! answer, asked only through the traits. SQLite is the one backend today; a
//! second one runs this same file.

use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::db;
use metafolder_daemon::log::Delta;
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
    let Delta::Found(delta) = store.ops_until(head, first, 10).unwrap() else {
        panic!("first is an ancestor of head");
    };
    assert_eq!(delta.len(), 2);
    assert_eq!(delta[0].id, head, "newest first");
    assert!(matches!(store.ops_until(head, first, 1).unwrap(), Delta::Budget));
    assert!(matches!(store.ops_until(first, head, 10).unwrap(), Delta::Unrelated));
    assert_eq!(store.ancestry(head).unwrap(), [head, delta[1].id, first]);
}

// ── Writing ───────────────────────────────────────────────────────────────────

use metafolder_daemon::log::{OpType, Retention};
use metafolder_daemon::store::{Begin, NewOp, Restoration};

fn empty() -> Connection {
    let conn = db::open_in_memory().unwrap();
    db::init_schema(&conn).unwrap();
    conn
}

fn s(v: &str) -> Value {
    Value::String(v.into())
}

#[test]
fn row_ids_are_never_reused_and_can_be_restored() {
    let mut conn = empty();
    let m = Uuid::new_v4();
    let (a, b) = {
        let tx = conn.begin_write().unwrap();
        tx.create_metarecord(m, 1).unwrap();
        let a = tx.insert_row(m, "k", &s("a"), None).unwrap();
        let b = tx.insert_row(m, "k", &s("b"), None).unwrap();
        tx.delete_row(b).unwrap();
        tx.commit().unwrap();
        (a, b)
    };
    assert!(b > a);
    let tx = conn.begin_write().unwrap();
    let c = tx.insert_row(m, "k", &s("c"), None).unwrap();
    assert!(c > b, "a deleted id is not handed out again");
    // Navigation puts a row back under its own id.
    let restored = tx.insert_row(m, "k", &s("b"), Some(b)).unwrap();
    assert_eq!(restored, b);
    assert_eq!(tx.row(b).unwrap().map(|r| r.value), Some(s("b")));
    let ids: Vec<i64> = tx.rows(m).unwrap().iter().map(|r| r.id).collect();
    assert_eq!(ids, [a, b, c], "row-id order");
    tx.commit().unwrap();
}

#[test]
fn a_forest_position_and_a_path_are_taken_once() {
    let mut conn = empty();
    let (root, x, y) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let tx = conn.begin_write().unwrap();
    for u in [root, x, y] {
        tx.create_metarecord(u, 1).unwrap();
    }
    tx.insert_row(root, "loc", &Value::TreeRef { parent: None, name: "r".into() }, None).unwrap();
    let under = |name: &str| Value::TreeRef { parent: Some(root), name: name.into() };
    tx.insert_row(x, "loc", &under("n"), None).unwrap();
    assert!(tx.insert_row(y, "loc", &under("n"), None).is_err(), "position taken");
    tx.insert_row(y, "loc", &under("m"), None).unwrap();
    let path = |name: &str| Value::TreeRef { parent: None, name: name.into() };
    tx.insert_row(x, "mfr_path", &path("p"), None).unwrap();
    assert!(tx.insert_row(x, "mfr_path", &path("q"), None).is_err(), "one path per metarecord");
    let mut kids = tx.children("loc", root).unwrap();
    kids.sort();
    let mut want = vec![(x, "n".to_string()), (y, "m".to_string())];
    want.sort();
    assert_eq!(kids, want);
}

#[test]
fn removing_a_metarecord_takes_its_rows() {
    let mut conn = empty();
    let m = Uuid::new_v4();
    let tx = conn.begin_write().unwrap();
    tx.create_metarecord(m, 7).unwrap();
    assert_eq!(tx.version(m).unwrap(), Some(7));
    tx.set_version(m, 9).unwrap();
    assert_eq!(tx.version(m).unwrap(), Some(9));
    let id = tx.insert_row(m, "k", &Value::Int(1), None).unwrap();
    tx.insert_row(m, "j", &Value::Int(2), None).unwrap();
    assert_eq!(tx.holders("k").unwrap(), [m]);
    assert_eq!(tx.value_types("k").unwrap(), ["int"]);
    tx.delete_rows(m, Some("j")).unwrap();
    assert_eq!(tx.rows(m).unwrap().len(), 1);
    tx.remove_metarecord(m).unwrap();
    assert_eq!(tx.version(m).unwrap(), None);
    assert_eq!(tx.row(id).unwrap().map(|r| r.id), None);
    assert!(tx.holders("k").unwrap().is_empty());
}

#[test]
fn appended_operations_chain_from_head() {
    let mut conn = empty();
    let m = Uuid::new_v4();
    let op = |field: &str| NewOp {
        op_type: OpType::SetField,
        entity: m,
        field_name: Some(field.into()),
        version_before: Some(1),
        version_after: Some(2),
        before: Vec::new(),
        after: vec![db::FieldRow { id: 5, name: field.into(), value: Value::Int(3) }],
        reverts_op_id: None,
    };
    let first = {
        let tx = conn.begin_write().unwrap();
        let rev = tx.begin_revision(Some("one"), 1_000).unwrap();
        let last = tx.append_ops(rev, None, 1, &[op("a"), op("b")]).unwrap();
        tx.set_head(Some(last)).unwrap();
        tx.commit().unwrap();
        last
    };
    let tx = conn.begin_write().unwrap();
    assert_eq!(tx.head().unwrap(), Some(first));
    let rev = tx.begin_revision(None, 2_000).unwrap();
    let last = tx.append_ops(rev, Some(first), 1, &[op("c")]).unwrap();
    tx.set_head(Some(last)).unwrap();
    let c = tx.op(last).unwrap().unwrap();
    assert_eq!((c.parent_id, c.rev_id, c.field_name.as_deref()), (Some(first), rev, Some("c")));
    assert_eq!(tx.snapshots(last, true).unwrap()[0].value, Value::Int(3));
    let b = tx.op(first).unwrap().unwrap();
    assert_eq!(b.field_name.as_deref(), Some("b"));
    assert_eq!(tx.op(b.parent_id.unwrap()).unwrap().unwrap().field_name.as_deref(), Some("a"));
    assert_eq!(tx.trim(Retention::UNLIMITED, last).unwrap(), 0);
    tx.commit().unwrap();
}

#[test]
fn an_uncommitted_transaction_leaves_nothing() {
    let mut conn = empty();
    let m = Uuid::new_v4();
    {
        let tx = conn.begin_write().unwrap();
        tx.create_metarecord(m, 1).unwrap();
        tx.insert_row(m, "k", &Value::Int(1), None).unwrap();
    }
    let store: &dyn Store = &conn;
    assert_eq!(store.version(m).unwrap(), None);
    assert!(store.metarecords().unwrap().is_empty());
}

#[test]
fn restorations_queue_in_order_and_leave_when_dropped() {
    let mut conn = empty();
    let (a, b, p) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let queued = [
        Restoration::SetPath { entity: a, parent: Some(p), name: "n".into() },
        Restoration::SetPath { entity: b, parent: None, name: "".into() },
        Restoration::ClearPath { entity: a },
        Restoration::ClearHashes { entity: b },
    ];
    let tx = conn.begin_write().unwrap();
    for r in &queued {
        tx.queue_restoration(r).unwrap();
    }
    let got = tx.restorations().unwrap();
    assert_eq!(got.iter().map(|(_, r)| r.clone()).collect::<Vec<_>>(), queued);
    assert!(got.windows(2).all(|w| w[0].0 < w[1].0), "queue order");
    tx.drop_restorations(got[1].0).unwrap();
    let left: Vec<Restoration> = tx.restorations().unwrap().into_iter().map(|(_, r)| r).collect();
    assert_eq!(left, &queued[2..]);
    tx.commit().unwrap();
}

#[test]
fn clearing_takes_every_metarecord() {
    let (mut conn, _, _) = fixture();
    let tx = conn.begin_write().unwrap();
    tx.clear_metarecords().unwrap();
    assert!(tx.metarecords().unwrap().is_empty());
    assert_eq!(tx.max_row_id().unwrap(), 0);
}

#[test]
fn the_log_lists_its_lines_and_revisions() {
    let (conn, _, _) = fixture();
    let store: &dyn Store = &conn;
    let head = store.head().unwrap().unwrap();
    let chain = store.ancestry_ops(head, None).unwrap();
    assert_eq!(chain.iter().map(|o| o.id).collect::<Vec<_>>(), store.ancestry(head).unwrap());
    assert_eq!(chain.len(), 3);
    assert_eq!(store.ancestry_ops(head, Some(2)).unwrap().len(), 2, "bounded walk");
    let all = store.all_ops().unwrap();
    assert_eq!(all.iter().map(|o| o.id).rev().collect::<Vec<_>>(), store.ancestry(head).unwrap());
    assert_eq!(store.active_line(head).unwrap().len(), 3);
    assert!(!store.has_children(head).unwrap());
    assert!(store.has_children(chain[1].id).unwrap());

    let revs: Vec<i64> = {
        let mut r: Vec<i64> = all.iter().map(|o| o.rev_id).collect();
        r.dedup();
        r
    };
    assert_eq!(revs.len(), 2);
    let meta = store.revisions(&[revs[0], revs[1], 999_999]).unwrap();
    assert_eq!(meta.len(), 2, "an id naming no revision is left out");
    assert!(meta[&revs[0]].timestamp <= meta[&revs[1]].timestamp);
    assert_eq!(meta[&revs[0]].origin, None, "a client's write");
    assert_eq!(store.counts().unwrap(), (3, 2));
}

#[test]
fn a_revision_names_its_operations_and_takes_a_label() {
    let (mut conn, _, child) = fixture();
    let store: &dyn Store = &conn;
    let all = store.all_ops().unwrap();
    let (first_rev, second_rev) = (all[0].rev_id, all[2].rev_id);
    let ids =
        |ops: Vec<metafolder_daemon::log::OpRow>| ops.iter().map(|o| o.id).collect::<Vec<_>>();
    assert_eq!(ids(store.revision_ops(first_rev).unwrap()), [all[0].id, all[1].id]);
    assert_eq!(ids(store.revision_ops(second_rev).unwrap()), [all[2].id]);
    assert!(store.revision_ops(999_999).unwrap().is_empty());
    assert_eq!(ids(store.entity_ops_after(child, 0).unwrap()), [all[1].id, all[2].id]);
    assert_eq!(ids(store.entity_ops_after(child, all[1].id).unwrap()), [all[2].id]);

    let tx = conn.begin_write().unwrap();
    assert!(tx.set_revision_label(first_rev, Some("before")).unwrap());
    assert!(!tx.set_revision_label(999_999, Some("x")).unwrap(), "no such revision");
    tx.commit().unwrap();
    let store: &dyn Store = &conn;
    assert_eq!(store.revisions(&[first_rev]).unwrap()[&first_rev].label.as_deref(), Some("before"));
}

#[test]
fn a_child_is_found_by_its_bytes_or_its_text() {
    let mut conn = empty();
    let (root, a, e) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let tx = conn.begin_write().unwrap();
    for u in [root, a, e] {
        tx.create_metarecord(u, 1).unwrap();
    }
    tx.insert_row(root, "loc", &Value::TreeRef { parent: None, name: "".into() }, None).unwrap();
    let under = |name: &str| Value::TreeRef { parent: Some(root), name: name.into() };
    tx.insert_row(a, "loc", &under("Photos"), None).unwrap();
    tx.insert_row(e, "loc", &under("Été"), None).unwrap();

    assert_eq!(tx.child_by_bytes("loc", None, b"").unwrap(), Some(root));
    assert_eq!(tx.child_by_bytes("loc", Some(root), b"Photos").unwrap(), Some(a));
    assert_eq!(tx.child_by_bytes("loc", Some(root), b"photos").unwrap(), None);
    assert_eq!(tx.child_by_text("loc", Some(root), "Photos", false).unwrap(), Some(a));
    assert_eq!(tx.child_by_text("loc", Some(root), "photos", false).unwrap(), None);
    assert_eq!(tx.child_by_text("loc", Some(root), "PHOTOS", true).unwrap(), Some(a));
    // NOCASE folds ASCII letters only.
    assert_eq!(tx.child_by_text("loc", Some(root), "ÉTÉ", true).unwrap(), None);
    assert_eq!(tx.child_by_text("loc", Some(root), "Été", true).unwrap(), Some(e));

    assert_eq!(tx.positions("loc", a).unwrap(), [(Some(root), "Photos".to_string())]);
    assert_eq!(tx.positions("loc", root).unwrap(), [(None, String::new())]);
    assert!(tx.positions("other", a).unwrap().is_empty());
}
