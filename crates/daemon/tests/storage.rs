//! Integration tests for the storage layer: SQLite schema, value encoding,
//! the logged write flow (Writer), TreeRef validation, reserved fields.

use metafolder_core::metarecord::{Field, TreeName, Value};
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

/// Creates an entry through a single-use Writer and returns it.
fn create(conn: &mut Connection, fields: Vec<Field>) -> metafolder_core::metarecord::MetaRecord {
    let mut w = Writer::begin(conn, None).unwrap();
    let m = w.create_metarecord(fields).unwrap();
    w.commit().unwrap();
    m
}

// ── Schema ────────────────────────────────────────────────────────────────────

/// EXPLAIN QUERY PLAN `detail` lines for `sql`, joined into one string.
fn query_plan(conn: &Connection, sql: &str) -> String {
    let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    let rows: Vec<String> =
        stmt.query_map([], |r| r.get::<_, String>(3)).unwrap().collect::<Result<_, _>>().unwrap();
    rows.join(" | ")
}

// ── One value type per field name (invariant) ──────────────────────────────────

#[test]
fn test_field_first_write_establishes_type() {
    // The first non-Nothing write of a name succeeds and fixes its type.
    let mut conn = test_conn();
    let m = create(&mut conn, vec![Field::new("rating", Value::Int(5))]);
    let got = db::get_metarecord(&conn, m.uuid).unwrap().unwrap();
    assert_eq!(got.get("rating"), Some(&Value::Int(5)));
}

#[test]
fn test_field_rejects_conflicting_value_type() {
    // Once `rating` is an Int repo-wide, a String write to it is rejected (400).
    let mut conn = test_conn();
    create(&mut conn, vec![Field::new("rating", Value::Int(5))]);

    let mut w = Writer::begin(&mut conn, None).unwrap();
    let err = w.set_field(Uuid::new_v4(), "rating", Value::String("five".into())).unwrap_err();
    assert!(err.to_string().contains("type"), "expected a type-conflict error, got: {err}");
}

#[test]
fn test_field_rejects_conflicting_type_within_one_create() {
    // Two rows of the same name with different types in a single create are rejected.
    let mut conn = test_conn();
    let mut w = Writer::begin(&mut conn, None).unwrap();
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
    let m = create(&mut conn, vec![Field::new("rating", Value::Int(5))]);

    let mut w = Writer::begin(&mut conn, None).unwrap();
    w.set_field(m.uuid, "rating", Value::Nothing).unwrap();
    w.commit().unwrap();
}

#[test]
fn test_field_rows_for_matches_per_record_reads() {
    // The batched readers must reproduce, for a set of metarecords, exactly what
    // the per-record `get_field_rows` / `get_version` return — grouped by owner,
    // per-record rows in id order, missing uuids absent.
    let mut conn = test_conn();
    let a = create(
        &mut conn,
        vec![
            Field::new("tag", Value::String("x".into())),
            Field::new("tag", Value::String("y".into())),
            Field::new("rating", Value::Int(5)),
        ],
    );
    let b = create(&mut conn, vec![Field::new("note", Value::Nothing)]);
    let empty = create(&mut conn, vec![]);
    let absent = Uuid::new_v4();

    let want = [a.uuid, b.uuid, empty.uuid, absent];
    let rows = db::field_rows_for(&conn, &want).unwrap();
    let versions = db::versions_for(&conn, &want).unwrap();

    for uuid in [a.uuid, b.uuid, empty.uuid] {
        assert_eq!(
            rows.get(&uuid).cloned().unwrap_or_default(),
            db::get_field_rows(&conn, uuid).unwrap()
        );
        assert_eq!(versions.get(&uuid).copied(), db::get_version(&conn, uuid).unwrap());
    }
    // An unknown uuid contributes no rows and no version.
    assert!(rows.get(&absent).is_none_or(|v| v.is_empty()));
    assert_eq!(versions.get(&absent), None);
}

#[test]
fn test_for_each_field_row_matches_per_record_scan() {
    // A single streaming scan of the whole `field` table must yield exactly the
    // same (uuid, id, name, value) rows as the per-metarecord `get_field_rows`
    // walk it replaces in the index build — every row, once, with its owner.
    let mut conn = test_conn();
    let a = create(
        &mut conn,
        vec![
            Field::new("tag", Value::String("x".into())),
            Field::new("tag", Value::String("y".into())), // multi-map
            Field::new("rating", Value::Int(5)),
        ],
    );
    let b = create(
        &mut conn,
        vec![Field::new("note", Value::Nothing)], // explicit absence
    );
    let _empty = create(&mut conn, vec![]); // no fields at all

    // Reference: the per-record accessor, gathered into (uuid, id) -> value.
    let mut expected: std::collections::HashMap<(Uuid, i64), (String, Value)> =
        std::collections::HashMap::new();
    for uuid in db::list_entries(&conn).unwrap() {
        for row in db::get_field_rows(&conn, uuid).unwrap() {
            expected.insert((uuid, row.id), (row.name, row.value));
        }
    }

    // The streaming scan must reproduce it exactly.
    let mut got: std::collections::HashMap<(Uuid, i64), (String, Value)> =
        std::collections::HashMap::new();
    db::for_each_field_row(&conn, |uuid, row| {
        let prev = got.insert((uuid, row.id), (row.name, row.value));
        assert!(prev.is_none(), "row id {} streamed twice", row.id);
        Ok(())
    })
    .unwrap();

    assert_eq!(got, expected);
    assert_eq!(a.fields.len(), 3);
    assert_eq!(b.fields.len(), 1);
}

#[test]
fn test_field_type_unlocks_when_empty() {
    // With no non-Nothing rows left, the name's type is unestablished again and a
    // new (different) type may be written.
    let mut conn = test_conn();
    let m = create(&mut conn, vec![Field::new("note", Value::Int(1))]);

    let mut w = Writer::begin(&mut conn, None).unwrap();
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
    let m = create(&mut conn, vec![Field::new("note", Value::Int(1))]);

    let mut w = Writer::begin(&mut conn, None).unwrap();
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
    let m1 = create(&mut conn, vec![Field::new("rating", Value::Int(3))]);
    // Nothing coexists and must survive the retype untouched.
    let m2 = create(
        &mut conn,
        vec![Field::new("rating", Value::Int(5)), Field::new("rating", Value::Nothing)],
    );

    let head_before = metafolder_daemon::log::get_head(&conn).unwrap();

    let mut w = Writer::begin(&mut conn, None).unwrap();
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
    let mut w = Writer::begin(&mut conn, None).unwrap();
    assert!(w.set_field(m1.uuid, "rating", Value::Int(9)).is_err());
    drop(w);

    // Rollback to before the retype restores the original Int values exactly.
    metafolder_daemon::log::navigate(&mut conn, head_before).unwrap();
    let g1 = db::get_metarecord(&conn, m1.uuid).unwrap().unwrap();
    assert_eq!(g1.get("rating"), Some(&Value::Int(3)));
}

#[test]
fn test_retype_field_records_fallbacks() {
    use metafolder_core::metarecord::FieldType;
    let mut conn = test_conn();
    let good = create(&mut conn, vec![Field::new("code", Value::String("42".into()))]);
    let bad = create(&mut conn, vec![Field::new("code", Value::String("oops".into()))]);

    let mut w = Writer::begin(&mut conn, None).unwrap();
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
    let target = Uuid::parse_str("8f3a2b1c4d5e6f708192a3b4c5d6e7f8").unwrap();
    let hex = "8f3a2b1c4d5e6f708192a3b4c5d6e7f8";

    // String → Ref: a valid hex uuid parses; junk falls back to Nothing.
    let good = create(&mut conn, vec![Field::new("link", Value::String(hex.into()))]);
    let bad = create(&mut conn, vec![Field::new("link", Value::String("nope".into()))]);
    let mut w = Writer::begin(&mut conn, None).unwrap();
    let summary = w.retype_field("link", FieldType::Ref).unwrap();
    w.commit().unwrap();
    assert_eq!(summary.converted, 2);
    assert_eq!(summary.fallback_uuids, vec![bad.uuid]);
    assert_eq!(
        db::get_metarecord(&conn, good.uuid).unwrap().unwrap().get("link"),
        Some(&Value::Ref(target))
    );
    assert_eq!(
        db::get_metarecord(&conn, bad.uuid).unwrap().unwrap().get("link"),
        Some(&Value::Nothing)
    );
}

#[test]
fn test_retype_string_to_tree_ref_validates_forest() {
    use metafolder_core::metarecord::FieldType;
    let mut conn = test_conn();
    // A root form "/tags" is always valid; a parented form whose parent does not
    // exist violates the forest and is demoted to Nothing (not an abort).
    let root = create(&mut conn, vec![Field::new("cat", Value::String("/tags".into()))]);
    let orphan = create(
        &mut conn,
        vec![Field::new("cat", Value::String("8f3a2b1c4d5e6f708192a3b4c5d6e7f8/leaf".into()))],
    );

    let mut w = Writer::begin(&mut conn, None).unwrap();
    let summary = w.retype_field("cat", FieldType::TreeRef).unwrap();
    w.commit().unwrap();

    assert_eq!(summary.converted, 2);
    assert_eq!(
        summary.fallback_uuids,
        vec![orphan.uuid],
        "the orphan parent falls back to Nothing"
    );
    assert_eq!(
        db::get_metarecord(&conn, root.uuid).unwrap().unwrap().get("cat"),
        Some(&Value::TreeRef { parent: None, name: "tags".into() })
    );
    assert_eq!(
        db::get_metarecord(&conn, orphan.uuid).unwrap().unwrap().get("cat"),
        Some(&Value::Nothing)
    );
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
fn test_distinct_value_types_are_read_from_the_index_alone() {
    // "Which value types does this field hold?" gates `osm` path mode. Asked as
    // "is there a row of another type?" it fetched every row of the field to
    // read its `value_type` — 81 ms on a 50k-row field, which was the whole cost
    // of a multi-term OSM path query. Asked as a DISTINCT it is answered from
    // the covering index, touching no table row at all.
    let conn = test_conn();
    let plan =
        query_plan(&conn, "SELECT DISTINCT value_type FROM field WHERE field_name = 'mfr_path'");
    assert!(
        plan.contains("idx_field_name_type"),
        "the type probe should use the covering index, plan was: {plan}"
    );
    assert!(
        plan.contains("COVERING"),
        "the type probe should not touch the table, plan was: {plan}"
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
    // The paginated listing seeks the `metarecord` primary key and reads rows
    // already ordered; the keyset cursor must not force a temp b-tree sort.
    let conn = test_conn();
    // The shape `list_entries_page` emits for a subsequent page (cursor present).
    let plan =
        query_plan(&conn, "SELECT uuid FROM metarecord WHERE uuid > x'01' ORDER BY uuid LIMIT 500");
    assert!(
        !plan.contains("TEMP B-TREE"),
        "listing should not sort via a temp b-tree, plan was: {plan}"
    );
    assert!(
        plan.contains("SEARCH") && !plan.contains("SCAN"),
        "listing should seek (not scan) the metarecord primary key, plan was: {plan}"
    );
}

#[test]
fn test_init_schema_creates_all_tables() {
    let conn = test_conn();
    let mut stmt =
        conn.prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name").unwrap();
    let tables: Vec<String> =
        stmt.query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
    for expected in [
        "metarecord",
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
    let head: Option<i64> =
        conn.query_row("SELECT op_id FROM log_head WHERE singleton = 1", [], |r| r.get(0)).unwrap();
    assert_eq!(head, None);
}

#[test]
fn test_tree_unique_index_rejects_duplicate_position() {
    let mut conn = test_conn();
    let root = create(
        &mut conn,
        vec![Field::new("mfr_path", Value::TreeRef { parent: None, name: "".into() })],
    );
    create(
        &mut conn,
        vec![Field::new(
            "mfr_path",
            Value::TreeRef { parent: Some(root.uuid), name: "a.mp3".into() },
        )],
    );
    // Same (field_name, parent, name) again must fail.
    let mut w = Writer::begin(&mut conn, None).unwrap();
    let err = w
        .create_metarecord(vec![Field::new(
            "mfr_path",
            Value::TreeRef { parent: Some(root.uuid), name: "a.mp3".into() },
        )])
        .unwrap_err();
    // The raw SQLite UNIQUE error must be mapped to the clean domain message
    // (a 400), not leak as an internal error — SQLite names the columns, not the
    // index, so the mapping keys off `value_name`.
    assert!(
        err.to_string().to_lowercase().contains("occupied"),
        "expected the mapped 'tree position already occupied' error, got: {err}"
    );
}

#[test]
fn test_mfr_path_is_single_valued() {
    let mut conn = test_conn();
    let root = create(
        &mut conn,
        vec![Field::new("mfr_path", Value::TreeRef { parent: None, name: "".into() })],
    );
    let file = create(
        &mut conn,
        vec![Field::new(
            "mfr_path",
            Value::TreeRef { parent: Some(root.uuid), name: "a.mp3".into() },
        )],
    );

    // Appending a second mfr_path is rejected — a metarecord tracks one path.
    let mut w = Writer::begin(&mut conn, None).unwrap();
    let err = w
        .append_field(
            file.uuid,
            "mfr_path",
            Value::TreeRef { parent: Some(root.uuid), name: "b.mp3".into() },
        )
        .unwrap_err();
    assert!(err.to_string().to_lowercase().contains("single-valued"), "got: {err}");
    drop(w);

    // Creating a metarecord with two mfr_path fields is likewise rejected.
    let mut w = Writer::begin(&mut conn, None).unwrap();
    let err = w
        .create_metarecord(vec![
            Field::new("mfr_path", Value::TreeRef { parent: Some(root.uuid), name: "x".into() }),
            Field::new("mfr_path", Value::TreeRef { parent: Some(root.uuid), name: "y".into() }),
        ])
        .unwrap_err();
    assert!(err.to_string().to_lowercase().contains("single-valued"), "got: {err}");
}

// ── Value encoding roundtrip through the field table ──────────────────────────

#[test]
fn test_all_value_types_roundtrip() {
    let mut conn = test_conn();
    let target = create(&mut conn, vec![Field::new("label", Value::String("t".into()))]);
    let root = create(
        &mut conn,
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
    let created = create(&mut conn, fields.clone());

    let got = db::get_metarecord(&conn, created.uuid).unwrap().expect("entry must exist");
    assert_eq!(got.uuid, created.uuid);
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
fn test_list_records_sorts_by_uuid() {
    let mut conn = test_conn();
    let e1 = create(&mut conn, vec![]);
    let e2 = create(&mut conn, vec![]);
    let e3 = create(&mut conn, vec![]);

    let got = db::list_entries(&conn).unwrap();
    let mut expected = vec![e1.uuid, e2.uuid, e3.uuid];
    expected.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    assert_eq!(got, expected);
}

// ── Writer: create ────────────────────────────────────────────────────────────

#[test]
fn test_create_record_initial_state() {
    let mut conn = test_conn();
    let m = create(&mut conn, vec![Field::new("rating", Value::Int(5))]);
    assert_eq!(m.version, 0);
    assert_eq!(m.fields.len(), 1);
    assert!(m.fields[0].id.is_some());
}

#[test]
fn test_create_record_writes_log() {
    let mut conn = test_conn();
    let m = create(&mut conn, vec![Field::new("rating", Value::Int(5))]);

    let (op_type, entity, parent_id, seq): (String, Vec<u8>, Option<i64>, i64) = conn
        .query_row("SELECT op_type, entity_uuid, parent_id, seq FROM operation", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
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
    let head: Option<i64> =
        conn.query_row("SELECT op_id FROM log_head WHERE singleton = 1", [], |r| r.get(0)).unwrap();
    let op_id: i64 = conn.query_row("SELECT id FROM operation", [], |r| r.get(0)).unwrap();
    assert_eq!(head, Some(op_id));
}

// ── Writer: set_field ─────────────────────────────────────────────────────────

#[test]
fn test_set_field_replaces_multimap_and_bumps_version() {
    let mut conn = test_conn();
    let m = create(
        &mut conn,
        vec![
            Field::new("tag", Value::String("jazz".into())),
            Field::new("tag", Value::String("live".into())),
        ],
    );

    let mut w = Writer::begin(&mut conn, None).unwrap();
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
    let mut w = Writer::begin(&mut conn, None).unwrap();
    assert!(w.set_field(Uuid::new_v4(), "rating", Value::Int(1)).is_err());
}

// ── Writer: append / replace / delete field ───────────────────────────────────

#[test]
fn test_append_field_keeps_existing_rows() {
    let mut conn = test_conn();
    let m = create(&mut conn, vec![Field::new("tag", Value::String("jazz".into()))]);

    let mut w = Writer::begin(&mut conn, None).unwrap();
    w.append_field(m.uuid, "tag", Value::String("live".into())).unwrap();
    w.commit().unwrap();

    let got = db::get_metarecord(&conn, m.uuid).unwrap().unwrap();
    assert_eq!(got.get_all("tag").len(), 2);
    assert_eq!(got.version, 1);
}

#[test]
fn test_replace_field_keeps_field_id() {
    let mut conn = test_conn();
    let m = create(
        &mut conn,
        vec![
            Field::new("tag", Value::String("jazz".into())),
            Field::new("tag", Value::String("live".into())),
        ],
    );
    let target_id = m.fields[0].id.unwrap();

    let mut w = Writer::begin(&mut conn, None).unwrap();
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
    let m1 = create(&mut conn, vec![Field::new("a", Value::Int(1))]);
    let m2 = create(&mut conn, vec![Field::new("a", Value::Int(2))]);

    let mut w = Writer::begin(&mut conn, None).unwrap();
    let err = w.replace_field(m1.uuid, m2.fields[0].id.unwrap(), Value::Int(3)).unwrap_err();
    assert!(err.to_string().contains("not found"), "unexpected error: {err}");
}

#[test]
fn test_delete_field_removes_single_row() {
    let mut conn = test_conn();
    let m = create(
        &mut conn,
        vec![
            Field::new("tag", Value::String("jazz".into())),
            Field::new("tag", Value::String("live".into())),
        ],
    );
    let target_id = m.fields[0].id.unwrap();

    let mut w = Writer::begin(&mut conn, None).unwrap();
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
    let m = create(&mut conn, vec![Field::new("a", Value::Int(1)), Field::new("b", Value::Int(2))]);

    let mut w = Writer::begin(&mut conn, None).unwrap();
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
    let n_rec: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM metarecord WHERE uuid = ?1",
            [m.uuid.as_bytes().to_vec()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n_rec, 0);

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
    let m = create(&mut conn, vec![]);

    let mut w = Writer::begin(&mut conn, Some("batch".into())).unwrap();
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

    let label: Option<String> =
        conn.query_row("SELECT label FROM revision WHERE id = ?1", [rev], |r| r.get(0)).unwrap();
    assert_eq!(label.as_deref(), Some("batch"));

    let head: Option<i64> =
        conn.query_row("SELECT op_id FROM log_head WHERE singleton = 1", [], |r| r.get(0)).unwrap();
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
    let m = create(&mut conn, vec![]);

    let mut w = Writer::begin(&mut conn, None).unwrap();
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

    let head: Option<i64> =
        conn.query_row("SELECT op_id FROM log_head WHERE singleton = 1", [], |r| r.get(0)).unwrap();
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
    let m = create(&mut conn, vec![]);
    let mut w = Writer::begin(&mut conn, None).unwrap();
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

    let dir = TempDir::new("prune-vacuum");
    let path = dir.join("db.sqlite");
    let mut conn = db::open_database(&path).unwrap();
    db::init_schema(&conn).unwrap();

    // One large revision (sizeable snapshots), then a tiny HEAD revision.
    let payload = "x".repeat(4096);
    let mut w = Writer::begin(&mut conn, None).unwrap();
    for _ in 0..256 {
        w.create_metarecord(vec![Field::new("payload", Value::String(payload.clone()))]).unwrap();
    }
    w.commit().unwrap();
    let mut w = Writer::begin(&mut conn, None).unwrap();
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
    let w = Writer::begin(&mut conn, None).unwrap();
    w.commit().unwrap();
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM revision", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0);
}

#[test]
fn test_dropped_writer_rolls_back() {
    let mut conn = test_conn();
    {
        let mut w = Writer::begin(&mut conn, None).unwrap();
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
    let mut w = Writer::begin(&mut conn, None).unwrap();
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
    // Parent exists but has no 'mfr_path' TreeRef field.
    let parent = create(&mut conn, vec![Field::new("label", Value::String("p".into()))]);
    let mut w = Writer::begin(&mut conn, None).unwrap();
    let err = w
        .create_metarecord(vec![Field::new(
            "mfr_path",
            Value::TreeRef { parent: Some(parent.uuid), name: "x".into() },
        )])
        .unwrap_err();
    assert!(err.to_string().contains("parent"), "unexpected error: {err}");
}

// ── Undecodable names (spec-data-model "Tree names") ─────────────────────────

/// Reads back the tree name stored for `uuid`'s `mfr_path`.
fn tree_name(conn: &Connection, uuid: Uuid) -> TreeName {
    let m = db::get_metarecord(conn, uuid).unwrap().expect("metarecord");
    let field = m.fields.iter().find(|f| f.name == "mfr_path").expect("mfr_path");
    match &field.value {
        Value::TreeRef { name, .. } => name.clone(),
        other => panic!("not a tree_ref: {other:?}"),
    }
}

#[test]
fn test_an_undecodable_name_round_trips_through_the_database() {
    // "café.mp4" in latin-1: a POSIX name is a byte string, and the database
    // must give back the exact bytes — they are what opens the file.
    let mut conn = test_conn();
    let root = create(
        &mut conn,
        vec![Field::new("mfr_path", Value::TreeRef { parent: None, name: "".into() })],
    );
    let name = TreeName::from_bytes(b"caf\xe9.mp4".to_vec());
    let file = create(
        &mut conn,
        vec![Field::new(
            "mfr_path",
            Value::TreeRef { parent: Some(root.uuid), name: name.clone() },
        )],
    );
    assert_eq!(tree_name(&conn, file.uuid), name);
    assert_eq!(tree_name(&conn, file.uuid).as_bytes(), b"caf\xe9.mp4");
}

#[test]
fn test_two_names_differing_only_in_undecodable_bytes_are_distinct_siblings() {
    // They display identically, so a text-keyed forest index would reject the
    // second as a duplicate. The forest is keyed on the bytes, so both exist.
    let mut conn = test_conn();
    let root = create(
        &mut conn,
        vec![Field::new("mfr_path", Value::TreeRef { parent: None, name: "".into() })],
    );
    let one = TreeName::from_bytes(b"caf\xe9.mp4".to_vec());
    let two = TreeName::from_bytes(b"caf\xff.mp4".to_vec());
    assert_eq!(one.display(), two.display(), "the test is pointless unless they collide as text");

    let a = create(
        &mut conn,
        vec![Field::new("mfr_path", Value::TreeRef { parent: Some(root.uuid), name: one.clone() })],
    );
    let b = create(
        &mut conn,
        vec![Field::new("mfr_path", Value::TreeRef { parent: Some(root.uuid), name: two.clone() })],
    );
    assert_eq!(tree_name(&conn, a.uuid), one);
    assert_eq!(tree_name(&conn, b.uuid), two);
}

#[test]
fn test_the_same_name_twice_in_one_directory_is_still_rejected() {
    // The uniqueness the forest relies on must survive the move to bytes.
    let mut conn = test_conn();
    let root = create(
        &mut conn,
        vec![Field::new("mfr_path", Value::TreeRef { parent: None, name: "".into() })],
    );
    let name = TreeName::from_bytes(b"caf\xe9.mp4".to_vec());
    create(
        &mut conn,
        vec![Field::new(
            "mfr_path",
            Value::TreeRef { parent: Some(root.uuid), name: name.clone() },
        )],
    );
    let mut w = Writer::begin(&mut conn, None).unwrap();
    let err = w
        .create_metarecord(vec![Field::new(
            "mfr_path",
            Value::TreeRef { parent: Some(root.uuid), name },
        )])
        .unwrap_err();
    assert!(err.to_string().contains("already occupied"), "unexpected error: {err}");
}

/// Reverts `field` to the pre-TreeName schema: no `value_name_bytes`, and the
/// forest index keyed on the displayed text. What an existing repository holds.
///
/// Rebuilt rather than `DROP COLUMN`-ed: SQLite rewrites the stored CREATE
/// TABLE text to drop a column, which trips over the trailing comment on the
/// last one. A rebuild also matches what an old database really looks like.
fn downgrade_to_text_keyed_forest(conn: &Connection) {
    conn.execute_batch(
        "CREATE TABLE field_old (
             id              INTEGER PRIMARY KEY AUTOINCREMENT,
             metarecord_uuid BLOB    NOT NULL,
             field_name      TEXT    NOT NULL,
             value_type      TEXT    NOT NULL,
             value_text      TEXT,
             value_int       INTEGER,
             value_real      REAL,
             value_uuid      BLOB,
             value_ref_repo  BLOB,
             value_name      TEXT
         );
         INSERT INTO field_old SELECT id, metarecord_uuid, field_name, value_type, value_text,
                value_int, value_real, value_uuid, value_ref_repo, value_name FROM field;
         DROP TABLE field;
         ALTER TABLE field_old RENAME TO field;
         CREATE UNIQUE INDEX idx_field_tree ON field(field_name, value_uuid, value_name)
             WHERE value_type = 'tree_ref';
         CREATE UNIQUE INDEX idx_mfr_path_single ON field(metarecord_uuid)
             WHERE field_name = 'mfr_path';",
    )
    .unwrap();
}

#[test]
fn test_an_existing_repository_migrates_to_byte_keyed_names_on_open() {
    let dir = common::TempDir::new("migrate-tree-names");
    let path = dir.path().join("db.sqlite");

    // A repository written before names carried their bytes.
    let mut conn = db::open_database(&path).unwrap();
    db::init_schema(&conn).unwrap();
    let root = create(
        &mut conn,
        vec![Field::new("mfr_path", Value::TreeRef { parent: None, name: "".into() })],
    );
    let kept = create(
        &mut conn,
        vec![Field::new(
            "mfr_path",
            Value::TreeRef { parent: Some(root.uuid), name: "vidéo.mp4".into() },
        )],
    );
    downgrade_to_text_keyed_forest(&conn);
    drop(conn);

    // Opening it runs the migration.
    let mut conn = db::open_database(&path).unwrap();

    // Existing names are untouched — the bytes are derived from the text, which
    // is lossless because an undecodable name could not have been stored here.
    assert_eq!(tree_name(&conn, kept.uuid), TreeName::from("vidéo.mp4".to_string()));
    assert!(tree_name(&conn, kept.uuid).is_exact());

    // ...and the forest now keys on the bytes, so a name that no text can
    // represent becomes storable alongside one that displays identically.
    let one = TreeName::from_bytes(b"caf\xe9.mp4".to_vec());
    let two = TreeName::from_bytes(b"caf\xff.mp4".to_vec());
    let a = create(
        &mut conn,
        vec![Field::new("mfr_path", Value::TreeRef { parent: Some(root.uuid), name: one.clone() })],
    );
    let b = create(
        &mut conn,
        vec![Field::new("mfr_path", Value::TreeRef { parent: Some(root.uuid), name: two.clone() })],
    );
    assert_eq!(tree_name(&conn, a.uuid), one);
    assert_eq!(tree_name(&conn, b.uuid), two);
}

#[test]
fn test_migrating_an_already_migrated_repository_is_a_no_op() {
    let dir = common::TempDir::new("migrate-tree-names-idempotent");
    let path = dir.path().join("db.sqlite");
    let mut conn = db::open_database(&path).unwrap();
    db::init_schema(&conn).unwrap();
    let root = create(
        &mut conn,
        vec![Field::new("mfr_path", Value::TreeRef { parent: None, name: "".into() })],
    );
    let name = TreeName::from_bytes(b"caf\xe9.mp4".to_vec());
    let file = create(
        &mut conn,
        vec![Field::new(
            "mfr_path",
            Value::TreeRef { parent: Some(root.uuid), name: name.clone() },
        )],
    );
    drop(conn);

    // Re-opening must neither re-derive the bytes from the (lossy) text nor
    // fail: the back-fill runs only when the column is being added.
    let conn = db::open_database(&path).unwrap();
    assert_eq!(tree_name(&conn, file.uuid), name);
}

#[test]
fn test_tree_ref_cycle_rejected() {
    let mut conn = test_conn();
    let a = create(
        &mut conn,
        vec![Field::new("parent", Value::TreeRef { parent: None, name: "a".into() })],
    );
    let b = create(
        &mut conn,
        vec![Field::new("parent", Value::TreeRef { parent: Some(a.uuid), name: "b".into() })],
    );
    // Re-pointing a under b would create a cycle a → b → a.
    let mut w = Writer::begin(&mut conn, None).unwrap();
    let err = w
        .set_field(a.uuid, "parent", Value::TreeRef { parent: Some(b.uuid), name: "a".into() })
        .unwrap_err();
    assert!(err.to_string().contains("cycle"), "unexpected error: {err}");
}

#[test]
fn test_tree_ref_self_parent_rejected() {
    let mut conn = test_conn();
    let a = create(
        &mut conn,
        vec![Field::new("parent", Value::TreeRef { parent: None, name: "a".into() })],
    );
    let mut w = Writer::begin(&mut conn, None).unwrap();
    let err = w
        .set_field(a.uuid, "parent", Value::TreeRef { parent: Some(a.uuid), name: "a".into() })
        .unwrap_err();
    assert!(err.to_string().contains("cycle"), "unexpected error: {err}");
}

#[test]
fn test_tree_ref_depth_limit() {
    let mut conn = test_conn();
    // Build a chain of exactly 1000 nodes (depth 1000): root is depth 1.
    let mut w = Writer::begin(&mut conn, None).unwrap();
    let root = w
        .create_metarecord(vec![Field::new(
            "parent",
            Value::TreeRef { parent: None, name: "n1".into() },
        )])
        .unwrap();
    let mut prev = root.uuid;
    for i in 2..=1000 {
        let e = w
            .create_metarecord(vec![Field::new(
                "parent",
                Value::TreeRef { parent: Some(prev), name: format!("n{i}").into() },
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

// ── next_version allocator (spec-data-model) ────────────────────────────────

use metafolder_daemon::log;

mod common;
use common::TempDir;

/// The value a field write assigns, read back from the row.
fn set_field(conn: &mut Connection, uuid: Uuid, name: &str, v: Value) {
    let mut w = Writer::begin(conn, None).unwrap();
    w.set_field(uuid, name, v).unwrap();
    w.commit().unwrap();
}

#[test]
fn test_next_version_gaps_after_rollback() {
    // A version number is never reused for a different state: after rolling back
    // and writing again, the new write gets a fresh number, not the one the
    // rolled-back write had.
    let mut conn = test_conn();
    let m = create(&mut conn, vec![]);
    assert_eq!(db::get_version(&conn, m.uuid).unwrap(), Some(0));

    set_field(&mut conn, m.uuid, "a", Value::Int(1)); // -> version 1
    let head_v1 = log::get_head(&conn).unwrap();
    assert_eq!(db::get_version(&conn, m.uuid).unwrap(), Some(1));

    set_field(&mut conn, m.uuid, "a", Value::Int(2)); // -> version 2
    assert_eq!(db::get_version(&conn, m.uuid).unwrap(), Some(2));

    log::navigate(&mut conn, head_v1).unwrap(); // roll back to the version-1 state
    assert_eq!(db::get_version(&conn, m.uuid).unwrap(), Some(1));

    set_field(&mut conn, m.uuid, "a", Value::Int(3)); // fresh write must get 3, not 2
    assert_eq!(db::get_version(&conn, m.uuid).unwrap(), Some(3));
}

#[test]
fn test_entity_version_after_recorded() {
    // Every operation records the version it produced; on a linear chain it is
    // exactly entity_version_before + 1.
    let mut conn = test_conn();
    let m = create(&mut conn, vec![]);
    set_field(&mut conn, m.uuid, "a", Value::Int(1));

    let (before, after): (Option<i64>, Option<i64>) = conn
        .query_row(
            "SELECT entity_version_before, entity_version_after FROM operation \
             WHERE op_type = 'set_field'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(before, Some(0));
    assert_eq!(after, Some(1));
}

#[test]
fn test_redo_restores_stored_after_version_across_a_gap() {
    // After a rollback gap, a write's assigned version is not before + 1; redoing
    // that write must land on the exact number it assigned (entity_version_after),
    // not the recomputed before + 1.
    let mut conn = test_conn();
    let m = create(&mut conn, vec![]);
    set_field(&mut conn, m.uuid, "a", Value::Int(1)); // v1
    let head_v1 = log::get_head(&conn).unwrap();
    set_field(&mut conn, m.uuid, "a", Value::Int(2)); // v2, next_version now 3

    log::navigate(&mut conn, head_v1).unwrap(); // back to v1 (next_version still 3)
    set_field(&mut conn, m.uuid, "a", Value::Int(3)); // v3 (before=1, after=3 — a gap)
    let head_v3 = log::get_head(&conn).unwrap();
    assert_eq!(db::get_version(&conn, m.uuid).unwrap(), Some(3));

    log::navigate(&mut conn, head_v1).unwrap(); // roll the v3 write back
    assert_eq!(db::get_version(&conn, m.uuid).unwrap(), Some(1));
    log::navigate(&mut conn, head_v3).unwrap(); // redo it: must restore 3, not 2
    assert_eq!(db::get_version(&conn, m.uuid).unwrap(), Some(3));
}

#[test]
fn test_recreate_metarecord_recomputes_next_version() {
    // Navigating across a delete recreates the record; its next_version must
    // resume above every version it ever held, so a later fresh write cannot
    // reuse a number an earlier state already used.
    let mut conn = test_conn();
    let m = create(&mut conn, vec![]);
    set_field(&mut conn, m.uuid, "a", Value::Int(1)); // v1
    set_field(&mut conn, m.uuid, "a", Value::Int(2)); // v2, next_version now 3
    let head_v2 = log::get_head(&conn).unwrap();

    {
        let mut w = Writer::begin(&mut conn, None).unwrap();
        w.delete_metarecord(m.uuid).unwrap(); // record gone (allocator gone with it)
        w.commit().unwrap();
    }
    assert_eq!(db::get_version(&conn, m.uuid).unwrap(), None);

    log::navigate(&mut conn, head_v2).unwrap(); // undo the delete: record restored at v2
    assert_eq!(db::get_version(&conn, m.uuid).unwrap(), Some(2));

    set_field(&mut conn, m.uuid, "a", Value::Int(9)); // must get 3, not reuse 1
    assert_eq!(db::get_version(&conn, m.uuid).unwrap(), Some(3));
}

// ── Bounded log reading (efficient log listing for huge repos) ──────────────────

#[test]
fn test_ancestry_ops_limited_returns_the_most_recent() {
    use metafolder_daemon::log;
    // A linear chain of writes: create + four set_fields on one metarecord.
    let mut conn = test_conn();
    let m = create(&mut conn, vec![Field::new("s", Value::Int(0))]);
    for i in 1..=4 {
        set_field(&mut conn, m.uuid, "s", Value::Int(i));
    }
    let head = log::get_head(&conn).unwrap().unwrap();
    // `ancestry_ops` is HEAD-first (depth 0 = HEAD) up to the root.
    let full = log::ancestry_ops(&conn, head).unwrap();
    assert_eq!(full.len(), 5, "create + four sets");

    // Bounded to 2: exactly the two most recent (HEAD and its parent), HEAD-first
    // — the prefix of the full ancestry, walked without scanning the whole log.
    let limited = log::ancestry_ops_limited(&conn, head, 2).unwrap();
    assert_eq!(
        limited.iter().map(|o| o.id).collect::<Vec<_>>(),
        full.iter().take(2).map(|o| o.id).collect::<Vec<_>>(),
    );
    // A cap larger than the chain returns the whole ancestry.
    let big = log::ancestry_ops_limited(&conn, head, 999).unwrap();
    assert_eq!(
        big.iter().map(|o| o.id).collect::<Vec<_>>(),
        full.iter().map(|o| o.id).collect::<Vec<_>>()
    );
}

#[test]
fn test_ancestry_ops_until_stops_at_the_anchor() {
    use metafolder_daemon::log;
    // The index's forward delta: the operations a write appended on top of a
    // known anchor. Reading them must cost the delta, not the whole log — so the
    // walk stops at the anchor instead of running to the root.
    let mut conn = test_conn();
    let m = create(&mut conn, vec![Field::new("s", Value::Int(0))]);
    for i in 1..=4 {
        set_field(&mut conn, m.uuid, "s", Value::Int(i));
    }
    let head = log::get_head(&conn).unwrap().unwrap();
    let full = log::ancestry_ops(&conn, head).unwrap();
    assert_eq!(full.len(), 5, "create + four sets");
    let anchor = full[2].id; // two operations behind HEAD

    // HEAD-first, anchor excluded: exactly the two operations on top of it.
    let delta = log::ancestry_ops_until(&conn, head, anchor, 100).unwrap().unwrap();
    assert_eq!(
        delta.iter().map(|o| o.id).collect::<Vec<_>>(),
        full.iter().take(2).map(|o| o.id).collect::<Vec<_>>(),
    );
    // An anchor that *is* HEAD is an empty delta, not a miss.
    assert!(log::ancestry_ops_until(&conn, head, head, 100).unwrap().unwrap().is_empty());
    // Too far for the budget → None (the caller rebuilds instead).
    assert!(log::ancestry_ops_until(&conn, head, anchor, 1).unwrap().is_none());
    // An id that is not on the chain at all → None, whatever the budget.
    assert!(log::ancestry_ops_until(&conn, head, 999_999, 100).unwrap().is_none());
    // The root has no parent: walking past it must not loop or error.
    let root = full.last().unwrap().id;
    assert_eq!(
        log::ancestry_ops_until(&conn, head, root, 100).unwrap().unwrap().len(),
        full.len() - 1,
    );
}

#[test]
fn test_index_build_progress_tracks_field_ids() {
    use metafolder_daemon::index::RepoIndex;
    use std::cell::RefCell;
    // Progress is reported against MAX(field.id) (a determinate bar), not the
    // metarecord count — so it does not saturate at ~10% on a repo whose rows
    // outnumber its metarecords, and it ends exactly at 100%.
    let mut conn = test_conn();
    for i in 0..50 {
        create(
            &mut conn,
            vec![
                Field::new("a", Value::Int(i)),
                Field::new("b", Value::String(format!("x{i}"))),
                Field::new("c", Value::Bool(i % 2 == 0)),
            ],
        );
    }
    let max_id = db::max_field_id(&conn).unwrap() as u64;
    assert!(max_id >= 150, "50 records * 3 fields → at least 150 rows: {max_id}");

    let seen: RefCell<Vec<(u64, u64)>> = RefCell::new(Vec::new());
    RepoIndex::build_reported(&conn, &|done, total| seen.borrow_mut().push((done, total)), &|| {
        false
    })
    .unwrap();
    let seen = seen.into_inner();
    // Every sample uses MAX(id) as the total, and the final one is exactly full.
    assert!(seen.iter().all(|&(_, total)| total == max_id), "total is MAX(field.id): {seen:?}");
    assert_eq!(seen.last(), Some(&(max_id, max_id)), "ends at 100%");
}

#[test]
fn test_index_build_is_cancellable() {
    use metafolder_daemon::index::RepoIndex;
    // The heavy per-metarecord scan that builds the query index must honour a
    // cancellation probe (spec-tasks "Cancellation"), so a Stop on a query that
    // triggered a rebuild actually stops it.
    let mut conn = test_conn();
    create(&mut conn, vec![Field::new("a", Value::Int(1))]);
    assert!(
        RepoIndex::build_reported(&conn, &|_, _| {}, &|| true).is_err(),
        "a pre-cancelled index build must bail"
    );
    assert!(
        RepoIndex::build_reported(&conn, &|_, _| {}, &|| false).is_ok(),
        "a non-cancelled build succeeds"
    );
}

#[test]
fn test_assemble_selected_is_cancellable() {
    use metafolder_daemon::query_exec;
    // The select-projection loop (the dominant cost of `select=*` over many
    // matches) must honour the cancellation probe.
    let mut conn = test_conn();
    let m = create(&mut conn, vec![Field::new("a", Value::Int(1))]);
    assert!(
        query_exec::assemble_selected(&conn, &[m.uuid], None, &|| true).is_err(),
        "a pre-cancelled assembly must bail"
    );
    let objects = query_exec::assemble_selected(&conn, &[m.uuid], None, &|| false).unwrap();
    assert_eq!(objects.len(), 1);
}

#[test]
fn test_has_children_reflects_forward_ops() {
    use metafolder_daemon::log;
    let mut conn = test_conn();
    let m = create(&mut conn, vec![Field::new("s", Value::Int(0))]);
    set_field(&mut conn, m.uuid, "s", Value::Int(1));
    let head = log::get_head(&conn).unwrap().unwrap();
    // HEAD is the tip of the chain: nothing points at it as a parent.
    assert!(!log::has_children(&conn, head).unwrap());
    // Its parent does have a forward child (HEAD).
    let parent = log::get_op(&conn, head).unwrap().unwrap().parent_id.unwrap();
    assert!(log::has_children(&conn, parent).unwrap());
}
