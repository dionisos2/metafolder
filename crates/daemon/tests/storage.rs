//! Integration tests for the storage layer: SQLite schema, value encoding,
//! the logged write flow (Writer), TreeRef validation, reserved fields.

use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::db;
use metafolder_daemon::log::{OpType, Writer};
use metafolder_daemon::reserved;
use rusqlite::Connection;
use uuid::Uuid;

fn test_conn() -> Connection {
    let conn = db::open_in_memory().unwrap();
    db::init_schema(&conn).unwrap();
    conn
}

fn repo_id() -> Uuid {
    Uuid::new_v4()
}

/// Creates an entry through a single-use Writer and returns it.
fn create(conn: &mut Connection, db_id: Uuid, fields: Vec<Field>) -> metafolder_core::metarecord::MetaRecord {
    let mut w = Writer::begin(conn, db_id, None).unwrap();
    let m = w.create_metarecord(fields).unwrap();
    w.commit().unwrap();
    m
}

// ── Schema ────────────────────────────────────────────────────────────────────

/// EXPLAIN QUERY PLAN `detail` lines for `sql`, joined into one string.
fn query_plan(conn: &Connection, sql: &str) -> String {
    let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    let rows: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(3))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    rows.join(" | ")
}

// ── One value type per field name (invariant) ──────────────────────────────────

#[test]
fn test_field_first_write_establishes_type() {
    // The first non-Nothing write of a name succeeds and fixes its type.
    let mut conn = test_conn();
    let db_id = repo_id();
    let m = create(&mut conn, db_id, vec![Field::new("rating", Value::Int(5))]);
    let got = db::get_metarecord(&conn, m.uuid).unwrap().unwrap();
    assert_eq!(got.get("rating"), Some(&Value::Int(5)));
}

#[test]
fn test_field_rejects_conflicting_value_type() {
    // Once `rating` is an Int repo-wide, a String write to it is rejected (400).
    let mut conn = test_conn();
    let db_id = repo_id();
    create(&mut conn, db_id, vec![Field::new("rating", Value::Int(5))]);

    let mut w = Writer::begin(&mut conn, db_id, None).unwrap();
    let err = w
        .set_field(Uuid::new_v4(), "rating", Value::String("five".into()))
        .unwrap_err();
    assert!(
        err.to_string().contains("type"),
        "expected a type-conflict error, got: {err}"
    );
}

#[test]
fn test_field_rejects_conflicting_type_within_one_create() {
    // Two rows of the same name with different types in a single create are rejected.
    let mut conn = test_conn();
    let db_id = repo_id();
    let mut w = Writer::begin(&mut conn, db_id, None).unwrap();
    let err = w
        .create_metarecord(vec![
            Field::new("tag", Value::String("a".into())),
            Field::new("tag", Value::Int(1)),
        ])
        .unwrap_err();
    assert!(err.to_string().contains("type"), "unexpected error: {err}");
}

#[test]
fn test_field_allows_nothing_against_any_type() {
    // Nothing is absence, not a type: it coexists with the established type.
    let mut conn = test_conn();
    let db_id = repo_id();
    let m = create(&mut conn, db_id, vec![Field::new("rating", Value::Int(5))]);

    let mut w = Writer::begin(&mut conn, db_id, None).unwrap();
    w.set_field(m.uuid, "rating", Value::Nothing).unwrap();
    w.commit().unwrap();
}

#[test]
fn test_field_type_unlocks_when_empty() {
    // With no non-Nothing rows left, the name's type is unestablished again and a
    // new (different) type may be written.
    let mut conn = test_conn();
    let db_id = repo_id();
    let m = create(&mut conn, db_id, vec![Field::new("note", Value::Int(1))]);

    let mut w = Writer::begin(&mut conn, db_id, None).unwrap();
    w.set_field(m.uuid, "note", Value::Nothing).unwrap(); // clears the Int row
    w.set_field(m.uuid, "note", Value::String("now text".into())).unwrap();
    w.commit().unwrap();
}

#[test]
fn test_field_type_unlocks_within_one_revision() {
    // The per-Writer type cache must not go stale: clearing a field to Nothing
    // mid-revision unlocks its type, so a later different-type write succeeds in
    // the *same* Writer.
    let mut conn = test_conn();
    let db_id = repo_id();
    let m = create(&mut conn, db_id, vec![Field::new("note", Value::Int(1))]);

    let mut w = Writer::begin(&mut conn, db_id, None).unwrap();
    w.set_field(m.uuid, "note", Value::Int(2)).unwrap(); // caches "int"
    w.set_field(m.uuid, "note", Value::Nothing).unwrap(); // clears → must drop cache
    w.set_field(m.uuid, "note", Value::String("text".into())).unwrap(); // now allowed
    w.commit().unwrap();

    let g = db::get_metarecord(&conn, m.uuid).unwrap().unwrap();
    assert_eq!(g.get("note"), Some(&Value::String("text".into())));
}

#[test]
fn test_retype_field_converts_rolls_back_and_relocks() {
    use metafolder_core::metarecord::FieldType;
    let mut conn = test_conn();
    let db_id = repo_id();
    let m1 = create(&mut conn, db_id, vec![Field::new("rating", Value::Int(3))]);
    // Nothing coexists and must survive the retype untouched.
    let m2 = create(
        &mut conn,
        db_id,
        vec![Field::new("rating", Value::Int(5)), Field::new("rating", Value::Nothing)],
    );

    let head_before = metafolder_daemon::log::get_head(&conn).unwrap();

    let mut w = Writer::begin(&mut conn, db_id, None).unwrap();
    let summary = w.retype_field("rating", FieldType::String).unwrap();
    w.commit().unwrap();
    assert_eq!(summary.converted, 2, "both Int rows convert; the Nothing row is skipped");
    assert!(summary.fallback_uuids.is_empty(), "Int→String never falls back");

    let g1 = db::get_metarecord(&conn, m1.uuid).unwrap().unwrap();
    assert_eq!(g1.get("rating"), Some(&Value::String("3".into())));
    let g2 = db::get_metarecord(&conn, m2.uuid).unwrap().unwrap();
    assert!(g2.get_all("rating").contains(&&Value::String("5".into())));
    assert!(g2.get_all("rating").contains(&&Value::Nothing), "Nothing preserved");

    // The field is now String repo-wide: a conflicting Int write is rejected.
    let mut w = Writer::begin(&mut conn, db_id, None).unwrap();
    assert!(w.set_field(m1.uuid, "rating", Value::Int(9)).is_err());
    drop(w);

    // Rollback to before the retype restores the original Int values exactly.
    metafolder_daemon::log::navigate(&mut conn, db_id, head_before).unwrap();
    let g1 = db::get_metarecord(&conn, m1.uuid).unwrap().unwrap();
    assert_eq!(g1.get("rating"), Some(&Value::Int(3)));
}

#[test]
fn test_retype_field_records_fallbacks() {
    use metafolder_core::metarecord::FieldType;
    let mut conn = test_conn();
    let db_id = repo_id();
    let good = create(&mut conn, db_id, vec![Field::new("code", Value::String("42".into()))]);
    let bad = create(&mut conn, db_id, vec![Field::new("code", Value::String("oops".into()))]);

    let mut w = Writer::begin(&mut conn, db_id, None).unwrap();
    let summary = w.retype_field("code", FieldType::Int).unwrap();
    w.commit().unwrap();

    assert_eq!(summary.converted, 2);
    assert_eq!(summary.fallback_uuids, vec![bad.uuid], "only the un-parsable value fell back");
    let g = db::get_metarecord(&conn, good.uuid).unwrap().unwrap();
    assert_eq!(g.get("code"), Some(&Value::Int(42)));
    let b = db::get_metarecord(&conn, bad.uuid).unwrap().unwrap();
    assert_eq!(b.get("code"), Some(&Value::Int(0)), "un-parsable → sentinel 0");
}

#[test]
fn test_retype_string_to_reference_types() {
    use metafolder_core::metarecord::FieldType;
    use uuid::Uuid;
    let mut conn = test_conn();
    let db_id = repo_id();
    let target = Uuid::parse_str("8f3a2b1c4d5e6f708192a3b4c5d6e7f8").unwrap();
    let hex = "8f3a2b1c4d5e6f708192a3b4c5d6e7f8";

    // String → Ref: a valid hex uuid parses; junk falls back to Nothing.
    let good = create(&mut conn, db_id, vec![Field::new("link", Value::String(hex.into()))]);
    let bad = create(&mut conn, db_id, vec![Field::new("link", Value::String("nope".into()))]);
    let mut w = Writer::begin(&mut conn, db_id, None).unwrap();
    let summary = w.retype_field("link", FieldType::Ref).unwrap();
    w.commit().unwrap();
    assert_eq!(summary.converted, 2);
    assert_eq!(summary.fallback_uuids, vec![bad.uuid]);
    assert_eq!(db::get_metarecord(&conn, good.uuid).unwrap().unwrap().get("link"), Some(&Value::Ref(target)));
    assert_eq!(db::get_metarecord(&conn, bad.uuid).unwrap().unwrap().get("link"), Some(&Value::Nothing));
}

#[test]
fn test_retype_string_to_tree_ref_validates_forest() {
    use metafolder_core::metarecord::FieldType;
    let mut conn = test_conn();
    let db_id = repo_id();
    // A root form "/tags" is always valid; a parented form whose parent does not
    // exist violates the forest and is demoted to Nothing (not an abort).
    let root = create(&mut conn, db_id, vec![Field::new("cat", Value::String("/tags".into()))]);
    let orphan = create(
        &mut conn,
        db_id,
        vec![Field::new("cat", Value::String("8f3a2b1c4d5e6f708192a3b4c5d6e7f8/leaf".into()))],
    );

    let mut w = Writer::begin(&mut conn, db_id, None).unwrap();
    let summary = w.retype_field("cat", FieldType::TreeRef).unwrap();
    w.commit().unwrap();

    assert_eq!(summary.converted, 2);
    assert_eq!(summary.fallback_uuids, vec![orphan.uuid], "the orphan parent falls back to Nothing");
    assert_eq!(
        db::get_metarecord(&conn, root.uuid).unwrap().unwrap().get("cat"),
        Some(&Value::TreeRef { parent: None, name: "tags".into() })
    );
    assert_eq!(db::get_metarecord(&conn, orphan.uuid).unwrap().unwrap().get("cat"), Some(&Value::Nothing));
}

#[test]
fn test_value_type_probe_seeks_via_index() {
    // The established-type probe seeks the field_name range via idx_field_name
    // (stopping at the first non-Nothing row), never a full table scan.
    let conn = test_conn();
    let plan = query_plan(
        &conn,
        "SELECT value_type FROM field \
         WHERE field_name = 'rating' AND value_type != 'nothing' LIMIT 1",
    );
    assert!(
        plan.contains("idx_field_name"),
        "type probe should seek via idx_field_name, plan was: {plan}"
    );
    assert!(
        !plan.contains("SCAN field"),
        "type probe should not scan the field table, plan was: {plan}"
    );
}

#[test]
fn test_field_name_predicate_seeks_not_scans() {
    // IsPresent/Eq-style predicates filter the EAV `field` table by field_name.
    // Without an index leftmost on field_name this is a full table scan (the
    // table holds ~one row per field per metarecord); it must seek instead.
    let conn = test_conn();
    let plan = query_plan(
        &conn,
        "SELECT DISTINCT metarecord_uuid FROM field \
         WHERE field_name = 'mfr_path' AND value_type != 'nothing'",
    );
    assert!(
        plan.contains("idx_field_name"),
        "field_name predicate should seek via idx_field_name, plan was: {plan}"
    );
    assert!(
        !plan.contains("SCAN field"),
        "field_name predicate should not full-scan the field table, plan was: {plan}"
    );
}

#[test]
fn test_metarecord_listing_keyset_avoids_temp_sort() {
    // The paginated listing seeks by (db_id, metarecord_uuid) and reads rows
    // already ordered; without that composite index every page materialises the
    // whole repo and sorts it in a temp b-tree.
    let conn = test_conn();
    // The shape `list_entries_page` emits for a subsequent page (cursor present).
    let plan = query_plan(
        &conn,
        "SELECT m1.metarecord_uuid FROM metarecord_db m1 \
         WHERE m1.db_id = x'00' AND m1.metarecord_uuid > x'01' \
           AND (SELECT COUNT(*) FROM metarecord_db m2 \
                WHERE m2.metarecord_uuid = m1.metarecord_uuid) = 1 \
         ORDER BY m1.metarecord_uuid LIMIT 500",
    );
    assert!(
        plan.contains("idx_metarecord_db_uuid"),
        "listing should use idx_metarecord_db_uuid, plan was: {plan}"
    );
    assert!(
        !plan.contains("TEMP B-TREE"),
        "listing should not sort via a temp b-tree, plan was: {plan}"
    );
}

#[test]
fn test_init_schema_creates_all_tables() {
    let conn = test_conn();
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .unwrap();
    let tables: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    for expected in [
        "metarecord",
        "metarecord_db",
        "field",
        "revision",
        "operation",
        "op_snapshot",
        "log_head",
        "pending_operation",
    ] {
        assert!(tables.contains(&expected.to_string()), "missing table {expected}");
    }
}

#[test]
fn test_log_head_starts_null() {
    let conn = test_conn();
    let head: Option<i64> = conn
        .query_row("SELECT op_id FROM log_head WHERE singleton = 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(head, None);
}

#[test]
fn test_tree_unique_index_rejects_duplicate_position() {
    let mut conn = test_conn();
    let db_id = repo_id();
    let root = create(
        &mut conn,
        db_id,
        vec![Field::new("mfr_path", Value::TreeRef { parent: None, name: "".into() })],
    );
    create(
        &mut conn,
        db_id,
        vec![Field::new("mfr_path", Value::TreeRef { parent: Some(root.uuid), name: "a.mp3".into() })],
    );
    // Same (field_name, parent, name) again must fail.
    let mut w = Writer::begin(&mut conn, db_id, None).unwrap();
    let err = w
        .create_metarecord(vec![Field::new(
            "mfr_path",
            Value::TreeRef { parent: Some(root.uuid), name: "a.mp3".into() },
        )])
        .unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("occupied")
            || err.to_string().to_lowercase().contains("unique"),
        "unexpected error: {err}"
    );
}

// ── Value encoding roundtrip through the field table ──────────────────────────

#[test]
fn test_all_value_types_roundtrip() {
    let mut conn = test_conn();
    let db_id = repo_id();
    let target = create(&mut conn, db_id, vec![Field::new("label", Value::String("t".into()))]);
    let root = create(
        &mut conn,
        db_id,
        vec![Field::new("parent", Value::TreeRef { parent: None, name: "tag1".into() })],
    );
    let repo2 = Uuid::new_v4();
    let fields = vec![
        Field::new("a", Value::Nothing),
        Field::new("b", Value::String("hello".into())),
        Field::new("c", Value::Int(-99)),
        Field::new("d", Value::Float(1.25)),
        Field::new("e", Value::Bool(false)),
        Field::new("f", Value::Bool(true)),
        Field::new(
            "g",
            Value::DateTime(metafolder_core::date::iso_to_ms("2023-06-01T12:00:00Z").unwrap()),
        ),
        Field::new("h", Value::Ref(target.uuid)),
        Field::new("parent", Value::TreeRef { parent: Some(root.uuid), name: "félins".into() }),
        Field::new("j", Value::RefBase(repo2)),
        Field::new("k", Value::ExternalRef { repo: repo2, metarecord: target.uuid }),
    ];
    let created = create(&mut conn, db_id, fields.clone());

    let got = db::get_metarecord(&conn, created.uuid).unwrap().expect("entry must exist");
    assert_eq!(got.uuid, created.uuid);
    assert_eq!(got.db_ids, vec![db_id]);
    assert_eq!(got.fields.len(), fields.len());
    for (orig, ret) in fields.iter().zip(got.fields.iter()) {
        assert_eq!(orig.name, ret.name);
        assert_eq!(orig.value, ret.value, "value mismatch for field '{}'", orig.name);
        assert!(ret.id.is_some(), "field ids must be set in responses");
    }
}

#[test]
fn test_get_record_returns_none_for_unknown_uuid() {
    let conn = test_conn();
    assert!(db::get_metarecord(&conn, Uuid::new_v4()).unwrap().is_none());
}

#[test]
fn test_list_records_filters_by_db_id_and_sorts_by_uuid() {
    let mut conn = test_conn();
    let db1 = repo_id();
    let db2 = repo_id();
    let e1 = create(&mut conn, db1, vec![]);
    let e2 = create(&mut conn, db1, vec![]);
    let _other = create(&mut conn, db2, vec![]);

    let got = db::list_entries(&conn, db1).unwrap();
    let mut expected = vec![e1.uuid, e2.uuid];
    expected.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    assert_eq!(got, expected);
}

// ── Writer: create ────────────────────────────────────────────────────────────

#[test]
fn test_create_record_initial_state() {
    let mut conn = test_conn();
    let db_id = repo_id();
    let m = create(&mut conn, db_id, vec![Field::new("rating", Value::Int(5))]);
    assert_eq!(m.db_ids, vec![db_id]);
    assert_eq!(m.version, 0);
    assert_eq!(m.fields.len(), 1);
    assert!(m.fields[0].id.is_some());
}

#[test]
fn test_create_record_writes_log() {
    let mut conn = test_conn();
    let m = create(&mut conn, repo_id(), vec![Field::new("rating", Value::Int(5))]);

    let (op_type, entity, parent_id, seq): (String, Vec<u8>, Option<i64>, i64) = conn
        .query_row(
            "SELECT op_type, entity_uuid, parent_id, seq FROM operation",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(op_type, "create_metarecord");
    assert_eq!(entity, m.uuid.as_bytes().to_vec());
    assert_eq!(parent_id, None, "first operation has no parent");
    assert_eq!(seq, 1);

    // After-snapshot contains the created field rows; no before rows.
    let n_before: i64 = conn
        .query_row("SELECT COUNT(*) FROM op_snapshot WHERE is_new = 0", [], |r| r.get(0))
        .unwrap();
    let n_after: i64 = conn
        .query_row("SELECT COUNT(*) FROM op_snapshot WHERE is_new = 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n_before, 0);
    assert_eq!(n_after, 1);

    // HEAD points at the operation.
    let head: Option<i64> = conn
        .query_row("SELECT op_id FROM log_head WHERE singleton = 1", [], |r| r.get(0))
        .unwrap();
    let op_id: i64 = conn.query_row("SELECT id FROM operation", [], |r| r.get(0)).unwrap();
    assert_eq!(head, Some(op_id));
}

// ── Writer: set_field ─────────────────────────────────────────────────────────

#[test]
fn test_set_field_replaces_multimap_and_bumps_version() {
    let mut conn = test_conn();
    let db_id = repo_id();
    let m = create(
        &mut conn,
        db_id,
        vec![
            Field::new("tag", Value::String("jazz".into())),
            Field::new("tag", Value::String("live".into())),
        ],
    );

    let mut w = Writer::begin(&mut conn, db_id, None).unwrap();
    w.set_field(m.uuid, "tag", Value::String("blues".into())).unwrap();
    w.commit().unwrap();

    let got = db::get_metarecord(&conn, m.uuid).unwrap().unwrap();
    let tags = got.get_all("tag");
    assert_eq!(tags, vec![&Value::String("blues".into())]);
    assert_eq!(got.version, 1, "version must be incremented by the write");

    // Log: before-snapshot has the two old rows, after-snapshot the new one.
    let op_id: i64 = conn
        .query_row("SELECT id FROM operation WHERE op_type = 'set_field'", [], |r| r.get(0))
        .unwrap();
    let n_before: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM op_snapshot WHERE op_id = ?1 AND is_new = 0",
            [op_id],
            |r| r.get(0),
        )
        .unwrap();
    let n_after: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM op_snapshot WHERE op_id = ?1 AND is_new = 1",
            [op_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!((n_before, n_after), (2, 1));

    let version_before: Option<u64> = conn
        .query_row("SELECT entity_version_before FROM operation WHERE id = ?1", [op_id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(version_before, Some(0));
}

#[test]
fn test_set_field_on_unknown_record_fails() {
    let mut conn = test_conn();
    let mut w = Writer::begin(&mut conn, repo_id(), None).unwrap();
    assert!(w.set_field(Uuid::new_v4(), "rating", Value::Int(1)).is_err());
}

// ── Writer: append / replace / delete field ───────────────────────────────────

#[test]
fn test_append_field_keeps_existing_rows() {
    let mut conn = test_conn();
    let db_id = repo_id();
    let m = create(&mut conn, db_id, vec![Field::new("tag", Value::String("jazz".into()))]);

    let mut w = Writer::begin(&mut conn, db_id, None).unwrap();
    w.append_field(m.uuid, "tag", Value::String("live".into())).unwrap();
    w.commit().unwrap();

    let got = db::get_metarecord(&conn, m.uuid).unwrap().unwrap();
    assert_eq!(got.get_all("tag").len(), 2);
    assert_eq!(got.version, 1);
}

#[test]
fn test_replace_field_keeps_field_id() {
    let mut conn = test_conn();
    let db_id = repo_id();
    let m = create(
        &mut conn,
        db_id,
        vec![
            Field::new("tag", Value::String("jazz".into())),
            Field::new("tag", Value::String("live".into())),
        ],
    );
    let target_id = m.fields[0].id.unwrap();

    let mut w = Writer::begin(&mut conn, db_id, None).unwrap();
    w.replace_field(m.uuid, target_id, Value::String("blues".into())).unwrap();
    w.commit().unwrap();

    let got = db::get_metarecord(&conn, m.uuid).unwrap().unwrap();
    let replaced = got.fields.iter().find(|f| f.id == Some(target_id)).unwrap();
    assert_eq!(replaced.value, Value::String("blues".into()));
    assert_eq!(got.get_all("tag").len(), 2, "the sibling row must be untouched");
}

#[test]
fn test_replace_field_rejects_foreign_field_id() {
    let mut conn = test_conn();
    let db_id = repo_id();
    let m1 = create(&mut conn, db_id, vec![Field::new("a", Value::Int(1))]);
    let m2 = create(&mut conn, db_id, vec![Field::new("a", Value::Int(2))]);

    let mut w = Writer::begin(&mut conn, db_id, None).unwrap();
    let err = w.replace_field(m1.uuid, m2.fields[0].id.unwrap(), Value::Int(3)).unwrap_err();
    assert!(err.to_string().contains("not found"), "unexpected error: {err}");
}

#[test]
fn test_delete_field_removes_single_row() {
    let mut conn = test_conn();
    let db_id = repo_id();
    let m = create(
        &mut conn,
        db_id,
        vec![
            Field::new("tag", Value::String("jazz".into())),
            Field::new("tag", Value::String("live".into())),
        ],
    );
    let target_id = m.fields[0].id.unwrap();

    let mut w = Writer::begin(&mut conn, db_id, None).unwrap();
    w.delete_field(m.uuid, target_id).unwrap();
    w.commit().unwrap();

    let got = db::get_metarecord(&conn, m.uuid).unwrap().unwrap();
    assert_eq!(got.get_all("tag"), vec![&Value::String("live".into())]);
    assert_eq!(got.version, 1);
}

// ── Writer: delete entry ──────────────────────────────────────────────────────

#[test]
fn test_delete_record_removes_everything_and_snapshots_before() {
    let mut conn = test_conn();
    let db_id = repo_id();
    let m = create(
        &mut conn,
        db_id,
        vec![Field::new("a", Value::Int(1)), Field::new("b", Value::Int(2))],
    );

    let mut w = Writer::begin(&mut conn, db_id, None).unwrap();
    w.delete_metarecord(m.uuid).unwrap();
    w.commit().unwrap();

    assert!(db::get_metarecord(&conn, m.uuid).unwrap().is_none());
    let n_fields: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM field WHERE metarecord_uuid = ?1",
            [m.uuid.as_bytes().to_vec()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n_fields, 0);
    let n_db: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM metarecord_db WHERE metarecord_uuid = ?1",
            [m.uuid.as_bytes().to_vec()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n_db, 0);

    let op_id: i64 = conn
        .query_row("SELECT id FROM operation WHERE op_type = 'delete_metarecord'", [], |r| r.get(0))
        .unwrap();
    let n_before: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM op_snapshot WHERE op_id = ?1 AND is_new = 0",
            [op_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n_before, 2);
}

// ── Writer: revision grouping and HEAD chain ──────────────────────────────────

#[test]
fn test_multiple_ops_in_one_revision_chain() {
    let mut conn = test_conn();
    let db_id = repo_id();
    let m = create(&mut conn, db_id, vec![]);

    let mut w = Writer::begin(&mut conn, db_id, Some("batch".into())).unwrap();
    w.set_field(m.uuid, "a", Value::Int(1)).unwrap();
    w.set_field(m.uuid, "b", Value::Int(2)).unwrap();
    w.set_field(m.uuid, "c", Value::Int(3)).unwrap();
    w.commit().unwrap();

    // The three operations share one revision, with seq 1..3 and a parent chain.
    let rows: Vec<(i64, Option<i64>, i64, i64)> = conn
        .prepare(
            "SELECT id, parent_id, rev_id, seq FROM operation
             WHERE op_type = 'set_field' ORDER BY seq",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(rows.len(), 3);
    let rev = rows[0].2;
    assert!(rows.iter().all(|r| r.2 == rev));
    assert_eq!(rows.iter().map(|r| r.3).collect::<Vec<_>>(), vec![1, 2, 3]);
    assert_eq!(rows[1].1, Some(rows[0].0));
    assert_eq!(rows[2].1, Some(rows[1].0));

    let label: Option<String> = conn
        .query_row("SELECT label FROM revision WHERE id = ?1", [rev], |r| r.get(0))
        .unwrap();
    assert_eq!(label.as_deref(), Some("batch"));

    let head: Option<i64> = conn
        .query_row("SELECT op_id FROM log_head WHERE singleton = 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(head, Some(rows[2].0));

    // Version was bumped once per op.
    assert_eq!(db::get_metarecord(&conn, m.uuid).unwrap().unwrap().version, 3);
}

#[test]
fn test_large_revision_chain_across_bulk_chunks() {
    // More operations than the incremental flush threshold (4096) and the
    // multi-row INSERT chunks: the parent chain, seq numbering, snapshots
    // and HEAD must stay correct across both kinds of boundary.
    const N: i64 = 5000;
    let mut conn = test_conn();
    let db_id = repo_id();
    let m = create(&mut conn, db_id, vec![]);

    let mut w = Writer::begin(&mut conn, db_id, None).unwrap();
    for i in 0..N {
        w.set_field(m.uuid, &format!("f{i}"), Value::Int(i)).unwrap();
    }
    w.commit().unwrap();

    let rows: Vec<(i64, Option<i64>, i64)> = conn
        .prepare(
            "SELECT id, parent_id, seq FROM operation
             WHERE op_type = 'set_field' ORDER BY seq",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(rows.len(), N as usize);
    let create_op: i64 = conn
        .query_row("SELECT id FROM operation WHERE op_type = 'create_metarecord'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows[0].1, Some(create_op), "first op chains to the previous HEAD");
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(row.2, i as i64 + 1, "seq numbering");
        if i > 0 {
            assert_eq!(row.1, Some(rows[i - 1].0), "parent chain at op {i}");
        }
    }

    let head: Option<i64> = conn
        .query_row("SELECT op_id FROM log_head WHERE singleton = 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(head, Some(rows.last().unwrap().0));

    // One after-snapshot per set_field operation.
    let snapshots: i64 = conn
        .query_row("SELECT COUNT(*) FROM op_snapshot WHERE is_new = 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(snapshots, N);
    assert_eq!(db::get_metarecord(&conn, m.uuid).unwrap().unwrap().version, N as u64);
}

#[test]
fn test_ancestry_detects_cycle() {
    let mut conn = test_conn();
    let db_id = repo_id();
    let m = create(&mut conn, db_id, vec![]);
    let mut w = Writer::begin(&mut conn, db_id, None).unwrap();
    w.set_field(m.uuid, "a", Value::Int(1)).unwrap();
    w.set_field(m.uuid, "b", Value::Int(2)).unwrap();
    w.commit().unwrap();

    // Corrupt the log: point the oldest operation at the newest one.
    conn.execute(
        "UPDATE operation SET parent_id = (SELECT MAX(id) FROM operation)
         WHERE id = (SELECT MIN(id) FROM operation)",
        [],
    )
    .unwrap();

    let head = metafolder_daemon::log::get_head(&conn).unwrap().unwrap();
    let err = metafolder_daemon::log::ancestry(&conn, head).unwrap_err();
    assert!(err.to_string().contains("cycle"), "unexpected error: {err}");
}

#[test]
fn test_prune_reclaims_disk_space() {
    use metafolder_daemon::log::{self, PruneMode};

    let dir = std::env::temp_dir().join(format!("mf-prune-vacuum-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("db.sqlite");
    let mut conn = db::open_database(&path).unwrap();
    db::init_schema(&conn).unwrap();
    let db_id = repo_id();

    // One large revision (sizeable snapshots), then a tiny HEAD revision.
    let payload = "x".repeat(4096);
    let mut w = Writer::begin(&mut conn, db_id, None).unwrap();
    for _ in 0..256 {
        w.create_metarecord(vec![Field::new("payload", Value::String(payload.clone()))]).unwrap();
    }
    w.commit().unwrap();
    let mut w = Writer::begin(&mut conn, db_id, None).unwrap();
    w.create_metarecord(vec![]).unwrap();
    w.commit().unwrap();

    // Fold the WAL into the main file so before/after sizes are comparable.
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
    let total_size = |p: &std::path::Path| {
        let main = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        let wal = std::fs::metadata(p.with_extension("sqlite-wal")).map(|m| m.len()).unwrap_or(0);
        main + wal
    };
    let before = total_size(&path);

    let head = log::get_head(&conn).unwrap().unwrap();
    log::prune(&mut conn, PruneMode::Before, head).unwrap();

    let after = total_size(&path);
    assert!(
        after < before * 7 / 10,
        "prune should compact the database file: before={before}, after={after}"
    );

    drop(conn);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_empty_writer_leaves_no_revision() {
    let mut conn = test_conn();
    let w = Writer::begin(&mut conn, repo_id(), None).unwrap();
    w.commit().unwrap();
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM revision", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0);
}

#[test]
fn test_dropped_writer_rolls_back() {
    let mut conn = test_conn();
    let db_id = repo_id();
    {
        let mut w = Writer::begin(&mut conn, db_id, None).unwrap();
        w.create_metarecord(vec![Field::new("a", Value::Int(1))]).unwrap();
        // No commit: dropped here.
    }
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM metarecord", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0, "uncommitted writes must roll back");
}

// ── TreeRef validation ────────────────────────────────────────────────────────

#[test]
fn test_tree_ref_parent_must_exist() {
    let mut conn = test_conn();
    let mut w = Writer::begin(&mut conn, repo_id(), None).unwrap();
    let err = w
        .create_metarecord(vec![Field::new(
            "mfr_path",
            Value::TreeRef { parent: Some(Uuid::new_v4()), name: "x".into() },
        )])
        .unwrap_err();
    assert!(err.to_string().contains("parent"), "unexpected error: {err}");
}

#[test]
fn test_tree_ref_parent_must_have_same_tree_field() {
    let mut conn = test_conn();
    let db_id = repo_id();
    // Parent exists but has no 'mfr_path' TreeRef field.
    let parent = create(&mut conn, db_id, vec![Field::new("label", Value::String("p".into()))]);
    let mut w = Writer::begin(&mut conn, db_id, None).unwrap();
    let err = w
        .create_metarecord(vec![Field::new(
            "mfr_path",
            Value::TreeRef { parent: Some(parent.uuid), name: "x".into() },
        )])
        .unwrap_err();
    assert!(err.to_string().contains("parent"), "unexpected error: {err}");
}

#[test]
fn test_tree_ref_cycle_rejected() {
    let mut conn = test_conn();
    let db_id = repo_id();
    let a = create(
        &mut conn,
        db_id,
        vec![Field::new("parent", Value::TreeRef { parent: None, name: "a".into() })],
    );
    let b = create(
        &mut conn,
        db_id,
        vec![Field::new("parent", Value::TreeRef { parent: Some(a.uuid), name: "b".into() })],
    );
    // Re-pointing a under b would create a cycle a → b → a.
    let mut w = Writer::begin(&mut conn, db_id, None).unwrap();
    let err = w
        .set_field(a.uuid, "parent", Value::TreeRef { parent: Some(b.uuid), name: "a".into() })
        .unwrap_err();
    assert!(err.to_string().contains("cycle"), "unexpected error: {err}");
}

#[test]
fn test_tree_ref_self_parent_rejected() {
    let mut conn = test_conn();
    let db_id = repo_id();
    let a = create(
        &mut conn,
        db_id,
        vec![Field::new("parent", Value::TreeRef { parent: None, name: "a".into() })],
    );
    let mut w = Writer::begin(&mut conn, db_id, None).unwrap();
    let err = w
        .set_field(a.uuid, "parent", Value::TreeRef { parent: Some(a.uuid), name: "a".into() })
        .unwrap_err();
    assert!(err.to_string().contains("cycle"), "unexpected error: {err}");
}

#[test]
fn test_tree_ref_depth_limit() {
    let mut conn = test_conn();
    let db_id = repo_id();
    // Build a chain of exactly 1000 nodes (depth 1000): root is depth 1.
    let mut w = Writer::begin(&mut conn, db_id, None).unwrap();
    let root = w
        .create_metarecord(vec![Field::new("parent", Value::TreeRef { parent: None, name: "n1".into() })])
        .unwrap();
    let mut prev = root.uuid;
    for i in 2..=1000 {
        let e = w
            .create_metarecord(vec![Field::new(
                "parent",
                Value::TreeRef { parent: Some(prev), name: format!("n{i}") },
            )])
            .unwrap();
        prev = e.uuid;
    }
    // Node 1001 exceeds the limit.
    let err = w
        .create_metarecord(vec![Field::new(
            "parent",
            Value::TreeRef { parent: Some(prev), name: "n1001".into() },
        )])
        .unwrap_err();
    assert!(err.to_string().contains("depth"), "unexpected error: {err}");
}

// ── Reserved fields ───────────────────────────────────────────────────────────

#[test]
fn test_reserved_mfr_requires_force() {
    assert!(reserved::check_writable("mfr_path", false).is_err());
    assert!(reserved::check_writable("mfr_size", false).is_err());
    assert!(reserved::check_writable("mfr_path", true).is_ok());
}

#[test]
fn test_reserved_known_mf_fields_are_writable() {
    for name in ["mf_watch", "mf_ignore", "mf_schema"] {
        assert!(reserved::check_writable(name, false).is_ok(), "{name} must be writable");
    }
}

#[test]
fn test_reserved_unknown_mf_field_rejected() {
    assert!(reserved::check_writable("mf_unknown", false).is_err());
    assert!(reserved::check_writable("mf_unknown", true).is_err(), "force does not allow typos");
}

#[test]
fn test_user_fields_are_writable() {
    assert!(reserved::check_writable("rating", false).is_ok());
    assert!(reserved::check_writable("mfrating", false).is_ok(), "prefix check needs underscore");
}

// ── OpType ────────────────────────────────────────────────────────────────────

#[test]
fn test_op_type_string_roundtrip() {
    for op in [
        OpType::CreateRecord,
        OpType::DeleteRecord,
        OpType::SetField,
        OpType::AppendField,
        OpType::DeleteField,
        OpType::FileDeleted,
        OpType::FileMoved,
        OpType::FileModified,
        OpType::Unknown,
    ] {
        assert_eq!(OpType::parse(op.as_str()).unwrap(), op);
    }
    assert_eq!(OpType::CreateRecord.as_str(), "create_metarecord");
    assert!(OpType::parse("bogus").is_none());
}
