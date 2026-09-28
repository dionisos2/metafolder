//! The SQLite layer's own tests: its schema, its query plans, its migrations
//! and its lock. Temporary — they go with the SQLite backend (roadmap "drop
//! SQLite", step 3c); the storage behaviour every backend shares is tested in
//! `storage.rs`, on the key-value store.
#![allow(dead_code, unused_imports)]

use metafolder_core::metarecord::{Field, TreeName, Value};
use metafolder_core::order;
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

// ── Value encoding roundtrip through the field table ──────────────────────────

// ── Writer: create ────────────────────────────────────────────────────────────

// ── Writer: set_field ─────────────────────────────────────────────────────────

// ── Writer: append / replace / delete field ───────────────────────────────────

// ── Writer: delete entry ──────────────────────────────────────────────────────

// ── Writer: revision grouping and HEAD chain ──────────────────────────────────

#[test]
fn test_prune_reclaims_disk_space() {
    use metafolder_daemon::log::{self, PruneMode};

    let dir = TempDir::new("prune-vacuum");
    let path = dir.join("db.sqlite");
    let mut conn = db::open_database(&path, "test").unwrap();
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

// ── TreeRef validation ────────────────────────────────────────────────────────

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
    let mut conn = db::open_database(&path, "test").unwrap();
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
    let mut conn = db::open_database(&path, "test").unwrap();

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
    let mut conn = db::open_database(&path, "test").unwrap();
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
    let conn = db::open_database(&path, "test").unwrap();
    assert_eq!(tree_name(&conn, file.uuid), name);
}

#[test]
fn test_an_existing_repository_gains_the_reverts_index_on_open() {
    let dir = common::TempDir::new("migrate-reverts-index");
    let path = dir.path().join("db.sqlite");
    let mut conn = db::open_database(&path, "test").unwrap();
    db::init_schema(&conn).unwrap();
    create(&mut conn, vec![Field::new("kind", Value::String("file".into()))]);

    // A repository written before `operation.reverts_op_id` was indexed. It is
    // the deletes that need it: the column is a foreign key back into
    // `operation`, so without the index every delete scans the whole log to
    // check nothing still points at the row — the retention trim and
    // `mf log prune` then cost O(deleted x log).
    conn.execute_batch("DROP INDEX idx_operation_reverts;").unwrap();
    assert!(query_plan(&conn, "DELETE FROM operation WHERE id = 1").contains("SCAN operation"));
    drop(conn);

    // Opening it back-fills the index.
    let conn = db::open_database(&path, "test").unwrap();
    let plan = query_plan(&conn, "DELETE FROM operation WHERE id = 1");
    assert!(
        plan.contains("idx_operation_reverts"),
        "the foreign key is still unindexed after the migration: {plan}"
    );
    assert!(!plan.contains("SCAN operation"), "deleting an operation still scans the log: {plan}");
}

// ── Reserved fields ───────────────────────────────────────────────────────────

// ── OpType ────────────────────────────────────────────────────────────────────

// ── The version is a content hash (spec-data-model "Version") ───────────────

use metafolder_daemon::log;
use metafolder_daemon::version;

mod common;
use common::TempDir;

/// The value a field write assigns, read back from the row.
fn set_field(conn: &mut Connection, uuid: Uuid, name: &str, v: Value) {
    let mut w = Writer::begin(conn, None).unwrap();
    w.set_field(uuid, name, v).unwrap();
    w.commit().unwrap();
}

/// The version the metarecord's current rows dictate, computed from outside the
/// write path. Everything below compares what the daemon *stored* against what
/// the content *says* — two routes to the same answer.
fn version_of_content(conn: &Connection, uuid: Uuid) -> u64 {
    version::of_rows(uuid, &db::get_field_rows(conn, uuid).unwrap())
}

#[test]
fn test_migration_converts_counter_versions_and_empties_the_log() {
    let mut conn = test_conn();
    let m = create(&mut conn, vec![Field::new("a", Value::Int(1))]);
    let empty = create(&mut conn, vec![]);
    set_field(&mut conn, m.uuid, "b", Value::String("x".into()));

    // Put the database back in its pre-migration shape: the allocator column,
    // counter versions, and a log the migration has to discard.
    conn.execute_batch(
        "ALTER TABLE metarecord ADD COLUMN next_version INTEGER NOT NULL DEFAULT 1;
         UPDATE metarecord SET version = 2, next_version = 3;
         DELETE FROM migration_state WHERE name = 'version-is-a-content-hash';",
    )
    .unwrap();
    assert_eq!(db::get_version(&conn, m.uuid).unwrap(), Some(2));
    assert!(count(&conn, "SELECT COUNT(*) FROM operation") > 0, "there is a log to discard");

    db::migrate_version_to_content_hash(&conn).unwrap();

    // Every version now describes the content it sits on — including a
    // metarecord with no field at all, which gets its base term.
    assert_eq!(db::get_version(&conn, m.uuid).unwrap(), Some(version_of_content(&conn, m.uuid)));
    assert_eq!(db::get_version(&conn, empty.uuid).unwrap(), Some(version::base(empty.uuid)));

    // The log is gone: its version columns hold counters, and a navigation step
    // replaying one would leave a metarecord describing some other state.
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM operation"), 0);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM revision"), 0);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM op_snapshot"), 0);
    let head: Option<i64> =
        conn.query_row("SELECT op_id FROM log_head WHERE singleton = 1", [], |r| r.get(0)).unwrap();
    assert_eq!(head, None, "HEAD is back to the empty state");

    // And the allocator itself is gone.
    assert!(conn.prepare("SELECT next_version FROM metarecord").is_err());
}

// ── Bounded log reading (efficient log listing for huge repos) ──────────────────

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

// ── One daemon per repository ─────────────────────────────────────────────────

#[test]
fn test_a_second_open_is_refused_immediately_and_names_the_other_daemon() {
    // The exclusive lock is the "one daemon per repository" invariant. A second
    // daemon must learn that *at once*: rusqlite installs a 5 s busy handler, so
    // a lock contention that is not disarmed turns every locked repository into
    // seconds of silent sleeping at startup — and the message that finally comes
    // out must name the cause, not the pragma that happened to hit the lock.
    let dir = TempDir::new("second-daemon");
    let path = dir.join("db.sqlite");
    let held = db::open_database(&path, "test").unwrap();
    db::init_schema(&held).unwrap();

    let start = std::time::Instant::now();
    let err = db::open_database(&path, "test").unwrap_err();
    let elapsed = start.elapsed();

    assert!(elapsed < std::time::Duration::from_secs(2), "the second open slept for {elapsed:?}");
    let message = format!("{err:#}");
    assert!(
        message.contains("another metafolder daemon"),
        "the lock must be reported as what it is, got: {message}"
    );
}

/// Opening a repository twice must not touch the schema the second time.
///
/// `PRAGMA schema_version` counts DDL statements, so it is the exact witness of
/// a migration that re-does its work on a database that no longer needs it —
/// the kind that costs nothing on a test database and minutes of full CPU on a
/// real one, with nothing on stderr to say what is running.
#[test]
fn test_reopening_a_migrated_database_runs_no_ddl() {
    let dir = TempDir::new("steady-state-open");
    let path = dir.join("db.sqlite");
    let conn = db::open_database(&path, "test").unwrap();
    db::init_schema(&conn).unwrap();
    drop(conn);

    let schema_version = |conn: &Connection| -> i64 {
        conn.pragma_query_value(None, "schema_version", |r| r.get(0)).unwrap()
    };
    // The first reopen may still complete a migration; the second one must be
    // pure, and so must every one after it.
    let conn = db::open_database(&path, "test").unwrap();
    let settled = schema_version(&conn);
    drop(conn);
    let conn = db::open_database(&path, "test").unwrap();
    assert_eq!(
        schema_version(&conn),
        settled,
        "opening an already-migrated database rewrote a schema object"
    );
}

// ── Join plans (correlated reads must seek, not scan) ─────────────────────────

/// The three whole-repository reads join `field` to itself on
/// `metarecord_uuid`, and each one is a single planner decision away from being
/// quadratic.
///
/// `idx_field_name_type(field_name, value_type)` and
/// `idx_field_metarecord(metarecord_uuid, field_name)` both offer two equality
/// columns, so with no statistics SQLite is free to pick either — and it picks
/// the first, which does not carry `metarecord_uuid`. The correlated side then
/// *scans every row of that field name in the repository* to find the one it
/// wants: on a repository with 500 000 orphans the orphan read did not finish
/// in ten minutes, against half a second once the join seeks. A `CROSS JOIN`
/// pins the join order but says nothing about the index, which is why each of
/// these carries `INDEXED BY`.
#[test]
fn test_a_groups_members_are_a_reverse_index_lookup() {
    // "Who points at this group?" is asked once per departing member
    // (spec-duplicates "Leaving a group"), so it must seek `idx_field_reverse`
    // and not walk every `mfr_duplicate_group` row in the repository. SQLite
    // only uses that *partial* index when the query's WHERE implies its
    // predicate, which `value_type = 'ref'` alone does not.
    let conn = test_conn();
    // This one takes a parameter, so it cannot go through `query_plan`.
    let sql = format!("EXPLAIN QUERY PLAN {}", db::DUPLICATE_GROUP_MEMBERS_SQL);
    let mut stmt = conn.prepare(&sql).unwrap();
    let plan: String = stmt
        .query_map(rusqlite::params![vec![0u8; 16]], |r| r.get::<_, String>(3))
        .unwrap()
        .collect::<Result<Vec<String>, _>>()
        .unwrap()
        .join(" | ");
    assert!(plan.contains("idx_field_reverse"), "the reverse index is not used: {plan}");
    assert_eq!(plan.matches("SCAN").count(), 0, "a scan crept in: {plan}");
}

#[test]
fn test_the_repository_wide_reads_seek_their_correlated_rows() {
    let conn = test_conn();
    for (what, sql) in [
        ("hashed orphans", db::HASHED_ORPHANS_SQL),
        ("tracked files with size", db::TRACKED_FILES_WITH_SIZE_SQL),
        ("duplicate groups", db::DUPLICATE_GROUPS_SQL),
    ] {
        let plan = query_plan(&conn, sql);
        // The driving table is free to use whichever index suits it; *every*
        // other line is a correlated lookup and must go through the index keyed
        // by the uuid. Counting them is the point — leaving one join unguarded
        // is enough to make the whole read quadratic.
        let lines = plan.split('|').filter(|l| !l.trim().is_empty()).count();
        let correlated = plan.matches("idx_field_metarecord").count();
        assert_eq!(
            correlated,
            lines - 1,
            "{what}: {} of {} correlated joins seek by metarecord_uuid: {plan}",
            correlated,
            lines - 1
        );
        assert_eq!(plan.matches("SCAN").count(), 0, "{what}: a scan crept in: {plan}");
    }
}

// ── What a revision obliges its caller to refresh ─────────────────────────────

/// A `Writer` that has recorded `n` operations touching nothing of interest.
fn pad_operations(w: &mut Writer, uuid: Uuid, n: usize) {
    for i in 0..n {
        w.set_field(uuid, "pad", Value::Int(i as i64)).unwrap();
    }
}

/// A `tree_ref` value, spelled once for the tests above.
fn tree_ref(parent: Option<Uuid>, name: &str) -> Value {
    Value::TreeRef { parent, name: name.into() }
}

#[test]
fn test_the_forest_scan_seeks_the_tree_index() {
    // The tree cache is rebuilt from this scan — at load, and again whenever an
    // incremental settle declines. Left to choose, SQLite walks the whole EAV
    // table to keep the one row in ten that is a tree_ref; only the partial
    // index holds exactly those.
    let conn = test_conn();
    let plan = query_plan(&conn, db::FOREST_SQL);
    assert!(plan.contains("idx_field_tree"), "the forest scan should seek the tree index: {plan}");
    assert!(!plan.contains("idx_field_name "), "it must not walk the whole field table: {plan}");
}

// ── The forest's referential integrity, on removal ───────────────────────────
//
// A TreeRef reference is validated when it is *created* (the parent must carry a
// position in the same forest). Nothing used to validate the other side: taking
// the parent's position away left its children naming a node that no longer
// existed — "detached" — reachable by uuid but under no parent and in no roots
// map, so `tree/roots` and path reconstruction disagreed about them. The check
// is now symmetric: a position with children cannot be removed.

/// The `p` forest: a root and one child under it. Returns `(root, child)`.
fn forest_pair(conn: &mut Connection) -> (Uuid, Uuid) {
    let root = create(conn, vec![Field::new("p", tree_ref(None, "animals"))]).uuid;
    let child = create(conn, vec![Field::new("p", tree_ref(Some(root), "cat"))]).uuid;
    (root, child)
}

/// The error a committed revision refuses with, or a panic if it committed.
fn refused(conn: &mut Connection, write: impl FnOnce(&mut Writer)) -> String {
    let mut w = Writer::begin(conn, None).unwrap();
    write(&mut w);
    w.commit().expect_err("the revision should have been refused").to_string()
}

#[test]
fn test_the_batched_cell_read_seeks_by_metarecord() {
    // The read the tree cache settles a revision from. Its correlated column is
    // `metarecord_uuid`, and left to choose SQLite may take
    // `idx_field_name_type` — which does not carry the uuid — and scan a whole
    // field name per chunk (spec-main "Key invariants").
    let conn = test_conn();
    let plan = query_plan(
        &conn,
        "SELECT metarecord_uuid, field_name, value_uuid, value_name, value_name_bytes \
         FROM field INDEXED BY idx_field_metarecord \
         WHERE metarecord_uuid IN (x'00', x'01') AND value_type = 'tree_ref' \
         ORDER BY metarecord_uuid, field_name, id",
    );
    assert!(plan.contains("idx_field_metarecord"), "should seek by metarecord: {plan}");
    assert!(!plan.contains("SCAN field"), "should not scan the field table: {plan}");
}

// ── The order_position_* → order_* rename (spec-file-tracking "mf order") ─────

/// The field names a repository written before the rename carries.
const LEGACY_FILE: &str = "order_position_file";
const LEGACY_DIR: &str = "order_position_dir";

/// How many rows of `table` carry `name` in their `field_name` column.
fn named(conn: &Connection, table: &str, name: &str) -> i64 {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table} WHERE field_name = ?1"), [name], |r| {
        r.get(0)
    })
    .unwrap()
}

/// Renames the two order fields back to the names they had before the rename,
/// in the data *and* in the log — what a repository written by an older daemon
/// looks like on disk.
fn downgrade_to_order_position_names(conn: &Connection) {
    conn.execute_batch(
        "UPDATE field SET field_name = 'order_position_file' WHERE field_name = 'order_file';
         UPDATE field SET field_name = 'order_position_dir' WHERE field_name = 'order_dir';
         UPDATE operation SET field_name = 'order_position_file' WHERE field_name = 'order_file';
         UPDATE operation SET field_name = 'order_position_dir' WHERE field_name = 'order_dir';
         UPDATE op_snapshot SET field_name = 'order_position_file' WHERE field_name = 'order_file';
         UPDATE op_snapshot SET field_name = 'order_position_dir' WHERE field_name = 'order_dir';",
    )
    .unwrap();
}

/// Writes one numbered file and one numbered directory, through the log. The
/// positions are written with `set_field` rather than at creation, so the name
/// lands in `operation.field_name` too (a `create_metarecord` operation names no
/// field — its snapshot rows carry the names) and the test can watch all three
/// tables.
fn numbered_children(conn: &mut Connection) -> (Uuid, Uuid) {
    let file = create(conn, vec![]);
    let dir = create(conn, vec![]);
    let mut w = Writer::begin(conn, None).unwrap();
    w.set_field(file.uuid, order::FIELD_FILE, Value::Int(1)).unwrap();
    w.set_field(dir.uuid, order::FIELD_DIR, Value::Int(1)).unwrap();
    w.commit().unwrap();
    (file.uuid, dir.uuid)
}

#[test]
fn test_order_position_fields_are_renamed_on_open() {
    let dir_ = common::TempDir::new("migrate-order-names");
    let path = dir_.path().join("db.sqlite");

    // A repository written before the rename: the positions, and the log
    // operations that wrote them, all speak the old names.
    let mut conn = db::open_database(&path, "test").unwrap();
    db::init_schema(&conn).unwrap();
    let (file, dir) = numbered_children(&mut conn);
    downgrade_to_order_position_names(&conn);
    assert_eq!(named(&conn, "field", LEGACY_FILE), 1, "the fixture is a legacy database");
    drop(conn);

    // Opening it renames both fields, in the data and in the log — a rollback
    // across one of these revisions must not resurrect the old name.
    let conn = db::open_database(&path, "test").unwrap();
    for table in ["field", "operation", "op_snapshot"] {
        assert_eq!(named(&conn, table, LEGACY_FILE), 0, "{table} still carries {LEGACY_FILE}");
        assert_eq!(named(&conn, table, LEGACY_DIR), 0, "{table} still carries {LEGACY_DIR}");
        assert!(named(&conn, table, order::FIELD_FILE) > 0, "{table} lost the file position");
        assert!(named(&conn, table, order::FIELD_DIR) > 0, "{table} lost the dir position");
    }

    // The values ride along with the names.
    let pos: i64 = conn
        .query_row(
            "SELECT value_int FROM field WHERE metarecord_uuid = ?1 AND field_name = ?2",
            rusqlite::params![file.as_bytes().as_slice(), order::FIELD_FILE],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(pos, 1);
    assert_eq!(named(&conn, "field", order::FIELD_DIR), 1, "the directory keeps its position");
    let _ = dir;
}

#[test]
fn test_renaming_the_order_fields_twice_is_a_no_op() {
    let dir_ = common::TempDir::new("migrate-order-names-idempotent");
    let path = dir_.path().join("db.sqlite");
    let mut conn = db::open_database(&path, "test").unwrap();
    db::init_schema(&conn).unwrap();
    let (file, _) = numbered_children(&mut conn);
    downgrade_to_order_position_names(&conn);
    drop(conn);

    // The first open migrates; the second must find nothing to do and leave the
    // rows exactly as they are.
    let conn = db::open_database(&path, "test").unwrap();
    let id: i64 = conn
        .query_row(
            "SELECT id FROM field WHERE metarecord_uuid = ?1 AND field_name = ?2",
            rusqlite::params![file.as_bytes().as_slice(), order::FIELD_FILE],
            |r| r.get(0),
        )
        .unwrap();
    drop(conn);

    let conn = db::open_database(&path, "test").unwrap();
    assert_eq!(named(&conn, "field", order::FIELD_FILE), 1, "no row was duplicated");
    let again: i64 = conn
        .query_row(
            "SELECT id FROM field WHERE metarecord_uuid = ?1 AND field_name = ?2",
            rusqlite::params![file.as_bytes().as_slice(), order::FIELD_FILE],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(again, id, "the field row keeps its id");
}

#[test]
fn test_the_order_rename_leaves_a_repository_that_already_uses_the_new_name_alone() {
    let dir_ = common::TempDir::new("migrate-order-names-collision");
    let path = dir_.path().join("db.sqlite");

    // A repository where BOTH names exist: the user has a field of their own
    // called `order_file`. Renaming into it would silently merge two different
    // fields on the same metarecord, so the migration must decline.
    let mut conn = db::open_database(&path, "test").unwrap();
    db::init_schema(&conn).unwrap();
    let m = create(
        &mut conn,
        vec![
            Field::new(order::FIELD_FILE, Value::Int(7)),
            Field::new(order::FIELD_DIR, Value::Int(9)),
        ],
    );
    // Downgrade only one of them, so the database holds `order_position_file`
    // (legacy) next to `order_file` (the user's own).
    conn.execute(
        "UPDATE field SET field_name = ?1 WHERE metarecord_uuid = ?2 AND field_name = ?3",
        rusqlite::params![LEGACY_DIR, m.uuid.as_bytes().as_slice(), order::FIELD_DIR],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO field (metarecord_uuid, field_name, value_type, value_int) \
         VALUES (?1, ?2, 'int', 3)",
        rusqlite::params![m.uuid.as_bytes().as_slice(), LEGACY_FILE],
    )
    .unwrap();
    drop(conn);

    let conn = db::open_database(&path, "test").unwrap();
    // The colliding name is left as it is — nothing is merged, nothing is lost.
    assert_eq!(named(&conn, "field", LEGACY_FILE), 1, "the colliding rename is declined");
    assert_eq!(named(&conn, "field", order::FIELD_FILE), 1, "the user's own field is untouched");
    // The name that does not collide still migrates.
    assert_eq!(named(&conn, "field", LEGACY_DIR), 0, "the free rename still happens");
    assert_eq!(named(&conn, "field", order::FIELD_DIR), 1);
}

// ── No duplicate rows (spec-data-model "No duplicate rows") ───────────────────

fn s(v: &str) -> Value {
    Value::String(v.to_string())
}

/// The values of `name` on `uuid`, in row order.
fn values_of(conn: &Connection, uuid: Uuid, name: &str) -> Vec<Value> {
    db::get_field_rows_named(conn, uuid, name).unwrap().into_iter().map(|r| r.value).collect()
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

/// Simulates a repository written before the no-duplicate rule: the marker the
/// migration leaves behind is removed, and a duplicate row is inserted by hand.
fn downgrade_to_duplicates_allowed(conn: &Connection, uuid: Uuid, name: &str) {
    conn.execute("DELETE FROM migration_state WHERE name = 'dedup-field-rows'", []).unwrap();
    conn.execute(
        "INSERT INTO field (metarecord_uuid, field_name, value_type, value_text)
         SELECT metarecord_uuid, field_name, value_type, value_text FROM field
          WHERE metarecord_uuid = ?1 AND field_name = ?2",
        rusqlite::params![uuid.as_bytes().as_slice(), name],
    )
    .unwrap();
}

#[test]
fn test_an_existing_repository_drops_duplicate_rows_on_open() {
    let dir = common::TempDir::new("migrate-duplicate-rows");
    let path = dir.path().join("db.sqlite");

    let mut conn = db::open_database(&path, "test").unwrap();
    db::init_schema(&conn).unwrap();
    let m = create(&mut conn, vec![Field::new("tag", s("jazz")), Field::new("tag", s("live"))]);
    let kept: Vec<i64> =
        db::get_field_rows_named(&conn, m.uuid, "tag").unwrap().into_iter().map(|r| r.id).collect();
    downgrade_to_duplicates_allowed(&conn, m.uuid, "tag");
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM field WHERE field_name = 'tag'"), 4);
    drop(conn);

    // Opening it deduplicates, keeping the lowest id of each group.
    let conn = db::open_database(&path, "test").unwrap();
    let rows: Vec<i64> =
        db::get_field_rows_named(&conn, m.uuid, "tag").unwrap().into_iter().map(|r| r.id).collect();
    assert_eq!(rows, kept, "the first row of each value survives");
    assert_eq!(values_of(&conn, m.uuid, "tag"), vec![s("jazz"), s("live")]);
    // ...and the pass records itself, so no later open re-scans `field`.
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM migration_state WHERE name = 'dedup-field-rows'"),
        1
    );
}

#[test]
fn test_an_existing_repository_drops_the_legacy_fts_index_on_open() {
    // `field_text` was a trigram FTS5 index maintained on every field write, to
    // pre-filter the SQL engine's REGEXP scan. No REGEXP scan runs any more —
    // text predicates are answered from the index's in-memory value partition
    // (spec-indexing "No operand runs in SQL") — so the table is dead weight
    // whose upkeep is on the write path. Opening a database that still carries
    // it drops it, once.
    let dir = common::TempDir::new("migrate-drop-fts");
    let path = dir.path().join("db.sqlite");

    let mut conn = db::open_database(&path, "test").unwrap();
    db::init_schema(&conn).unwrap();
    let m = create(&mut conn, vec![Field::new("tag", s("jazz"))]);
    // Re-create the legacy index by hand, as a database written before this
    // carries it.
    conn.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS field_text USING fts5(
             text, content='', contentless_delete=1, tokenize='trigram');
         INSERT OR REPLACE INTO field_text(rowid, text) VALUES (1, 'jazz');",
    )
    .unwrap();
    assert!(table_exists(&conn, "field_text"), "the fixture must carry the legacy index");
    drop(conn);

    let mut conn = db::open_database(&path, "test").unwrap();
    assert!(!table_exists(&conn, "field_text"), "opening must drop it");
    // The data it indexed is untouched, and writing still works without it.
    assert_eq!(values_of(&conn, m.uuid, "tag"), vec![s("jazz")]);
    let m2 = create(&mut conn, vec![Field::new("tag", s("live"))]);
    assert_eq!(values_of(&conn, m2.uuid, "tag"), vec![s("live")]);
    assert!(!table_exists(&conn, "field_text"), "and no write re-creates it");
}

/// Whether the database carries `table`.
fn table_exists(conn: &rusqlite::Connection, table: &str) -> bool {
    count(
        conn,
        &format!("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = '{table}'"),
    ) > 0
}

#[test]
fn test_a_fresh_repository_is_marked_deduplicated_without_a_scan() {
    let dir = common::TempDir::new("fresh-duplicate-marker");
    let path = dir.path().join("db.sqlite");
    let conn = db::open_database(&path, "test").unwrap();
    db::init_schema(&conn).unwrap();
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM migration_state WHERE name = 'dedup-field-rows'"),
        1,
        "a database that cannot hold duplicates is marked done straight away"
    );
}
