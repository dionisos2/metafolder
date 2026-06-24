//! Incremental-maintenance equivalence (spec-indexing increment 6).
//!
//! The invariant: after any sequence of writes, refreshing an existing index
//! incrementally (`RepoIndex::refresh`, the forward log-replay path) yields the
//! exact same query answers as building a fresh index from scratch. The
//! from-scratch build is itself oracle-validated against SQL
//! (`tests/index_oracle.rs`), so "incremental == rebuild" transitively means
//! "incremental == SQL".

use metafolder_core::metarecord::{Field, Value};
use metafolder_core::query::{FollowTarget, Query};
use metafolder_daemon::db;
use metafolder_daemon::index::{RepoIndex, SortBy};
use metafolder_daemon::log::Writer;
use rusqlite::Connection;
use uuid::Uuid;

struct Repo {
    conn: Connection,
    db_id: Uuid,
    index: RepoIndex,
}

impl Repo {
    fn new() -> Self {
        let conn = db::open_in_memory().unwrap();
        db::init_schema(&conn).unwrap();
        let db_id = Uuid::new_v4();
        let index = RepoIndex::build(&conn, db_id).unwrap();
        Self { conn, db_id, index }
    }

    /// Runs a write closure in one revision, then refreshes the index
    /// incrementally and asserts it matches a fresh rebuild.
    fn write<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut Writer) -> R,
    {
        let mut w = Writer::begin(&mut self.conn, self.db_id, None).unwrap();
        let out = f(&mut w);
        w.commit().unwrap();
        self.index.refresh(&self.conn, self.db_id).unwrap();
        self.assert_consistent();
        out
    }

    fn assert_consistent(&self) {
        let fresh = RepoIndex::build(&self.conn, self.db_id).unwrap();
        for q in battery() {
            assert_eq!(set(&self.index, &q), set(&fresh, &q), "evaluate divergence on {q:?}");
            assert_eq!(
                self.index.count(&q).ok(),
                fresh.count(&q).ok(),
                "count divergence on {q:?}"
            );
        }
        for (q, field, asc) in sorted_battery() {
            let by = [SortBy { field: field.into(), ascending: asc }];
            assert_eq!(
                self.index.evaluate_sorted(&q, &by, Some(50)).ok(),
                fresh.evaluate_sorted(&q, &by, Some(50)).ok(),
                "sorted divergence on {q:?} by {field} asc={asc}"
            );
        }
    }
}

fn set(index: &RepoIndex, q: &Query) -> Option<Vec<Uuid>> {
    index.evaluate(q).ok().map(|bm| {
        let mut v = index.to_uuids(&bm);
        v.sort();
        v
    })
}

fn s(v: &str) -> Value {
    Value::String(v.into())
}
fn i(n: i64) -> Value {
    Value::Int(n)
}
fn present(f: &str) -> Query {
    Query::IsPresent { field: f.into() }
}
fn eq(f: &str, v: Value) -> Query {
    Query::Eq { field: f.into(), value: v }
}
fn follows(f: &str, cond: Query) -> Query {
    Query::Follows { field: f.into(), target: FollowTarget::Condition(Box::new(cond)) }
}

fn battery() -> Vec<Query> {
    vec![
        present("rate"),
        Query::IsAbsent { field: "rate".into() },
        Query::IsUnknown { field: "rate".into() },
        eq("rate", i(5)),
        Query::Neq { field: "rate".into(), value: i(5) },
        Query::Gte { field: "rate".into(), value: i(5) },
        Query::Lt { field: "rate".into(), value: i(5) },
        present("kind"),
        eq("kind", s("file")),
        Query::Neq { field: "kind".into(), value: s("file") },
        present("note"),
        eq("note", s("hi")),
        present("loc"),
        follows("loc", eq("tag", s("root"))),
        Query::And { operands: vec![eq("kind", s("file")), Query::Gte { field: "rate".into(), value: i(3) }] },
        Query::Not { operand: Box::new(eq("kind", s("file"))) },
    ]
}

fn sorted_battery() -> Vec<(Query, &'static str, bool)> {
    vec![
        (present("rate"), "rate", true),
        (present("rate"), "rate", false),
        (present("kind"), "kind", true),
        (present("kind"), "rate", false),
    ]
}

// Build a small tree-of-files repo and return the root + a file's uuid/field id.
fn seed(r: &mut Repo) -> (Uuid, Uuid, i64) {
    r.write(|w| {
        let root = w
            .create_metarecord(vec![
                Field::new("tag", s("root")),
                Field::new("loc", Value::TreeRef { parent: None, name: "root".into() }),
            ])
            .unwrap()
            .uuid;
        let file = w
            .create_metarecord(vec![
                Field::new("kind", s("file")),
                Field::new("rate", i(5)),
                Field::new("loc", Value::TreeRef { parent: Some(root), name: "a".into() }),
            ])
            .unwrap();
        let rate_id = file.fields.iter().find(|f| f.name == "rate").unwrap().id.unwrap();
        (root, file.uuid, rate_id)
    })
}

#[test]
fn incremental_create() {
    let mut r = Repo::new();
    seed(&mut r);
    r.write(|w| {
        w.create_metarecord(vec![Field::new("kind", s("file")), Field::new("rate", i(9))]).unwrap();
    });
}

#[test]
fn incremental_set_field_changes_value() {
    let mut r = Repo::new();
    let (_root, file, _rate_id) = seed(&mut r);
    r.write(|w| w.set_field(file, "rate", i(8)).unwrap());
    r.write(|w| w.set_field(file, "kind", s("dir")).unwrap());
}

#[test]
fn incremental_set_field_to_nothing() {
    let mut r = Repo::new();
    let (_root, file, _rate_id) = seed(&mut r);
    r.write(|w| w.set_field(file, "rate", Value::Nothing).unwrap());
}

#[test]
fn incremental_append_multimap() {
    let mut r = Repo::new();
    let (_root, file, _rate_id) = seed(&mut r);
    r.write(|w| w.append_field(file, "rate", i(2)).unwrap());
    r.write(|w| w.append_field(file, "rate", i(20)).unwrap());
    r.write(|w| w.append_field(file, "note", s("hi")).unwrap());
}

#[test]
fn incremental_replace_and_delete_field() {
    let mut r = Repo::new();
    let (_root, file, rate_id) = seed(&mut r);
    r.write(|w| w.replace_field(file, rate_id, i(42)).unwrap());
    r.write(|w| w.delete_field(file, rate_id).unwrap());
}

#[test]
fn incremental_delete_metarecord() {
    let mut r = Repo::new();
    let (_root, file, _rate_id) = seed(&mut r);
    r.write(|w| w.delete_metarecord(file).unwrap());
}

#[test]
fn incremental_treeref_move() {
    let mut r = Repo::new();
    let (root, file, _rate_id) = seed(&mut r);
    // Add a second directory and move the file under it.
    let dir2 = r.write(|w| {
        w.create_metarecord(vec![
            Field::new("tag", s("d2")),
            Field::new("loc", Value::TreeRef { parent: Some(root), name: "d2".into() }),
        ])
        .unwrap()
        .uuid
    });
    r.write(|w| w.set_field(file, "loc", Value::TreeRef { parent: Some(dir2), name: "a".into() }).unwrap());
}

#[test]
fn incremental_mixed_sequence() {
    let mut r = Repo::new();
    let (_root, file, rate_id) = seed(&mut r);
    // replace_field first, while rate_id is still the live row (a later
    // set_field on "rate" would delete it).
    r.write(|w| w.replace_field(file, rate_id, i(3)).unwrap());
    r.write(|w| w.set_field(file, "rate", i(7)).unwrap());
    let other = r.write(|w| {
        w.create_metarecord(vec![Field::new("kind", s("dir")), Field::new("rate", i(1))]).unwrap().uuid
    });
    r.write(|w| w.append_field(file, "note", s("hi")).unwrap());
    r.write(|w| w.set_field(other, "kind", s("file")).unwrap());
    r.write(|w| w.delete_metarecord(other).unwrap());
    r.write(|w| w.set_field(file, "note", Value::Nothing).unwrap());
}

/// A rollback rewrites history (the new HEAD is not a forward extension), so
/// `refresh` must detect it and fall back to a full rebuild — still correct.
#[test]
fn incremental_rollback_falls_back_to_rebuild() {
    let mut r = Repo::new();
    let (_root, file, _rate_id) = seed(&mut r);
    let checkpoint = db::current_head(&r.conn).unwrap();
    r.write(|w| w.set_field(file, "rate", i(99)).unwrap());
    r.write(|w| w.set_field(file, "kind", s("dir")).unwrap());

    // Move HEAD back to the checkpoint: built_at_head is now a descendant of
    // HEAD, not an ancestor → forward_delta returns None → rebuild.
    metafolder_daemon::log::navigate(&mut r.conn, r.db_id, checkpoint).unwrap();
    r.index.refresh(&r.conn, r.db_id).unwrap();
    r.assert_consistent();
}
