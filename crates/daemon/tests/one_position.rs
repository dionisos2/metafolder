//! A metarecord holds at most one position in a forest (spec-data-model "One
//! position per forest"): every write shape that could give it a second
//! `tree_ref` row of the same name is refused, and a retype demotes the extra
//! rows to `Nothing` as it demotes any other forest violation.

use metafolder_core::metarecord::{Field, FieldType, Value};
use metafolder_daemon::db;
use metafolder_daemon::log::Writer;
use rusqlite::Connection;
use uuid::Uuid;

mod common;

fn repo() -> Connection {
    let conn = db::open_in_memory().unwrap();
    db::init_schema(&conn).unwrap();
    conn
}

fn tree(parent: Option<Uuid>, name: &str) -> Value {
    Value::TreeRef { parent, name: name.into() }
}

/// `tags` with `red` and `blue` below it, and `test` under `red`.
fn forest(conn: &mut Connection) -> (Uuid, Uuid, Uuid) {
    let mut w = Writer::begin(conn, None).unwrap();
    let tags = w.create_metarecord(vec![Field::new("tag", tree(None, "tags"))]).unwrap().uuid;
    let red = w.create_metarecord(vec![Field::new("tag", tree(Some(tags), "red"))]).unwrap().uuid;
    let blue = w.create_metarecord(vec![Field::new("tag", tree(Some(tags), "blue"))]).unwrap().uuid;
    let test = w.create_metarecord(vec![Field::new("tag", tree(Some(red), "test"))]).unwrap().uuid;
    w.commit().unwrap();
    (blue, red, test)
}

fn refused(result: anyhow::Result<impl std::fmt::Debug>) {
    let err = result.expect_err("a second position must be refused");
    assert!(err.to_string().contains("one position"), "unexpected error: {err}");
}

#[test]
fn a_record_cannot_be_created_at_two_positions() {
    let mut conn = repo();
    let (blue, red, _) = forest(&mut conn);
    let mut w = Writer::begin(&mut conn, None).unwrap();
    refused(w.create_metarecord(vec![
        Field::new("tag", tree(Some(red), "x")),
        Field::new("tag", tree(Some(blue), "x")),
    ]));
}

#[test]
fn a_second_position_cannot_be_added() {
    let mut conn = repo();
    let (blue, _, test) = forest(&mut conn);
    let mut w = Writer::begin(&mut conn, None).unwrap();
    refused(w.append_field(test, "tag", tree(Some(blue), "test")));
    // The position it already has is no second one: a no-op, as ever.
    let red_test = w.store().rows_named(test, "tag").unwrap()[0].value.clone();
    w.append_field(test, "tag", red_test).unwrap();
}

#[test]
fn a_set_or_an_overwrite_cannot_hold_two_positions() {
    let mut conn = repo();
    let (blue, red, test) = forest(&mut conn);
    let mut w = Writer::begin(&mut conn, None).unwrap();
    refused(w.set_field_multi(test, "tag", vec![tree(Some(red), "t"), tree(Some(blue), "t")]));
    refused(w.set_record(
        test,
        vec![Field::new("tag", tree(Some(red), "t")), Field::new("tag", tree(Some(blue), "t"))],
    ));
    // Moving it is a set of its one position.
    w.set_field(test, "tag", tree(Some(blue), "test")).unwrap();
    w.commit().unwrap();
}

#[test]
fn a_row_cannot_be_edited_into_a_second_position() {
    let mut conn = repo();
    let (blue, _, test) = forest(&mut conn);
    let mut w = Writer::begin(&mut conn, None).unwrap();
    let other = match w.append_field(test, "note", Value::String("n".into())).unwrap() {
        metafolder_daemon::log::Appended::Created(id) => id,
        other => panic!("{other:?}"),
    };
    refused(w.rename_field(test, other, "tag", tree(Some(blue), "test")));
    // Editing its one position in place is a move.
    let own = w.store().rows_named(test, "tag").unwrap()[0].id;
    w.replace_field(test, own, tree(Some(blue), "test")).unwrap();
    w.commit().unwrap();
}

#[test]
fn a_position_beside_a_nothing_row_is_still_one() {
    let mut conn = repo();
    let (_, red, _) = forest(&mut conn);
    let mut w = Writer::begin(&mut conn, None).unwrap();
    let rec = w
        .create_metarecord(vec![
            Field::new("tag", Value::Nothing),
            Field::new("tag", tree(Some(red), "y")),
        ])
        .unwrap()
        .uuid;
    w.append_field(rec, "other", Value::Nothing).unwrap();
    w.commit().unwrap();
}

#[test]
fn a_retype_keeps_one_position_and_demotes_the_rest() {
    let mut conn = repo();
    let mut w = Writer::begin(&mut conn, None).unwrap();
    let rec = w
        .create_metarecord(vec![
            Field::new("loc", Value::String("/a".into())),
            Field::new("loc", Value::String("/b".into())),
        ])
        .unwrap()
        .uuid;
    let summary = w.retype_field("loc", FieldType::TreeRef).unwrap();
    assert_eq!(summary.fallback_uuids, vec![rec]);
    let rows = w.store().rows_named(rec, "loc").unwrap();
    let positions = rows.iter().filter(|r| matches!(r.value, Value::TreeRef { .. })).count();
    assert_eq!(positions, 1, "{rows:?}");
    w.commit().unwrap();
}

/// A repository written before the rule may still hold a node at two
/// positions: `mf repo check` names it, on either backend.
#[test]
fn a_check_names_a_record_left_at_two_positions() {
    use metafolder_daemon::store::one_position_problems;
    let dir = common::TempDir::new("one-position-check");
    let kv = metafolder_daemon::kvstore::KvStore::open(dir.path()).unwrap();
    let stores: Vec<Box<dyn metafolder_daemon::store::Database>> =
        vec![Box::new(repo()), Box::new(kv)];
    for mut store in stores {
        let (blue, test) = {
            let mut w = Writer::begin(&mut *store, None).unwrap();
            let tags =
                w.create_metarecord(vec![Field::new("tag", tree(None, "tags"))]).unwrap().uuid;
            let red =
                w.create_metarecord(vec![Field::new("tag", tree(Some(tags), "red"))]).unwrap();
            let blue =
                w.create_metarecord(vec![Field::new("tag", tree(Some(tags), "blue"))]).unwrap();
            let test =
                w.create_metarecord(vec![Field::new("tag", tree(Some(red.uuid), "test"))]).unwrap();
            w.commit().unwrap();
            (blue.uuid, test.uuid)
        };
        assert!(one_position_problems(&*store).unwrap().is_empty());
        // What an older daemon let through, written under the Writer.
        let tx = store.begin_write().unwrap();
        tx.insert_row(test, "tag", &tree(Some(blue), "test"), None).unwrap();
        tx.commit().unwrap();
        let problems = one_position_problems(&*store).unwrap();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains(&test.to_string()) && problems[0].contains("'tag'"));
    }
}
