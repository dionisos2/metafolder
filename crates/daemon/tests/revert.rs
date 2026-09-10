//! Library-level tests for reverting (spec-event-log "Revert"), where a
//! revision's exact shape can be built on purpose.
//!
//! The delicate cases all come from one place: a rollback's inverse restores
//! `field.id` exactly, which is what lets it compose inverses blindly, and a
//! revert cannot — it is a new write, so its inserts take fresh ids. Every test
//! here pins a shape where that difference could show.

use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::log::{self, OpType, Writer};
use metafolder_daemon::{db, revert};
use rusqlite::Connection;
use uuid::Uuid;

fn test_conn() -> Connection {
    let conn = db::open_in_memory().unwrap();
    db::init_schema(&conn).unwrap();
    conn
}

/// The whole database as `(uuid, field name, value)`, sorted — ids excluded on
/// purpose: a revert restores values, never the row ids they used to have.
fn state(conn: &Connection) -> Vec<(String, String, String)> {
    let mut stmt = conn
        .prepare(
            "SELECT m.uuid, f.field_name, f.value_type, f.value_text, f.value_int,
                    f.value_real, f.value_name
             FROM metarecord m LEFT JOIN field f ON f.metarecord_uuid = m.uuid",
        )
        .unwrap();
    let mut rows: Vec<(String, String, String)> = stmt
        .query_map([], |r| {
            let uuid: Vec<u8> = r.get(0)?;
            let name: Option<String> = r.get(1)?;
            let rendered = format!(
                "{:?}/{:?}/{:?}/{:?}/{:?}",
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<i64>>(4)?,
                r.get::<_, Option<f64>>(5)?,
                r.get::<_, Option<String>>(6)?,
            );
            Ok((format!("{uuid:02x?}"), name.unwrap_or_default(), rendered))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    rows.sort();
    rows
}

/// Reverts `ops` (oldest first) as one new revision, the way the route does.
fn revert_ops(conn: &mut Connection, ops: &[log::OpRow]) {
    let mut w = Writer::begin(conn, None).unwrap();
    revert::apply(&mut w, ops).unwrap();
    w.commit().unwrap();
}

fn ops_of_revision(conn: &Connection, rev_id: i64) -> Vec<log::OpRow> {
    revert::revision_ops(conn, rev_id).unwrap()
}

fn field_values(conn: &Connection, uuid: Uuid, name: &str) -> Vec<Value> {
    db::get_field_rows_named(conn, uuid, name).unwrap().into_iter().map(|r| r.value).collect()
}

// ── The id remap ──────────────────────────────────────────────────────────────

/// A revision that appends a row and then deletes it. Walking the inverses
/// backwards re-creates the row under a *new* id before the inverse of the
/// append has to remove it — so that inverse must follow the remap, or it
/// deletes nothing and leaves a row the revert invented.
#[test]
fn test_revert_of_an_append_then_delete_of_the_same_row() {
    let mut conn = test_conn();
    let uuid = {
        let mut w = Writer::begin(&mut conn, None).unwrap();
        let m = w.create_metarecord(vec![Field::new("tag", Value::String("keep".into()))]).unwrap();
        w.commit().unwrap();
        m.uuid
    };
    let before = state(&conn);

    let rev = {
        let mut w = Writer::begin(&mut conn, None).unwrap();
        let id = w.append_field(uuid, "tag", Value::String("doomed".into())).unwrap();
        w.delete_field(uuid, id).unwrap();
        let rev = w.rev_id();
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

/// A revision that appends a row and then replaces the whole cell. The inverse
/// of the `set_field` re-creates the cell — including the appended row, under a
/// new id — and the inverse of the append must then find *that* row.
#[test]
fn test_revert_of_an_append_then_set_on_the_same_cell() {
    let mut conn = test_conn();
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
        let rev = w.rev_id();
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
/// This one is *refused*, and the reason is worth pinning. Reverting a retype
/// is legitimate — the revision being undone is the very one that changed the
/// field's type — but the reverse walk gets there through a transient state:
/// after undoing the last row's append, the cell still holds the *other* row as
/// a string, so `validate_value_type` sees an established string type and
/// refuses the int being restored. The final state would be consistent; no
/// intermediate one is. Undoing a retype is `mf retype` back, until validation
/// moves to commit time (spec-event-log "Open questions").
#[test]
fn test_revert_of_a_retype_shaped_revision_is_refused_by_type_validation() {
    let mut conn = test_conn();
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

    let rev = {
        let mut w = Writer::begin(&mut conn, None).unwrap();
        w.retype_field("rating", metafolder_core::metarecord::FieldType::String).unwrap();
        let rev = w.rev_id();
        w.commit().unwrap();
        rev
    };
    let converted = state(&conn);

    let ops = ops_of_revision(&conn, rev);
    let mut w = Writer::begin(&mut conn, None).unwrap();
    let err = revert::apply(&mut w, &ops).expect_err("type validation refuses the restored int");
    assert!(format!("{err}").contains("value type"), "unexpected error: {err}");
    drop(w); // the transaction rolls back

    assert_eq!(state(&conn), converted, "a refused revert writes nothing");
    let mut values = field_values(&conn, uuid, "rating");
    values.sort_by_key(|v| format!("{v:?}"));
    assert_eq!(values, vec![Value::String("3".into()), Value::String("5".into())]);
}

/// The contrast that explains the refusal above: a *rollback* across the same
/// retype works. Navigation does not go through the `Writer` at all — its
/// inverse writes rows with `db::insert_field_row` directly — so
/// `validate_value_type` never runs, and the intermediate states it would have
/// objected to are never examined by anything.
#[test]
fn test_rollback_across_a_retype_works_where_a_revert_cannot() {
    let mut conn = test_conn();
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
    let checkpoint = log::get_head(&conn).unwrap();

    {
        let mut w = Writer::begin(&mut conn, None).unwrap();
        w.retype_field("rating", metafolder_core::metarecord::FieldType::String).unwrap();
        w.commit().unwrap();
    }
    assert_ne!(state(&conn), before, "the retype converted the rows");

    log::navigate(&mut conn, checkpoint).unwrap();

    let mut values = field_values(&conn, uuid, "rating");
    values.sort_by_key(|v| format!("{v:?}"));
    assert_eq!(values, vec![Value::Int(3), Value::Int(5)], "both rows are ints again");
    // And a rollback restores the row ids exactly, so the whole state matches.
    assert_eq!(state(&conn), before);
}

// ── Reverting a whole-record write ────────────────────────────────────────────

#[test]
fn test_revert_of_a_set_record_restores_every_field() {
    let mut conn = test_conn();
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
        let rev = w.rev_id();
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
    let mut conn = test_conn();
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
        let rev = w.rev_id();
        w.commit().unwrap();
        rev
    };
    let ops = ops_of_revision(&conn, rev);
    revert_ops(&mut conn, &ops);
    assert_eq!(state(&conn), before, "the record returns under its own uuid");
}

// ── Oracle: revert of a suffix ≡ rollback across it ───────────────────────────

/// When the reverted set is everything since a point, a revert must land on the
/// state a rollback to that point would produce. Values only: a rollback
/// restores row ids exactly and a revert deliberately does not.
#[test]
fn test_revert_of_the_last_revisions_matches_a_rollback() {
    let mut conn = test_conn();
    let uuid = {
        let mut w = Writer::begin(&mut conn, None).unwrap();
        let m = w.create_metarecord(vec![Field::new("a", Value::Int(1))]).unwrap();
        w.commit().unwrap();
        m.uuid
    };

    let checkpoint = log::get_head(&conn).unwrap();
    let expected = state(&conn);

    // Three revisions of assorted shapes.
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
        revs.push(w.rev_id());
        w.commit().unwrap();
    }
    let after_writes = state(&conn);
    assert_ne!(after_writes, expected);

    // The rollback answer.
    log::navigate(&mut conn, checkpoint).unwrap();
    let by_rollback = state(&conn);
    assert_eq!(by_rollback, expected);

    // Redo, then get there by reverting instead.
    let tip = {
        let ops = log::all_ops(&conn).unwrap();
        ops.last().unwrap().id
    };
    log::navigate(&mut conn, Some(tip)).unwrap();
    assert_eq!(state(&conn), after_writes, "back at the tip");

    let mut ops: Vec<log::OpRow> = vec![];
    for rev in &revs {
        ops.extend(ops_of_revision(&conn, *rev));
    }
    ops.sort_by_key(|o| o.id);
    revert_ops(&mut conn, &ops);
    assert_eq!(
        state(&conn),
        by_rollback,
        "a revert of the whole suffix lands where a rollback does"
    );
}

// ── The written op type ───────────────────────────────────────────────────────

/// Reverting a file event records a file op type, because that is what tells a
/// later navigation to move the file when it crosses this operation.
#[test]
fn test_reverting_a_file_event_records_a_file_op_type() {
    let mut conn = test_conn();
    let uuid = {
        let mut w = Writer::begin(&mut conn, None).unwrap();
        let m = w.create_metarecord(vec![Field::new("mfr_size", Value::Int(1))]).unwrap();
        w.commit().unwrap();
        m.uuid
    };
    let rev = {
        let mut w = Writer::begin(&mut conn, None).unwrap();
        w.set_field_as(OpType::FileModified, uuid, "mfr_size", Value::Int(2)).unwrap();
        let rev = w.rev_id();
        w.commit().unwrap();
        rev
    };
    let ops = ops_of_revision(&conn, rev);
    revert_ops(&mut conn, &ops);

    let head = log::get_head(&conn).unwrap().unwrap();
    let written = log::get_op(&conn, head).unwrap().unwrap();
    assert_eq!(written.op_type, "file_modified", "not set_field: navigation keys on this");
    assert_eq!(field_values(&conn, uuid, "mfr_size"), vec![Value::Int(1)]);
}
