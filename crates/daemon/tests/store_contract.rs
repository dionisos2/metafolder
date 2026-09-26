//! The storage boundary's contract (docs/spec-storage.org "Increment 2,
//! concretely"): what any backend of `metafolder_daemon::store::Store` must
//! answer, asked only through the traits. SQLite is the one backend today; a
//! second one runs this same file.

use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::db;
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::Delta;
use metafolder_daemon::log::Writer;
use metafolder_daemon::store::Handle;
use metafolder_daemon::store::Store;
use uuid::Uuid;

mod common;
use common::TempDir;

/// The two backends every test here runs on.
#[derive(Clone, Copy, Debug)]
enum Backend {
    Sqlite,
    Kv,
}

/// An empty database of the given backend (and the directory a KV store
/// lives in, removed when dropped).
fn open(backend: Backend) -> (Handle, Option<TempDir>) {
    match backend {
        Backend::Sqlite => {
            let conn = db::open_in_memory().unwrap();
            db::init_schema(&conn).unwrap();
            (Box::new(conn), None)
        }
        Backend::Kv => {
            let dir = TempDir::new("store-contract-kv");
            (Box::new(KvStore::open(dir.path()).unwrap()), Some(dir))
        }
    }
}

fn tref(parent: Option<Uuid>, name: &str) -> Field {
    Field::new("loc", Value::TreeRef { parent, name: name.into() })
}

/// A root, a child with two string rows, and a later revision rewriting one
/// field: three operations over two revisions.
fn fixture(backend: Backend) -> (Handle, Option<TempDir>, Uuid, Uuid) {
    let (mut conn, dir) = open(backend);
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
    (conn, dir, root, child)
}

fn rows_answer_what_was_written(backend: Backend) {
    let (conn, _dir, root, child) = fixture(backend);
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
    let field: Vec<(Uuid, Value)> =
        store.field_rows("tag").unwrap().into_iter().map(|(u, r)| (u, r.value)).collect();
    assert_eq!(field, [(child, Value::String("a".into())), (child, Value::String("b".into()))]);
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

fn scans_cover_every_row_in_id_order(backend: Backend) {
    let (conn, _dir, root, child) = fixture(backend);
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

fn the_log_walks_back_from_head(backend: Backend) {
    let (conn, _dir, _, child) = fixture(backend);
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

fn empty(backend: Backend) -> (Handle, Option<TempDir>) {
    open(backend)
}

fn s(v: &str) -> Value {
    Value::String(v.into())
}

fn row_ids_are_never_reused_and_can_be_restored(backend: Backend) {
    let (mut conn, _dir) = empty(backend);
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

fn a_forest_position_and_a_path_are_taken_once(backend: Backend) {
    let (mut conn, _dir) = empty(backend);
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

fn removing_a_metarecord_takes_its_rows(backend: Backend) {
    let (mut conn, _dir) = empty(backend);
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

fn appended_operations_chain_from_head(backend: Backend) {
    let (mut conn, _dir) = empty(backend);
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

fn an_uncommitted_transaction_leaves_nothing(backend: Backend) {
    let (mut conn, _dir) = empty(backend);
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

fn restorations_queue_in_order_and_leave_when_dropped(backend: Backend) {
    let (mut conn, _dir) = empty(backend);
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

fn clearing_takes_every_metarecord(backend: Backend) {
    let (mut conn, _dir, _, _) = fixture(backend);
    let tx = conn.begin_write().unwrap();
    tx.clear_metarecords().unwrap();
    assert!(tx.metarecords().unwrap().is_empty());
    assert_eq!(tx.max_row_id().unwrap(), 0);
}

fn the_log_lists_its_lines_and_revisions(backend: Backend) {
    let (conn, _dir, _, _) = fixture(backend);
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

fn a_revision_names_its_operations_and_takes_a_label(backend: Backend) {
    let (mut conn, _dir, _, child) = fixture(backend);
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

fn a_child_is_found_by_its_bytes_or_its_text(backend: Backend) {
    let (mut conn, _dir) = empty(backend);
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

// ── Questions: every SQLite answer is its derived default ─────────────────────

use metafolder_daemon::store::{Derived, Questions};

/// A repository touching every question.
fn questions_fixture(backend: Backend) -> (Handle, Option<TempDir>, Uuid) {
    let (mut conn, dir) = empty(backend);
    let mut w = Writer::begin(&mut conn, None).unwrap();
    let root = w
        .create_metarecord(vec![Field::new(
            "mfr_path",
            Value::TreeRef { parent: None, name: "".into() },
        )])
        .unwrap()
        .uuid;
    let path = |name: &str| {
        Field::new("mfr_path", Value::TreeRef { parent: Some(root), name: name.into() })
    };
    let int = |n: &str, v: i64| Field::new(n, Value::Int(v));
    let st = |n: &str, v: &str| Field::new(n, s(v));
    let group = w
        .create_metarecord(vec![
            st("mfr_content_hash", "h1"),
            int("mfr_content_size", 10),
            int("mfr_duplicate_count", 2),
        ])
        .unwrap()
        .uuid;
    w.create_metarecord(vec![st("mfr_content_hash", "h2"), int("mfr_content_size", 20)]).unwrap();
    for (i, name) in ["a", "b", "c"].iter().enumerate() {
        let mut fields = vec![
            path(name),
            st("mfr_type", "file"),
            int("mfr_size", 10 * (i as i64 + 1)),
            st("mfr_partial_hash", &format!("p{i}")),
            int("mfr_hash_mtime", 100 + i as i64),
            Field::new("mf_watch_exceeded", Value::Bool(i % 2 == 0)),
            st("tag", "x"),
            st("tag", &format!("y{i}")),
        ];
        if i < 2 {
            fields.push(int("mfr_hash_size", 10));
            fields.push(st("mfr_full_hash", &format!("f{i}")));
            fields.push(Field::new("mfr_duplicate_group", Value::Ref(group)));
        }
        w.create_metarecord(fields).unwrap();
    }
    // Orphans: one hashed, one not; a directory.
    w.create_metarecord(vec![
        Field::new("mfr_path", Value::Nothing),
        int("mfr_size", 7),
        st("mfr_partial_hash", "op"),
        st("mfr_full_hash", "of"),
    ])
    .unwrap();
    w.create_metarecord(vec![Field::new("mfr_path", Value::Nothing), int("mfr_size", 8)]).unwrap();
    w.create_metarecord(vec![path("dir"), st("mfr_type", "directory")]).unwrap();
    // Schema material: declared types, a wrongly typed row, counts.
    w.create_metarecord(vec![st("mf_schema", "song"), st("title", "t"), st("title", "u")]).unwrap();
    w.create_metarecord(vec![st("mf_schema", "song")]).unwrap();
    w.create_metarecord(vec![st("mf_schema", "film"), st("title", "v")]).unwrap();
    w.commit().unwrap();
    (conn, dir, group)
}

fn sorted<T: Ord>(mut v: Vec<T>) -> Vec<T> {
    v.sort();
    v
}

#[test]
fn every_sqlite_answer_is_its_derived_default() {
    let (conn, _dir, group) = questions_fixture(Backend::Sqlite);
    let sql: &dyn Store = &conn;
    let derived = Derived(conn.as_sqlite().unwrap());
    for (name, value) in [
        ("mf_watch_exceeded", Value::Bool(true)),
        ("mf_watch_exceeded", Value::Bool(false)),
        ("mfr_path", Value::Nothing),
        ("tag", s("x")),
    ] {
        assert_eq!(
            sorted(sql.holding(name, &value).unwrap()),
            sorted(derived.holding(name, &value).unwrap()),
            "holding {name} = {value:?}"
        );
    }
    assert!(!sql.holding("mf_watch_exceeded", &Value::Bool(true)).unwrap().is_empty());
    assert_eq!(sql.string_owners("tag").unwrap(), derived.string_owners("tag").unwrap());
    assert_eq!(
        sql.ref_map("mfr_duplicate_group").unwrap(),
        derived.ref_map("mfr_duplicate_group").unwrap()
    );
    assert_eq!(sql.hash_cache().unwrap(), derived.hash_cache().unwrap());
    assert_eq!(
        sorted(sql.tracked_files_with_size().unwrap()),
        sorted(derived.tracked_files_with_size().unwrap())
    );
    assert_eq!(sql.tracked_files_with_size().unwrap().len(), 3);
    assert_eq!(sql.duplicate_groups().unwrap(), derived.duplicate_groups().unwrap());
    assert_eq!(
        sorted(sql.duplicate_group_members(group).unwrap()),
        sorted(derived.duplicate_group_members(group).unwrap())
    );
    let key = |o: &metafolder_daemon::db::OrphanCandidate| (o.uuid, o.size);
    let mut a = sql.hashed_orphans().unwrap();
    let mut b = derived.hashed_orphans().unwrap();
    a.sort_by_key(key);
    b.sort_by_key(key);
    assert_eq!(a, b);
    assert_eq!(a.len(), 1);
    for (field, allowed) in [("title", "string"), ("title", "int"), ("tag", "string")] {
        assert_eq!(
            sorted(sql.wrong_type(field, allowed, 100).unwrap()),
            sorted(derived.wrong_type(field, allowed, 100).unwrap()),
            "wrong_type {field} {allowed}"
        );
    }
    for n in [0, 1, 2, 3] {
        assert_eq!(
            sorted(sql.count_over("title", n, 100).unwrap()),
            sorted(derived.count_over("title", n, 100).unwrap()),
            "count_over {n}"
        );
        assert_eq!(
            sorted(sql.count_under("title", n, 100).unwrap()),
            sorted(derived.count_under("title", n, 100).unwrap()),
            "count_under {n}"
        );
    }
    assert_eq!(
        sorted(sql.missing("title", 100).unwrap()),
        sorted(derived.missing("title", 100).unwrap())
    );
    let types = vec!["song".to_string()];
    assert_eq!(
        sorted(sql.typed_missing(&types, "title", 100).unwrap()),
        sorted(derived.typed_missing(&types, "title", 100).unwrap())
    );
    assert_eq!(sql.typed_missing(&types, "title", 100).unwrap().len(), 1);
    assert_eq!(sql.missing("title", 2).unwrap().len(), 2, "the limit holds");
    assert_eq!(derived.missing("title", 2).unwrap().len(), 2);
}

fn targets_are_found_along_the_ancestry(backend: Backend) {
    let (mut conn, _dir, _, _) = fixture(backend);
    let store: &dyn Store = &conn;
    let all = store.all_ops().unwrap();
    let head = store.head().unwrap().unwrap();
    assert_eq!(store.ops_after(all[0].id).unwrap().len(), 2);
    assert_eq!(store.ops_after_count(all[0].id).unwrap(), 2);
    assert_eq!(store.ops_after_count(head).unwrap(), 0);
    // Before the second revision: the last operation of the first.
    assert_eq!(store.before_revision_of(head).unwrap(), Some(all[1].id));
    assert_eq!(store.before_revision_of(all[1].id).unwrap(), None, "before the first: empty");
    let meta = store.revisions(&[all[0].rev_id, all[2].rev_id]).unwrap();
    let first_ts = meta[&all[0].rev_id].timestamp;
    assert_eq!(store.ancestor_at_or_before(head, i64::MAX).unwrap(), Some(head));
    assert_eq!(store.ancestor_at_or_before(head, first_ts - 1).unwrap(), None);
    assert!(store.ancestor_at_or_before(head, first_ts).unwrap().is_some());

    let tx = conn.begin_write().unwrap();
    tx.set_revision_label(all[0].rev_id, Some("start")).unwrap();
    tx.commit().unwrap();
    let store: &dyn Store = &conn;
    assert_eq!(store.ancestor_labelled(head, "start").unwrap(), Some(all[1].id), "its last op");
    assert_eq!(store.ancestor_labelled(head, "none").unwrap(), None);
}

fn pruning_removes_operations_and_their_empty_revisions(backend: Backend) {
    let (mut conn, _dir, _, _) = fixture(backend);
    let all = (&conn as &dyn Store).all_ops().unwrap();
    let tx = conn.begin_write().unwrap();
    tx.detach_op(all[2].id).unwrap();
    tx.delete_ops(&[all[1].id, all[0].id]).unwrap();
    tx.drop_empty_revisions().unwrap();
    tx.commit().unwrap();
    let store: &dyn Store = &conn;
    assert_eq!(store.counts().unwrap(), (1, 1));
    assert_eq!(store.op(all[2].id).unwrap().unwrap().parent_id, None);
    assert!(store.snapshots(all[0].id, true).unwrap().is_empty(), "snapshots go too");
    conn.compact().unwrap();
}

/// A name too long to key whole (LMDB caps a key at 511 bytes) is still one
/// position: found by its bytes, listed and read back whole — and a name
/// sharing its first bytes is another position.
fn a_long_name_is_a_position_like_any(backend: Backend) {
    let (mut conn, _dir) = open(backend);
    let long = "x".repeat(1_000);
    let (twin, sibling) = (format!("{long}-twin"), format!("{long}-sibling"));
    let mut w = Writer::begin(&mut conn, None).unwrap();
    let root = w.create_metarecord(vec![tref(None, "root")]).unwrap().uuid;
    let node = w.create_metarecord(vec![tref(Some(root), &twin)]).unwrap().uuid;
    w.commit().unwrap();
    let store: &dyn Store = &conn;
    assert_eq!(store.child_by_bytes("loc", Some(root), twin.as_bytes()).unwrap(), Some(node));
    assert_eq!(store.child_by_bytes("loc", Some(root), sibling.as_bytes()).unwrap(), None);
    assert_eq!(store.children("loc", root).unwrap(), vec![(node, twin.clone())]);
    let names: Vec<String> =
        store.forest().unwrap().iter().map(|r| r.name.display().into_owned()).collect();
    assert!(names.contains(&twin), "the forest reads the whole name back");

    let mut w = Writer::begin(&mut conn, None).unwrap();
    let other = w.create_metarecord(vec![tref(Some(root), &sibling)]).unwrap().uuid;
    w.commit().unwrap();
    let store: &dyn Store = &conn;
    assert_eq!(store.child_by_bytes("loc", Some(root), sibling.as_bytes()).unwrap(), Some(other));
    assert_eq!(store.children("loc", root).unwrap().len(), 2);
}

/// Every test above, on each backend.
macro_rules! on_both {
    ($($name:ident),* $(,)?) => {
        mod sqlite {
            $(#[test] fn $name() { super::$name(super::Backend::Sqlite) })*
        }
        mod kv {
            $(#[test] fn $name() { super::$name(super::Backend::Kv) })*
        }
    };
}

on_both!(
    rows_answer_what_was_written,
    scans_cover_every_row_in_id_order,
    the_log_walks_back_from_head,
    row_ids_are_never_reused_and_can_be_restored,
    a_forest_position_and_a_path_are_taken_once,
    removing_a_metarecord_takes_its_rows,
    appended_operations_chain_from_head,
    an_uncommitted_transaction_leaves_nothing,
    restorations_queue_in_order_and_leave_when_dropped,
    clearing_takes_every_metarecord,
    the_log_lists_its_lines_and_revisions,
    a_revision_names_its_operations_and_takes_a_label,
    a_child_is_found_by_its_bytes_or_its_text,
    targets_are_found_along_the_ancestry,
    pruning_removes_operations_and_their_empty_revisions,
    a_long_name_is_a_position_like_any
);

// ── The key-value store's map ───────────────────────────────────────────────
//
// LMDB maps its whole file into the address space, reserving `map_size` up
// front. A daemon holds one map per loaded repository, so a fixed large map
// runs a process out of virtual address space long before it runs out of
// memory: 1 TiB each exhausted the 128 TiB of x86-64 at ~120 repositories.

/// Enough stores at once that a map of 1 TiB each could not all be reserved.
#[test]
fn many_kv_stores_are_open_at_once() {
    let dirs: Vec<TempDir> = (0..200).map(|_| TempDir::new("kv-many")).collect();
    let stores: Vec<KvStore> = dirs.iter().map(|d| KvStore::open(d.path()).unwrap()).collect();
    assert_eq!(stores.len(), 200);
}

/// A store starting on a map smaller than what it will hold grows it: the
/// map is a reservation, not a quota on the repository's size.
#[test]
fn a_kv_store_grows_past_its_initial_map() {
    let dir = TempDir::new("kv-grow");
    let mut store: Handle = Box::new(KvStore::open_with_map_size(dir.path(), 1 << 20).unwrap());
    let blob = "x".repeat(64 * 1024);
    let mut written = Vec::new();
    for _ in 0..64 {
        let mut w = Writer::begin(&mut store, None).unwrap();
        written.push(
            w.create_metarecord(vec![Field::new("blob", Value::String(blob.clone()))])
                .unwrap()
                .uuid,
        );
        w.commit().unwrap();
    }
    assert_eq!(store.metarecord_count().unwrap(), 64);
    drop(store);

    let store = KvStore::open_with_map_size(dir.path(), 1 << 20).unwrap();
    let last =
        metafolder_daemon::store::Rows::string_field(&store, *written.last().unwrap(), "blob")
            .unwrap();
    assert_eq!(last.map(|s| s.len()), Some(blob.len()), "reopened below its size, it reads it all");
}

/// The store's lock is an `flock`, which a `fork` shares with the child
/// until its `exec` closes it: a process that spawns anything holds its own
/// lock, briefly, from another descriptor. Opening waits such a transient
/// holder out — and still refuses a lasting one (a second daemon).
#[test]
fn a_kv_store_waits_out_a_transient_lock_holder() {
    use std::os::fd::AsRawFd;
    let dir = TempDir::new("kv-lock");
    drop(KvStore::open(dir.path()).unwrap());
    let holder = std::fs::File::open(dir.path().join("daemon.lock")).unwrap();
    // SAFETY: flock on a descriptor this test owns.
    assert_eq!(unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(200));
        drop(holder);
    });
    KvStore::open(dir.path()).expect("the transient holder is waited out");
    release.join().unwrap();

    let _open = KvStore::open(dir.path()).unwrap();
    assert!(KvStore::open(dir.path()).is_err(), "a lasting holder is refused");
}

/// A healthy store checks clean, and stays so once reindexed.
fn a_healthy_store_checks_clean(backend: Backend) {
    let (mut conn, _dir, _, _) = fixture(backend);
    assert!(conn.check().unwrap().is_empty());
    conn.reindex().unwrap();
    assert!(conn.check().unwrap().is_empty());
}

mod checks {
    #[test]
    fn sqlite() {
        super::a_healthy_store_checks_clean(super::Backend::Sqlite)
    }
    #[test]
    fn kv() {
        super::a_healthy_store_checks_clean(super::Backend::Kv)
    }
}
