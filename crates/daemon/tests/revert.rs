//! Library-level tests for reverting (doc "Revert"), where a
//! write's exact shape can be built on purpose.
//!
//! The delicate cases all come from one place: a revert is a new write, so
//! its inserts take fresh ids, and nothing may address a row by the id a
//! snapshot recorded. Every test here pins a shape where that could show.

use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::log::{self, OpType, Writer};
use metafolder_daemon::revert;
use uuid::Uuid;

use metafolder_daemon::kvstore::KvStore;

use metafolder_daemon::store::{Log as _, Rows as _};

mod common;

fn test_conn() -> (KvStore, common::TempDir) {
    common::kv::store()
}

/// The whole database as `(uuid, field name, value)`, sorted — ids excluded on
/// purpose: a revert restores values, never the row ids they used to have.
fn state(conn: &KvStore) -> Vec<(String, String, String)> {
    let mut rows = Vec::new();
    for uuid in conn.metarecords().unwrap() {
        let fields = conn.rows(uuid).unwrap();
        if fields.is_empty() {
            rows.push((uuid.to_string(), String::new(), String::new()));
        }
        for row in fields {
            rows.push((uuid.to_string(), row.name, format!("{:?}", row.value)));
        }
    }
    rows.sort();
    rows
}

/// Reverts `ops` (oldest first) as one new write, the way the route does.
fn revert_ops(conn: &mut KvStore, ops: &[log::OpRow]) {
    let mut w = Writer::begin(conn, None).unwrap();
    revert::apply(&mut w, ops).unwrap();
    w.commit().unwrap();
}

fn ops_of_revision(conn: &KvStore, rev_id: i64) -> Vec<log::OpRow> {
    revert::label_ops(conn, rev_id).unwrap()
}

fn field_values(conn: &KvStore, uuid: Uuid, name: &str) -> Vec<Value> {
    conn.rows_named(uuid, name).unwrap().into_iter().map(|r| r.value).collect()
}

// ── The id remap ──────────────────────────────────────────────────────────────

/// A write that appends a row and then deletes it. Walking the inverses
/// backwards re-creates the row under a *new* id before the inverse of the
/// append has to remove it — so that inverse must follow the remap, or it
/// deletes nothing and leaves a row the revert invented.
#[test]
fn test_revert_of_an_append_then_delete_of_the_same_row() {
    let (mut conn, _dir) = test_conn();
    let uuid = {
        let mut w = Writer::begin(&mut conn, None).unwrap();
        let m = w.create_metarecord(vec![Field::new("tag", Value::String("keep".into()))]).unwrap();
        w.commit().unwrap();
        m.uuid
    };
    let before = state(&conn);

    let rev = {
        let mut w = Writer::begin(&mut conn, None).unwrap();
        let id = w.append_field(uuid, "tag", Value::String("doomed".into())).unwrap().id();
        w.delete_field(uuid, id).unwrap();
        let rev = w.label_id();
        w.commit().unwrap();
        rev
    };

    let ops = ops_of_revision(&conn, rev);
    assert_eq!(ops.len(), 2, "append + delete");
    revert_ops(&mut conn, &ops);

    let values = field_values(&conn, uuid, "tag");
    assert_eq!(values, vec![Value::String("keep".into())], "no invented row survives");
    assert_eq!(state(&conn), before, "the revert lands exactly on the pre-revision state");
}

/// A write that appends a row and then replaces the whole cell. The inverse
/// of the `set_field` re-creates the cell — including the appended row, under a
/// new id — and the inverse of the append must then find *that* row.
#[test]
fn test_revert_of_an_append_then_set_on_the_same_cell() {
    let (mut conn, _dir) = test_conn();
    let uuid = {
        let mut w = Writer::begin(&mut conn, None).unwrap();
        let m = w.create_metarecord(vec![Field::new("tag", Value::String("a".into()))]).unwrap();
        w.commit().unwrap();
        m.uuid
    };
    let before = state(&conn);

    let rev = {
        let mut w = Writer::begin(&mut conn, None).unwrap();
        w.append_field(uuid, "tag", Value::String("b".into())).unwrap();
        w.set_field(uuid, "tag", Value::String("c".into())).unwrap();
        let rev = w.label_id();
        w.commit().unwrap();
        rev
    };
    assert_eq!(field_values(&conn, uuid, "tag"), vec![Value::String("c".into())]);

    let ops = ops_of_revision(&conn, rev);
    revert_ops(&mut conn, &ops);

    assert_eq!(
        field_values(&conn, uuid, "tag"),
        vec![Value::String("a".into())],
        "the row the revision appended must not survive its revert"
    );
    assert_eq!(state(&conn), before);
}

/// The shape `retype_field` produces through `replace_owned_row`: one
/// `delete_field` plus one `append_field` per converted row, all on one cell,
/// with the *original* id reused by the insert.
///
/// Reverting it means passing through states that are not type-consistent —
/// after undoing the last row's append, the cell still holds the others in the
/// new type — so the revert defers type validation to its own commit and checks
/// the state it actually lands on.
#[test]
fn test_revert_of_a_retype_shaped_revision_restores_both_rows() {
    let (mut conn, _dir) = test_conn();
    let uuid = {
        let mut w = Writer::begin(&mut conn, None).unwrap();
        let m = w
            .create_metarecord(vec![
                Field::new("rating", Value::Int(3)),
                Field::new("rating", Value::Int(5)),
            ])
            .unwrap();
        w.commit().unwrap();
        m.uuid
    };
    let before = state(&conn);

    let rev = {
        let mut w = Writer::begin(&mut conn, None).unwrap();
        w.retype_field("rating", metafolder_core::metarecord::FieldType::String).unwrap();
        let rev = w.label_id();
        w.commit().unwrap();
        rev
    };
    let mut converted = field_values(&conn, uuid, "rating");
    converted.sort_by_key(|v| format!("{v:?}"));
    assert_eq!(
        converted,
        vec![Value::String("3".into()), Value::String("5".into())],
        "the retype converted both rows"
    );

    let ops = ops_of_revision(&conn, rev);
    revert_ops(&mut conn, &ops);

    let mut values = field_values(&conn, uuid, "rating");
    values.sort_by_key(|v| format!("{v:?}"));
    assert_eq!(values, vec![Value::Int(3), Value::Int(5)], "both rows come back, both as ints");
    assert_eq!(state(&conn), before);
}

/// Deferring the check must not become skipping it. Here a *later* retype — one
/// the revert is not undoing — genuinely made the field a string, so restoring
/// an int would leave two types under one name. That is refused, at commit.
#[test]
fn test_a_revert_still_refuses_a_real_type_conflict() {
    let (mut conn, _dir) = test_conn();
    let (uuid, other) = {
        let mut w = Writer::begin(&mut conn, None).unwrap();
        let a = w.create_metarecord(vec![Field::new("rating", Value::Int(3))]).unwrap();
        let b = w.create_metarecord(vec![Field::new("other", Value::Int(1))]).unwrap();
        w.commit().unwrap();
        (a.uuid, b.uuid)
    };
    // The write to revert: it removes the only `rating` row.
    let rev = {
        let mut w = Writer::begin(&mut conn, None).unwrap();
        w.clear_field_as(OpType::SetField, uuid, "rating").unwrap();
        let rev = w.label_id();
        w.commit().unwrap();
        rev
    };
    // Meanwhile `rating` is established as a string elsewhere — nothing that
    // touches the reverted cell, so no dependency blocks the revert.
    {
        let mut w = Writer::begin(&mut conn, None).unwrap();
        w.set_field(other, "rating", Value::String("high".into())).unwrap();
        w.commit().unwrap();
    }
    let before = state(&conn);

    let ops = ops_of_revision(&conn, rev);
    let mut w = Writer::begin(&mut conn, None).unwrap();
    revert::apply(&mut w, &ops).unwrap();
    let err = w.commit().expect_err("two types under one field name must be refused");
    assert!(format!("{err}").contains("value type"), "unexpected error: {err}");

    assert_eq!(state(&conn), before, "a refused revert writes nothing");
}

// ── Reverting a whole-record write ────────────────────────────────────────────

#[test]
fn test_revert_of_a_set_record_restores_every_field() {
    let (mut conn, _dir) = test_conn();
    let uuid = {
        let mut w = Writer::begin(&mut conn, None).unwrap();
        let m = w
            .create_metarecord(vec![
                Field::new("a", Value::Int(1)),
                Field::new("b", Value::String("x".into())),
            ])
            .unwrap();
        w.commit().unwrap();
        m.uuid
    };
    let before = state(&conn);

    let rev = {
        let mut w = Writer::begin(&mut conn, None).unwrap();
        w.set_record(uuid, vec![Field::new("a", Value::Int(99))]).unwrap();
        let rev = w.label_id();
        w.commit().unwrap();
        rev
    };
    assert!(field_values(&conn, uuid, "b").is_empty(), "the overwrite dropped b");

    let ops = ops_of_revision(&conn, rev);
    revert_ops(&mut conn, &ops);
    assert_eq!(state(&conn), before, "b comes back with a");
}

#[test]
fn test_revert_of_a_delete_record_recreates_it_with_the_same_uuid() {
    let (mut conn, _dir) = test_conn();
    let uuid = {
        let mut w = Writer::begin(&mut conn, None).unwrap();
        let m = w.create_metarecord(vec![Field::new("a", Value::Int(1))]).unwrap();
        w.commit().unwrap();
        m.uuid
    };
    let before = state(&conn);

    let rev = {
        let mut w = Writer::begin(&mut conn, None).unwrap();
        w.delete_metarecord(uuid).unwrap();
        let rev = w.label_id();
        w.commit().unwrap();
        rev
    };
    let ops = ops_of_revision(&conn, rev);
    revert_ops(&mut conn, &ops);
    assert_eq!(state(&conn), before, "the record returns under its own uuid");
}

/// A folder and its file deleted in one write, the folder first — what a
/// trashing's cascade writes. The revert recreates them newest first, so the
/// file before its folder: its position names a parent not there *yet*. The
/// forest is checked on the state the revert lands on, not on each step.
#[test]
fn test_revert_of_a_subtree_deleted_parent_first_recreates_it() {
    use metafolder_core::metarecord::TreeName;
    let (mut conn, _dir) = test_conn();
    let path = |parent: Option<Uuid>, name: &str| {
        Field::new("mfr_path", Value::TreeRef { parent, name: TreeName::from(name) })
    };
    let (dir, file) = {
        let mut w = Writer::begin(&mut conn, None).unwrap();
        let root = w.create_metarecord(vec![path(None, "")]).unwrap().uuid;
        let dir = w.create_metarecord(vec![path(Some(root), "A")]).unwrap().uuid;
        let file = w.create_metarecord(vec![path(Some(dir), "B.txt")]).unwrap().uuid;
        w.commit().unwrap();
        (dir, file)
    };
    let before = state(&conn);

    let rev = {
        let mut w = Writer::begin(&mut conn, None).unwrap();
        w.delete_metarecord(dir).unwrap();
        w.delete_metarecord(file).unwrap();
        let rev = w.label_id();
        w.commit().unwrap();
        rev
    };
    let ops = ops_of_revision(&conn, rev);
    revert_ops(&mut conn, &ops);
    assert_eq!(state(&conn), before, "the folder and its file are back");
}

/// Deferring the forest check is not skipping it: a position whose parent is
/// still missing once the revert is done is refused, at commit.
#[test]
fn test_a_revert_still_refuses_a_position_whose_parent_is_gone() {
    use metafolder_core::metarecord::TreeName;
    let (mut conn, _dir) = test_conn();
    let path = |parent: Option<Uuid>, name: &str| {
        Field::new("mfr_path", Value::TreeRef { parent, name: TreeName::from(name) })
    };
    let (dir, file) = {
        let mut w = Writer::begin(&mut conn, None).unwrap();
        let root = w.create_metarecord(vec![path(None, "")]).unwrap().uuid;
        let dir = w.create_metarecord(vec![path(Some(root), "A")]).unwrap().uuid;
        let file = w.create_metarecord(vec![path(Some(dir), "B.txt")]).unwrap().uuid;
        w.commit().unwrap();
        (dir, file)
    };
    // The file goes, then the folder: reverting only the file's deletion
    // would place it under a folder that is no longer there.
    let file_rev = {
        let mut w = Writer::begin(&mut conn, None).unwrap();
        w.delete_metarecord(file).unwrap();
        let rev = w.label_id();
        w.commit().unwrap();
        rev
    };
    let mut w = Writer::begin(&mut conn, None).unwrap();
    w.delete_metarecord(dir).unwrap();
    w.commit().unwrap();

    let ops = ops_of_revision(&conn, file_rev);
    let mut w = Writer::begin(&mut conn, None).unwrap();
    revert::apply(&mut w, &ops).unwrap();
    let err = w.commit().expect_err("a position under a missing parent must be refused");
    assert!(format!("{err}").contains("TreeRef parent"), "unexpected error: {err}");
}

// ── Oracle: revert of a suffix ≡ the state before it ──────────────────────────

/// When the reverted set is everything since a point, a revert must land on the
/// state the repository had at that point. Values only: a revert gives the rows
/// it puts back fresh ids.
#[test]
fn test_revert_of_the_last_revisions_returns_to_the_state_before_them() {
    let (mut conn, _dir) = test_conn();
    let uuid = {
        let mut w = Writer::begin(&mut conn, None).unwrap();
        let m = w.create_metarecord(vec![Field::new("a", Value::Int(1))]).unwrap();
        w.commit().unwrap();
        m.uuid
    };

    let expected = state(&conn);

    // Three writes of assorted shapes.
    let mut revs = vec![];
    for (i, shape) in [0, 1, 2].iter().enumerate() {
        let mut w = Writer::begin(&mut conn, None).unwrap();
        match shape {
            0 => w.set_field(uuid, "a", Value::Int(10 + i as i64)).unwrap(),
            1 => {
                w.append_field(uuid, "b", Value::String("x".into())).unwrap();
            }
            _ => {
                w.set_field_multi(uuid, "a", vec![Value::Int(7), Value::Int(8)]).unwrap();
            }
        }
        revs.push(w.label_id());
        w.commit().unwrap();
    }
    let after_writes = state(&conn);
    assert_ne!(after_writes, expected);

    let mut ops: Vec<log::OpRow> = vec![];
    for rev in &revs {
        ops.extend(ops_of_revision(&conn, *rev));
    }
    ops.sort_by_key(|o| o.id);
    revert_ops(&mut conn, &ops);
    assert_eq!(state(&conn), expected, "a revert of the whole suffix lands where it started");
}

// ── The written op type ───────────────────────────────────────────────────────

/// Reverting a file event records a file op type, because that is what tells a
/// later navigation to move the file when it crosses this operation.
#[test]
fn test_reverting_a_file_event_records_a_file_op_type() {
    let (mut conn, _dir) = test_conn();
    let uuid = {
        let mut w = Writer::begin(&mut conn, None).unwrap();
        let m = w.create_metarecord(vec![Field::new("mfr_size", Value::Int(1))]).unwrap();
        w.commit().unwrap();
        m.uuid
    };
    let rev = {
        let mut w = Writer::begin(&mut conn, None).unwrap();
        w.set_field_as(OpType::FileModified, uuid, "mfr_size", Value::Int(2)).unwrap();
        let rev = w.label_id();
        w.commit().unwrap();
        rev
    };
    let ops = ops_of_revision(&conn, rev);
    revert_ops(&mut conn, &ops);

    let head = conn.head().unwrap().unwrap();
    let written = conn.op(head).unwrap().unwrap();
    assert_eq!(written.op_type, "file_modified", "not set_field: navigation keys on this");
    assert_eq!(field_values(&conn, uuid, "mfr_size"), vec![Value::Int(1)]);
}
