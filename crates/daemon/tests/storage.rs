//! Integration tests for the storage layer: value encoding, the logged write
//! flow (Writer), TreeRef validation, reserved fields.

use metafolder_core::metarecord::{Field, TreeName, Value};
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::{OpRow, OpType, Writer};
use metafolder_daemon::reserved;
use metafolder_daemon::store::{Begin, Log, Rows};
use uuid::Uuid;

fn test_conn() -> (KvStore, common::TempDir) {
    common::kv::store()
}

/// Creates an entry through a single-use Writer and returns it.
fn create(conn: &mut KvStore, fields: Vec<Field>) -> metafolder_core::metarecord::MetaRecord {
    let mut w = Writer::begin(conn, None).unwrap();
    let m = w.create_metarecord(fields).unwrap();
    w.commit().unwrap();
    m
}

/// The operations from `from` back to `until` (excluded), `None` when
/// `until` is not an ancestor within `max` steps.
fn ops_until(conn: &KvStore, from: i64, until: i64, max: usize) -> Option<Vec<OpRow>> {
    match conn.ops_until(from, until, max).unwrap() {
        metafolder_daemon::log::Delta::Found(ops) => Some(ops),
        _ => None,
    }
}

/// The operations of one type, in id order.
fn ops_of_type(conn: &KvStore, op_type: &str) -> Vec<OpRow> {
    let mut ops: Vec<OpRow> =
        conn.all_ops().unwrap().into_iter().filter(|o| o.op_type == op_type).collect();
    ops.sort_by_key(|o| o.id);
    ops
}

// ── One value type per field name (invariant) ──────────────────────────────────

#[test]
fn test_field_first_write_establishes_type() {
    // The first non-Nothing write of a name succeeds and fixes its type.
    let (mut conn, _dir) = test_conn();
    let m = create(&mut conn, vec![Field::new("rating", Value::Int(5))]);
    let got = conn.metarecord(m.uuid).unwrap().unwrap();
    assert_eq!(got.get("rating"), Some(&Value::Int(5)));
}

#[test]
fn test_field_rejects_conflicting_value_type() {
    // Once `rating` is an Int repo-wide, a String write to it is rejected (400).
    let (mut conn, _dir) = test_conn();
    create(&mut conn, vec![Field::new("rating", Value::Int(5))]);

    let mut w = Writer::begin(&mut conn, None).unwrap();
    let err = w.set_field(Uuid::new_v4(), "rating", Value::String("five".into())).unwrap_err();
    assert!(err.to_string().contains("type"), "expected a type-conflict error, got: {err}");
}

#[test]
fn test_field_rejects_conflicting_type_within_one_create() {
    // Two rows of the same name with different types in a single create are rejected.
    let (mut conn, _dir) = test_conn();
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
    let (mut conn, _dir) = test_conn();
    let m = create(&mut conn, vec![Field::new("rating", Value::Int(5))]);

    let mut w = Writer::begin(&mut conn, None).unwrap();
    w.set_field(m.uuid, "rating", Value::Nothing).unwrap();
    w.commit().unwrap();
}

#[test]
fn test_field_type_unlocks_when_empty() {
    // With no non-Nothing rows left, the name's type is unestablished again and a
    // new (different) type may be written.
    let (mut conn, _dir) = test_conn();
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
    let (mut conn, _dir) = test_conn();
    let m = create(&mut conn, vec![Field::new("note", Value::Int(1))]);

    let mut w = Writer::begin(&mut conn, None).unwrap();
    w.set_field(m.uuid, "note", Value::Int(2)).unwrap(); // caches "int"
    w.set_field(m.uuid, "note", Value::Nothing).unwrap(); // clears → must drop cache
    w.set_field(m.uuid, "note", Value::String("text".into())).unwrap(); // now allowed
    w.commit().unwrap();

    let g = conn.metarecord(m.uuid).unwrap().unwrap();
    assert_eq!(g.get("note"), Some(&Value::String("text".into())));
}

#[test]
fn test_retype_field_converts_rolls_back_and_relocks() {
    use metafolder_core::metarecord::FieldType;
    let (mut conn, _dir) = test_conn();
    let m1 = create(&mut conn, vec![Field::new("rating", Value::Int(3))]);
    // Nothing coexists and must survive the retype untouched.
    let m2 = create(
        &mut conn,
        vec![Field::new("rating", Value::Int(5)), Field::new("rating", Value::Nothing)],
    );

    let head_before = conn.head().unwrap();

    let mut w = Writer::begin(&mut conn, None).unwrap();
    let summary = w.retype_field("rating", FieldType::String).unwrap();
    w.commit().unwrap();
    assert_eq!(summary.converted, 2, "both Int rows convert; the Nothing row is skipped");
    assert!(summary.fallback_uuids.is_empty(), "Int→String never falls back");

    let g1 = conn.metarecord(m1.uuid).unwrap().unwrap();
    assert_eq!(g1.get("rating"), Some(&Value::String("3".into())));
    let g2 = conn.metarecord(m2.uuid).unwrap().unwrap();
    assert!(g2.get_all("rating").contains(&&Value::String("5".into())));
    assert!(g2.get_all("rating").contains(&&Value::Nothing), "Nothing preserved");

    // The field is now String repo-wide: a conflicting Int write is rejected.
    let mut w = Writer::begin(&mut conn, None).unwrap();
    assert!(w.set_field(m1.uuid, "rating", Value::Int(9)).is_err());
    drop(w);

    // Rollback to before the retype restores the original Int values exactly.
    metafolder_daemon::log::navigate(&mut conn, head_before).unwrap();
    let g1 = conn.metarecord(m1.uuid).unwrap().unwrap();
    assert_eq!(g1.get("rating"), Some(&Value::Int(3)));
}

#[test]
fn test_retype_field_records_fallbacks() {
    use metafolder_core::metarecord::FieldType;
    let (mut conn, _dir) = test_conn();
    let good = create(&mut conn, vec![Field::new("code", Value::String("42".into()))]);
    let bad = create(&mut conn, vec![Field::new("code", Value::String("oops".into()))]);

    let mut w = Writer::begin(&mut conn, None).unwrap();
    let summary = w.retype_field("code", FieldType::Int).unwrap();
    w.commit().unwrap();

    assert_eq!(summary.converted, 2);
    assert_eq!(summary.fallback_uuids, vec![bad.uuid], "only the un-parsable value fell back");
    let g = conn.metarecord(good.uuid).unwrap().unwrap();
    assert_eq!(g.get("code"), Some(&Value::Int(42)));
    let b = conn.metarecord(bad.uuid).unwrap().unwrap();
    assert_eq!(b.get("code"), Some(&Value::Int(0)), "un-parsable → sentinel 0");
}

#[test]
fn test_retype_string_to_reference_types() {
    use metafolder_core::metarecord::FieldType;
    use uuid::Uuid;
    let (mut conn, _dir) = test_conn();
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
    assert_eq!(conn.metarecord(good.uuid).unwrap().unwrap().get("link"), Some(&Value::Ref(target)));
    assert_eq!(conn.metarecord(bad.uuid).unwrap().unwrap().get("link"), Some(&Value::Nothing));
}

#[test]
fn test_retype_string_to_tree_ref_validates_forest() {
    use metafolder_core::metarecord::FieldType;
    let (mut conn, _dir) = test_conn();
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
        conn.metarecord(root.uuid).unwrap().unwrap().get("cat"),
        Some(&Value::TreeRef { parent: None, name: "tags".into() })
    );
    assert_eq!(conn.metarecord(orphan.uuid).unwrap().unwrap().get("cat"), Some(&Value::Nothing));
}

#[test]
fn test_log_head_starts_null() {
    let (conn, _dir) = test_conn();
    assert_eq!(conn.head().unwrap(), None);
}

#[test]
fn test_tree_unique_index_rejects_duplicate_position() {
    let (mut conn, _dir) = test_conn();
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
    // An occupied position is refused with the clean domain message (a 400),
    // not an internal error.
    assert!(
        err.to_string().to_lowercase().contains("occupied"),
        "expected the mapped 'tree position already occupied' error, got: {err}"
    );
}

#[test]
fn test_mfr_path_is_single_valued() {
    let (mut conn, _dir) = test_conn();
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

    // Appending a second mfr_path is rejected — a metarecord tracks one path
    // (the Writer's rule for every forest, spec-data-model "One position per
    // forest"; the storage layer holds `mfr_path` to it on its own too).
    let mut w = Writer::begin(&mut conn, None).unwrap();
    let err = w
        .append_field(
            file.uuid,
            "mfr_path",
            Value::TreeRef { parent: Some(root.uuid), name: "b.mp3".into() },
        )
        .unwrap_err();
    assert!(err.to_string().contains("one position per forest"), "got: {err}");
    drop(w);

    // Creating a metarecord with two mfr_path fields is likewise rejected.
    let mut w = Writer::begin(&mut conn, None).unwrap();
    let err = w
        .create_metarecord(vec![
            Field::new("mfr_path", Value::TreeRef { parent: Some(root.uuid), name: "x".into() }),
            Field::new("mfr_path", Value::TreeRef { parent: Some(root.uuid), name: "y".into() }),
        ])
        .unwrap_err();
    assert!(err.to_string().contains("one position per forest"), "got: {err}");
}

// ── Value encoding roundtrip through the field table ──────────────────────────

#[test]
fn test_all_value_types_roundtrip() {
    let (mut conn, _dir) = test_conn();
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

    let got = conn.metarecord(created.uuid).unwrap().expect("entry must exist");
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
    let (conn, _dir) = test_conn();
    assert!(conn.metarecord(Uuid::new_v4()).unwrap().is_none());
}

#[test]
fn test_list_records_sorts_by_uuid() {
    let (mut conn, _dir) = test_conn();
    let e1 = create(&mut conn, vec![]);
    let e2 = create(&mut conn, vec![]);
    let e3 = create(&mut conn, vec![]);

    let got = conn.metarecords().unwrap();
    let mut expected = vec![e1.uuid, e2.uuid, e3.uuid];
    expected.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    assert_eq!(got, expected);
}

// ── Writer: create ────────────────────────────────────────────────────────────

#[test]
fn test_create_record_initial_state() {
    let (mut conn, _dir) = test_conn();
    let m = create(&mut conn, vec![Field::new("rating", Value::Int(5))]);
    assert_eq!(Some(m.version), conn.version(m.uuid).unwrap());
    assert_eq!(m.version, version_of_content(&conn, m.uuid), "the create reports what it stored");
    assert_ne!(m.version, version::base(m.uuid), "and the field it carries is in there");
    assert_eq!(m.fields.len(), 1);
    assert!(m.fields[0].id.is_some());
}

#[test]
fn test_create_record_writes_log() {
    let (mut conn, _dir) = test_conn();
    let m = create(&mut conn, vec![Field::new("rating", Value::Int(5))]);

    let ops = conn.all_ops().unwrap();
    assert_eq!(ops.len(), 1);
    let op = &ops[0];
    assert_eq!(op.op_type, "create_metarecord");
    assert_eq!(op.entity_uuid, m.uuid);
    assert_eq!(op.parent_id, None, "first operation has no parent");
    assert_eq!(op.seq, 1);

    // After-snapshot contains the created field rows; no before rows.
    assert_eq!(conn.snapshots(op.id, false).unwrap().len(), 0);
    assert_eq!(conn.snapshots(op.id, true).unwrap().len(), 1);

    // HEAD points at the operation.
    assert_eq!(conn.head().unwrap(), Some(op.id));
}

// ── Writer: set_field ─────────────────────────────────────────────────────────

#[test]
fn test_set_field_replaces_multimap_and_bumps_version() {
    let (mut conn, _dir) = test_conn();
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

    let got = conn.metarecord(m.uuid).unwrap().unwrap();
    let tags = got.get_all("tag");
    assert_eq!(tags, vec![&Value::String("blues".into())]);
    assert_ne!(got.version, m.version, "the write moves the version");
    assert_eq!(got.version, version_of_content(&conn, m.uuid));

    // Log: before-snapshot has the two old rows, after-snapshot the new one.
    let op = ops_of_type(&conn, "set_field").pop().expect("a set_field operation");
    let counts =
        (conn.snapshots(op.id, false).unwrap().len(), conn.snapshots(op.id, true).unwrap().len());
    assert_eq!(counts, (2, 1));
    assert_eq!(op.entity_version_before, Some(m.version), "the state the op moved away from");
}

#[test]
fn test_set_field_on_unknown_record_fails() {
    let (mut conn, _dir) = test_conn();
    let mut w = Writer::begin(&mut conn, None).unwrap();
    assert!(w.set_field(Uuid::new_v4(), "rating", Value::Int(1)).is_err());
}

// ── Writer: append / replace / delete field ───────────────────────────────────

#[test]
fn test_append_field_keeps_existing_rows() {
    let (mut conn, _dir) = test_conn();
    let m = create(&mut conn, vec![Field::new("tag", Value::String("jazz".into()))]);

    let mut w = Writer::begin(&mut conn, None).unwrap();
    w.append_field(m.uuid, "tag", Value::String("live".into())).unwrap();
    w.commit().unwrap();

    let got = conn.metarecord(m.uuid).unwrap().unwrap();
    assert_eq!(got.get_all("tag").len(), 2);
    assert_ne!(got.version, m.version);
    assert_eq!(got.version, version_of_content(&conn, m.uuid));
}

#[test]
fn test_replace_field_keeps_field_id() {
    let (mut conn, _dir) = test_conn();
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

    let got = conn.metarecord(m.uuid).unwrap().unwrap();
    let replaced = got.fields.iter().find(|f| f.id == Some(target_id)).unwrap();
    assert_eq!(replaced.value, Value::String("blues".into()));
    assert_eq!(got.get_all("tag").len(), 2, "the sibling row must be untouched");
}

#[test]
fn test_replace_field_rejects_foreign_field_id() {
    let (mut conn, _dir) = test_conn();
    let m1 = create(&mut conn, vec![Field::new("a", Value::Int(1))]);
    let m2 = create(&mut conn, vec![Field::new("a", Value::Int(2))]);

    let mut w = Writer::begin(&mut conn, None).unwrap();
    let err = w.replace_field(m1.uuid, m2.fields[0].id.unwrap(), Value::Int(3)).unwrap_err();
    assert!(err.to_string().contains("not found"), "unexpected error: {err}");
}

#[test]
fn test_delete_field_removes_single_row() {
    let (mut conn, _dir) = test_conn();
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

    let got = conn.metarecord(m.uuid).unwrap().unwrap();
    assert_eq!(got.get_all("tag"), vec![&Value::String("live".into())]);
    assert_ne!(got.version, m.version);
    assert_eq!(got.version, version_of_content(&conn, m.uuid));
}

// ── Writer: delete entry ──────────────────────────────────────────────────────

#[test]
fn test_delete_record_removes_everything_and_snapshots_before() {
    let (mut conn, _dir) = test_conn();
    let m = create(&mut conn, vec![Field::new("a", Value::Int(1)), Field::new("b", Value::Int(2))]);

    let mut w = Writer::begin(&mut conn, None).unwrap();
    w.delete_metarecord(m.uuid).unwrap();
    w.commit().unwrap();

    assert!(conn.metarecord(m.uuid).unwrap().is_none());
    assert!(conn.rows(m.uuid).unwrap().is_empty());
    assert!(conn.version(m.uuid).unwrap().is_none());

    let op = ops_of_type(&conn, "delete_metarecord").pop().expect("a delete operation");
    assert_eq!(conn.snapshots(op.id, false).unwrap().len(), 2);
}

// ── Writer: revision grouping and HEAD chain ──────────────────────────────────

#[test]
fn test_multiple_ops_in_one_revision_chain() {
    let (mut conn, _dir) = test_conn();
    let m = create(&mut conn, vec![]);

    let mut w = Writer::begin(&mut conn, Some("batch".into())).unwrap();
    w.set_field(m.uuid, "a", Value::Int(1)).unwrap();
    w.set_field(m.uuid, "b", Value::Int(2)).unwrap();
    w.set_field(m.uuid, "c", Value::Int(3)).unwrap();
    w.commit().unwrap();

    // The three operations share one revision, with seq 1..3 and a parent chain.
    let ops = ops_of_type(&conn, "set_field");
    assert_eq!(ops.len(), 3);
    let rev = ops[0].rev_id;
    assert!(ops.iter().all(|o| o.rev_id == rev));
    assert_eq!(ops.iter().map(|o| o.seq).collect::<Vec<_>>(), vec![1, 2, 3]);
    assert_eq!(ops[1].parent_id, Some(ops[0].id));
    assert_eq!(ops[2].parent_id, Some(ops[1].id));

    let label = conn.revisions(&[rev]).unwrap().remove(&rev).and_then(|m| m.label);
    assert_eq!(label.as_deref(), Some("batch"));
    assert_eq!(conn.head().unwrap(), Some(ops[2].id));

    // The version describes the state the three ops left behind.
    assert_eq!(
        conn.metarecord(m.uuid).unwrap().unwrap().version,
        version_of_content(&conn, m.uuid)
    );
}

#[test]
fn test_large_revision_chain_across_bulk_chunks() {
    // More operations than the incremental flush threshold (4096): the parent
    // chain, seq numbering, snapshots and HEAD must stay correct across it.
    const N: i64 = 5000;
    let (mut conn, _dir) = test_conn();
    let m = create(&mut conn, vec![]);

    let mut w = Writer::begin(&mut conn, None).unwrap();
    for i in 0..N {
        w.set_field(m.uuid, &format!("f{i}"), Value::Int(i)).unwrap();
    }
    w.commit().unwrap();

    let ops = ops_of_type(&conn, "set_field");
    assert_eq!(ops.len(), N as usize);
    let create_op = ops_of_type(&conn, "create_metarecord")[0].id;
    assert_eq!(ops[0].parent_id, Some(create_op), "first op chains to the previous HEAD");
    for (i, op) in ops.iter().enumerate() {
        assert_eq!(op.seq, i as i64 + 1, "seq numbering");
        if i > 0 {
            assert_eq!(op.parent_id, Some(ops[i - 1].id), "parent chain at op {i}");
        }
        assert_eq!(conn.snapshots(op.id, true).unwrap().len(), 1, "after-snapshot of op {i}");
    }
    assert_eq!(conn.head().unwrap(), Some(ops.last().unwrap().id));
}

#[test]
fn test_ancestry_detects_cycle() {
    let (mut conn, _dir) = test_conn();
    let m = create(&mut conn, vec![]);
    let mut w = Writer::begin(&mut conn, None).unwrap();
    w.set_field(m.uuid, "a", Value::Int(1)).unwrap();
    w.set_field(m.uuid, "b", Value::Int(2)).unwrap();
    w.commit().unwrap();

    // Corrupt the log: point the oldest operation at the newest one.
    let ops = conn.all_ops().unwrap();
    let (oldest, newest) = (ops.iter().min_by_key(|o| o.id), ops.iter().max_by_key(|o| o.id));
    let mut corrupt = oldest.unwrap().clone();
    corrupt.parent_id = Some(newest.unwrap().id);
    let (before, after) =
        (conn.snapshots(corrupt.id, false).unwrap(), conn.snapshots(corrupt.id, true).unwrap());
    let txn = conn.begin_write().unwrap();
    txn.import_op(&corrupt, &before, &after).unwrap();
    txn.commit().unwrap();

    let head = conn.head().unwrap().unwrap();
    let err = conn.ancestry(head).unwrap_err();
    assert!(err.to_string().contains("cycle"), "unexpected error: {err}");
}

#[test]
fn test_empty_writer_leaves_no_revision() {
    let (mut conn, _dir) = test_conn();
    let w = Writer::begin(&mut conn, None).unwrap();
    w.commit().unwrap();
    assert_eq!(conn.counts().unwrap().1, 0);
}

#[test]
fn test_dropped_writer_rolls_back() {
    let (mut conn, _dir) = test_conn();
    {
        let mut w = Writer::begin(&mut conn, None).unwrap();
        w.create_metarecord(vec![Field::new("a", Value::Int(1))]).unwrap();
        // No commit: dropped here.
    }
    assert_eq!(conn.metarecord_count().unwrap(), 0, "uncommitted writes must roll back");
}

// ── TreeRef validation ────────────────────────────────────────────────────────

#[test]
fn test_tree_ref_parent_must_exist() {
    let (mut conn, _dir) = test_conn();
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
    let (mut conn, _dir) = test_conn();
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
fn tree_name(conn: &KvStore, uuid: Uuid) -> TreeName {
    let m = conn.metarecord(uuid).unwrap().expect("metarecord");
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
    let (mut conn, _dir) = test_conn();
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
fn test_a_literal_name_and_an_escaped_one_are_distinct_siblings() {
    // "caf%E9.mp4" is both a legal file name and how the byte 0xE9 is shown.
    // The two files display alike, so a text-keyed forest would reject the
    // second as a duplicate. The forest is keyed on the bytes, so both exist.
    let (mut conn, _dir) = test_conn();
    let root = create(
        &mut conn,
        vec![Field::new("mfr_path", Value::TreeRef { parent: None, name: "".into() })],
    );
    let literal = TreeName::from("caf%E9.mp4");
    let escaped = TreeName::from_bytes(b"caf\xe9.mp4".to_vec());
    assert_eq!(
        literal.display(),
        escaped.display(),
        "the test is pointless unless they collide as text"
    );

    let a = create(
        &mut conn,
        vec![Field::new(
            "mfr_path",
            Value::TreeRef { parent: Some(root.uuid), name: literal.clone() },
        )],
    );
    let b = create(
        &mut conn,
        vec![Field::new(
            "mfr_path",
            Value::TreeRef { parent: Some(root.uuid), name: escaped.clone() },
        )],
    );
    assert_eq!(tree_name(&conn, a.uuid), literal);
    assert_eq!(tree_name(&conn, b.uuid), escaped);
}

#[test]
fn test_the_same_name_twice_in_one_directory_is_still_rejected() {
    // The uniqueness the forest relies on must survive the move to bytes.
    let (mut conn, _dir) = test_conn();
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

#[test]
fn test_tree_ref_cycle_rejected() {
    let (mut conn, _dir) = test_conn();
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
    let (mut conn, _dir) = test_conn();
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
    let (mut conn, _dir) = test_conn();
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

// ── The version is a content hash (spec-data-model "Version") ───────────────

use metafolder_daemon::log;
use metafolder_daemon::version;

mod common;

/// The value a field write assigns, read back from the row.
fn set_field(conn: &mut KvStore, uuid: Uuid, name: &str, v: Value) {
    let mut w = Writer::begin(conn, None).unwrap();
    w.set_field(uuid, name, v).unwrap();
    w.commit().unwrap();
}

/// The version the metarecord's current rows dictate, computed from outside the
/// write path. Everything below compares what the daemon *stored* against what
/// the content *says* — two routes to the same answer.
fn version_of_content(conn: &KvStore, uuid: Uuid) -> u64 {
    version::of_rows(uuid, &conn.rows(uuid).unwrap())
}

#[test]
fn test_stored_version_always_describes_the_stored_content() {
    // The invariant everything else rests on. A write assigns the version
    // incrementally, from the rows it moves; this checks that shortcut against
    // a full recompute, after every kind of write.
    let (mut conn, _dir) = test_conn();
    let m = create(&mut conn, vec![Field::new("a", Value::Int(1))]);
    assert_eq!(conn.version(m.uuid).unwrap(), Some(version_of_content(&conn, m.uuid)));

    set_field(&mut conn, m.uuid, "b", Value::String("x".into()));
    assert_eq!(conn.version(m.uuid).unwrap(), Some(version_of_content(&conn, m.uuid)));

    {
        let mut w = Writer::begin(&mut conn, None).unwrap();
        w.append_field(m.uuid, "tag", Value::String("jazz".into())).unwrap();
        w.commit().unwrap();
    }
    assert_eq!(conn.version(m.uuid).unwrap(), Some(version_of_content(&conn, m.uuid)));

    set_field(&mut conn, m.uuid, "a", Value::Nothing);
    assert_eq!(conn.version(m.uuid).unwrap(), Some(version_of_content(&conn, m.uuid)));
}

#[test]
fn test_a_value_put_back_restores_the_version() {
    // The property the counter could not have: changing a field and changing it
    // back leaves the metarecord in the state it was in, so it leaves the
    // version there too. This is what lets sync say "nothing to propagate".
    let (mut conn, _dir) = test_conn();
    let m = create(&mut conn, vec![]);

    set_field(&mut conn, m.uuid, "a", Value::Int(1));
    let at_one = conn.version(m.uuid).unwrap();

    set_field(&mut conn, m.uuid, "a", Value::Int(2));
    assert_ne!(conn.version(m.uuid).unwrap(), at_one);

    set_field(&mut conn, m.uuid, "a", Value::Int(1));
    assert_eq!(conn.version(m.uuid).unwrap(), at_one);
}

#[test]
fn test_rollback_and_redo_land_on_the_version_the_content_dictates() {
    // Navigation restores the rows; the version follows them. Nothing is
    // replayed from the log, so a rewind cannot leave a metarecord carrying a
    // version that describes some other state.
    let (mut conn, _dir) = test_conn();
    let m = create(&mut conn, vec![]);

    set_field(&mut conn, m.uuid, "a", Value::Int(1));
    let head_v1 = conn.head().unwrap();
    let at_one = conn.version(m.uuid).unwrap();

    set_field(&mut conn, m.uuid, "a", Value::Int(2));
    let head_v2 = conn.head().unwrap();
    let at_two = conn.version(m.uuid).unwrap();

    log::navigate(&mut conn, head_v1).unwrap();
    assert_eq!(conn.version(m.uuid).unwrap(), at_one);
    assert_eq!(conn.version(m.uuid).unwrap(), Some(version_of_content(&conn, m.uuid)));

    log::navigate(&mut conn, head_v2).unwrap();
    assert_eq!(conn.version(m.uuid).unwrap(), at_two);
    assert_eq!(conn.version(m.uuid).unwrap(), Some(version_of_content(&conn, m.uuid)));
}

#[test]
fn test_a_write_after_a_rollback_reuses_the_version_of_the_state_it_recreates() {
    // The opposite of what the allocator guaranteed, and the point of the
    // change: a version names a *state*, not a write. Rolling back and writing
    // the value the rolled-back write had put there returns to that state, so
    // it returns to its version.
    let (mut conn, _dir) = test_conn();
    let m = create(&mut conn, vec![]);

    set_field(&mut conn, m.uuid, "a", Value::Int(1));
    let head_v1 = conn.head().unwrap();
    set_field(&mut conn, m.uuid, "a", Value::Int(2));
    let at_two = conn.version(m.uuid).unwrap();

    log::navigate(&mut conn, head_v1).unwrap();
    set_field(&mut conn, m.uuid, "a", Value::Int(2));
    assert_eq!(conn.version(m.uuid).unwrap(), at_two);
}

#[test]
fn test_entity_version_columns_name_the_states_around_the_operation() {
    // `entity_version_before`/`after` are provenance, not authority: nothing
    // reads them to decide what to write. They must still agree with the states
    // they name, or a client correlating an operation with something it
    // observed from outside the revision would be misled.
    let (mut conn, _dir) = test_conn();
    let m = create(&mut conn, vec![]);
    let before_write = conn.version(m.uuid).unwrap().unwrap();

    set_field(&mut conn, m.uuid, "a", Value::Int(1));
    let after_write = conn.version(m.uuid).unwrap().unwrap();

    let op = ops_of_type(&conn, "set_field").pop().expect("a set_field operation");
    assert_eq!(op.entity_version_before, Some(before_write));
    assert_eq!(op.entity_version_after, Some(after_write));
}

#[test]
fn test_recreating_a_metarecord_restores_its_content_version() {
    // Navigating across a delete recreates the record. There is no allocator to
    // rebuild and no stored number to replay: the restored rows say what the
    // version is.
    let (mut conn, _dir) = test_conn();
    let m = create(&mut conn, vec![]);
    set_field(&mut conn, m.uuid, "a", Value::Int(1));
    set_field(&mut conn, m.uuid, "a", Value::Int(2));
    let head_v2 = conn.head().unwrap();
    let at_two = conn.version(m.uuid).unwrap();

    {
        let mut w = Writer::begin(&mut conn, None).unwrap();
        w.delete_metarecord(m.uuid).unwrap();
        w.commit().unwrap();
    }
    assert_eq!(conn.version(m.uuid).unwrap(), None);

    log::navigate(&mut conn, head_v2).unwrap();
    assert_eq!(conn.version(m.uuid).unwrap(), at_two);
    assert_eq!(conn.version(m.uuid).unwrap(), Some(version_of_content(&conn, m.uuid)));
}

// ── Bounded log reading (efficient log listing for huge repos) ──────────────────

#[test]
fn test_ancestry_ops_limited_returns_the_most_recent() {
    // A linear chain of writes: create + four set_fields on one metarecord.
    let (mut conn, _dir) = test_conn();
    let m = create(&mut conn, vec![Field::new("s", Value::Int(0))]);
    for i in 1..=4 {
        set_field(&mut conn, m.uuid, "s", Value::Int(i));
    }
    let head = conn.head().unwrap().unwrap();
    // `ancestry_ops` is HEAD-first (depth 0 = HEAD) up to the root.
    let full = conn.ancestry_ops(head, None).unwrap();
    assert_eq!(full.len(), 5, "create + four sets");

    // Bounded to 2: exactly the two most recent (HEAD and its parent), HEAD-first
    // — the prefix of the full ancestry, walked without scanning the whole log.
    let limited = conn.ancestry_ops(head, Some(2)).unwrap();
    assert_eq!(
        limited.iter().map(|o| o.id).collect::<Vec<_>>(),
        full.iter().take(2).map(|o| o.id).collect::<Vec<_>>(),
    );
    // A cap larger than the chain returns the whole ancestry.
    let big = conn.ancestry_ops(head, Some(999)).unwrap();
    assert_eq!(
        big.iter().map(|o| o.id).collect::<Vec<_>>(),
        full.iter().map(|o| o.id).collect::<Vec<_>>()
    );
}

#[test]
fn test_ancestry_ops_until_stops_at_the_anchor() {
    // The index's forward delta: the operations a write appended on top of a
    // known anchor. Reading them must cost the delta, not the whole log — so the
    // walk stops at the anchor instead of running to the root.
    let (mut conn, _dir) = test_conn();
    let m = create(&mut conn, vec![Field::new("s", Value::Int(0))]);
    for i in 1..=4 {
        set_field(&mut conn, m.uuid, "s", Value::Int(i));
    }
    let head = conn.head().unwrap().unwrap();
    let full = conn.ancestry_ops(head, None).unwrap();
    assert_eq!(full.len(), 5, "create + four sets");
    let anchor = full[2].id; // two operations behind HEAD

    // HEAD-first, anchor excluded: exactly the two operations on top of it.
    let delta = ops_until(&conn, head, anchor, 100).unwrap();
    assert_eq!(
        delta.iter().map(|o| o.id).collect::<Vec<_>>(),
        full.iter().take(2).map(|o| o.id).collect::<Vec<_>>(),
    );
    // An anchor that *is* HEAD is an empty delta, not a miss.
    assert!(ops_until(&conn, head, head, 100).unwrap().is_empty());
    // Too far for the budget → None (the caller rebuilds instead).
    assert!(ops_until(&conn, head, anchor, 1).is_none());
    // An id that is not on the chain at all → None, whatever the budget.
    assert!(ops_until(&conn, head, 999_999, 100).is_none());
    // The root has no parent: walking past it must not loop or error.
    let root = full.last().unwrap().id;
    assert_eq!(ops_until(&conn, head, root, 100).unwrap().len(), full.len() - 1,);
}

#[test]
fn test_assemble_selected_is_cancellable() {
    use metafolder_daemon::query_result as query_exec;
    // The select-projection loop (the dominant cost of `select=*` over many
    // matches) must honour the cancellation probe.
    let (mut conn, _dir) = test_conn();
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
    let (mut conn, _dir) = test_conn();
    let m = create(&mut conn, vec![Field::new("s", Value::Int(0))]);
    set_field(&mut conn, m.uuid, "s", Value::Int(1));
    let head = conn.head().unwrap().unwrap();
    // HEAD is the tip of the chain: nothing points at it as a parent.
    assert!(!conn.has_children(head).unwrap());
    // Its parent does have a forward child (HEAD).
    let parent = conn.op(head).unwrap().unwrap().parent_id.unwrap();
    assert!(conn.has_children(parent).unwrap());
}

// ── One daemon per repository ─────────────────────────────────────────────────

// ── Join plans (correlated reads must seek, not scan) ─────────────────────────

// ── What a revision obliges its caller to refresh ─────────────────────────────

/// A `Writer` that has recorded `n` operations touching nothing of interest.
fn pad_operations(w: &mut Writer, uuid: Uuid, n: usize) {
    for i in 0..n {
        w.set_field(uuid, "pad", Value::Int(i as i64)).unwrap();
    }
}

#[test]
fn test_a_revision_reports_what_it_did_to_the_forest_in_write_order() {
    let (mut conn, _dir) = test_conn();
    let root = create(&mut conn, vec![Field::new("p", tree_ref(None, "r"))]).uuid;
    let a = create(&mut conn, vec![]).uuid;
    let b = create(&mut conn, vec![]).uuid;

    let mut w = Writer::begin(&mut conn, None).unwrap();
    w.set_field(a, "p", tree_ref(Some(root), "a")).unwrap();
    w.set_field(b, "p", tree_ref(Some(root), "b")).unwrap();
    w.set_field(a, "note", Value::String("x".into())).unwrap();
    let effects = w.effects();
    w.commit().unwrap();

    assert!(effects.touches_tree());
    assert!(!effects.touches_watch());
    let moved: Vec<(&str, Uuid)> =
        effects.tree_ops().iter().map(|op| (op.field(), op.uuid())).collect();
    assert_eq!(
        moved,
        [("p", a), ("p", b)],
        "what each operation moved, in the order it was written"
    );
}

/// One description per *shape* of operation, and it has to be the right one:
/// a set replaces the cell, an append and a row deletion name only what they
/// move.
#[test]
fn test_each_shape_of_operation_says_what_it_did_to_the_forest() {
    use metafolder_daemon::log::TreeOp;

    let (mut conn, _dir) = test_conn();
    let root = create(&mut conn, vec![Field::new("p", tree_ref(None, "r"))]).uuid;
    let a = create(&mut conn, vec![Field::new("p", tree_ref(Some(root), "a"))]).uuid;

    // A set replaces the cell.
    let mut w = Writer::begin(&mut conn, None).unwrap();
    w.set_field(a, "p", tree_ref(Some(root), "a2")).unwrap();
    let effects = w.effects();
    w.commit().unwrap();
    assert!(matches!(effects.tree_ops(), [TreeOp::Set { positions, .. }] if positions.len() == 1));

    // An append adds to it — to a record with no position yet (a second one
    // is refused).
    let c = create(&mut conn, vec![Field::new("note", Value::String("c".into()))]).uuid;
    let mut w = Writer::begin(&mut conn, None).unwrap();
    w.append_field(c, "p", tree_ref(Some(root), "c")).unwrap();
    let effects = w.effects();
    w.commit().unwrap();
    assert!(matches!(effects.tree_ops(), [TreeOp::Add { positions, .. }] if positions.len() == 1));

    // Deleting one row takes that position out, and names it.
    let rows = conn.rows_named(c, "p").unwrap();
    let second = rows.last().unwrap().id;
    let mut w = Writer::begin(&mut conn, None).unwrap();
    w.delete_field(c, second).unwrap();
    let effects = w.effects();
    w.commit().unwrap();
    match effects.tree_ops() {
        [TreeOp::Remove { positions, .. }] => {
            assert_eq!(positions.len(), 1);
            assert_eq!(positions[0].row, second, "the row it moved, by id");
        }
        other => panic!("expected one Remove, got {other:?}"),
    }

    // Deleting the metarecord empties every cell it held.
    let mut w = Writer::begin(&mut conn, None).unwrap();
    w.delete_metarecord(a).unwrap();
    let effects = w.effects();
    w.commit().unwrap();
    assert!(matches!(effects.tree_ops(), [TreeOp::Set { positions, .. }] if positions.is_empty()));

    // A write that moves no position says nothing at all.
    let mut w = Writer::begin(&mut conn, None).unwrap();
    w.set_field(root, "note", Value::String("x".into())).unwrap();
    let effects = w.effects();
    w.commit().unwrap();
    assert!(effects.tree_ops().is_empty());
}

#[test]
fn test_a_tree_cell_survives_a_flush_of_the_operation_buffer() {
    // The buffered operations are written out in batches, and the effects used
    // to be read off that buffer: a revision long enough to flush forgot every
    // tree write that preceded the flush, and left the cache stale.
    let (mut conn, _dir) = test_conn();
    let root = create(&mut conn, vec![Field::new("p", tree_ref(None, "r"))]).uuid;
    let a = create(&mut conn, vec![]).uuid;

    let mut w = Writer::begin(&mut conn, None).unwrap();
    w.set_field(a, "p", tree_ref(Some(root), "a")).unwrap();
    pad_operations(&mut w, a, metafolder_daemon::log::FLUSH_THRESHOLD + 1);
    let effects = w.effects();
    w.commit().unwrap();

    assert_eq!(effects.tree_ops().len(), 1);
    assert_eq!((effects.tree_ops()[0].field(), effects.tree_ops()[0].uuid()), ("p", a));
}

#[test]
fn test_a_revision_reports_a_change_to_the_watched_scope() {
    let (mut conn, _dir) = test_conn();
    let a = create(&mut conn, vec![]).uuid;

    let mut w = Writer::begin(&mut conn, None).unwrap();
    w.set_field(a, "mf_watch", Value::Bool(true)).unwrap();
    let effects = w.effects();
    w.commit().unwrap();

    assert!(effects.touches_watch());
    assert!(!effects.touches_tree());
}

#[test]
fn test_a_revision_touching_neither_asks_for_no_refresh() {
    let (mut conn, _dir) = test_conn();
    let a = create(&mut conn, vec![]).uuid;

    let mut w = Writer::begin(&mut conn, None).unwrap();
    w.set_field(a, "note", Value::String("x".into())).unwrap();
    let effects = w.effects();
    w.commit().unwrap();

    assert!(!effects.touches_tree());
    assert!(!effects.touches_watch());
    assert!(effects.tree_ops().is_empty());
}

#[test]
fn test_a_large_revision_lists_every_cell_it_changed() {
    // The list used to be dropped past a cap, and the caller rebuilt the whole
    // forest instead. A revision that changes thousands of positions is exactly
    // the one whose cache upkeep must not be guessed at, so it is never
    // truncated — the cache settles a batch of any size.
    let (mut conn, _dir) = test_conn();
    let root = create(&mut conn, vec![Field::new("p", tree_ref(None, "r"))]).uuid;

    const N: usize = 5000;
    let mut w = Writer::begin(&mut conn, None).unwrap();
    for i in 0..N {
        let uuid = w.create_metarecord(vec![]).unwrap().uuid;
        w.set_field(uuid, "p", tree_ref(Some(root), &format!("n{i}"))).unwrap();
    }
    let effects = w.effects();
    w.commit().unwrap();

    assert!(effects.touches_tree());
    assert_eq!(effects.tree_ops().len(), N, "every position moved, none dropped");
}

/// A `tree_ref` value, spelled once for the tests above.
fn tree_ref(parent: Option<Uuid>, name: &str) -> Value {
    Value::TreeRef { parent, name: name.into() }
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
fn forest_pair(conn: &mut KvStore) -> (Uuid, Uuid) {
    let root = create(conn, vec![Field::new("p", tree_ref(None, "animals"))]).uuid;
    let child = create(conn, vec![Field::new("p", tree_ref(Some(root), "cat"))]).uuid;
    (root, child)
}

/// The error a committed revision refuses with, or a panic if it committed.
fn refused(conn: &mut KvStore, write: impl FnOnce(&mut Writer)) -> String {
    let mut w = Writer::begin(conn, None).unwrap();
    write(&mut w);
    w.commit().expect_err("the revision should have been refused").to_string()
}

#[test]
fn test_a_position_with_children_cannot_be_unset() {
    let (mut conn, _dir) = test_conn();
    let (root, child) = forest_pair(&mut conn);

    let err = refused(&mut conn, |w| {
        w.delete_fields_named(root, "p").unwrap();
    });
    assert!(err.contains("placed under it"), "unexpected error: {err}");
    // Nothing was written: the child still resolves through its parent.
    assert_eq!(conn.rows_named(root, "p").unwrap().len(), 1);
    let _ = child;
}

#[test]
fn test_a_position_with_children_cannot_be_replaced_by_nothing() {
    let (mut conn, _dir) = test_conn();
    let (root, _child) = forest_pair(&mut conn);

    let err = refused(&mut conn, |w| {
        w.set_field(root, "p", Value::Nothing).unwrap();
    });
    assert!(err.contains("placed under it"), "unexpected error: {err}");
}

#[test]
fn test_a_metarecord_with_children_cannot_be_deleted() {
    let (mut conn, _dir) = test_conn();
    let (root, _child) = forest_pair(&mut conn);

    let err = refused(&mut conn, |w| {
        w.delete_metarecord(root).unwrap();
    });
    assert!(err.contains("placed under it"), "unexpected error: {err}");
    assert!(conn.metarecord(root).unwrap().is_some(), "nothing was deleted");
}

#[test]
fn test_a_whole_record_overwrite_may_not_drop_a_position_with_children() {
    let (mut conn, _dir) = test_conn();
    let (root, _child) = forest_pair(&mut conn);

    let err = refused(&mut conn, |w| {
        w.set_record(root, vec![Field::new("label", Value::String("x".into()))]).unwrap();
    });
    assert!(err.contains("placed under it"), "unexpected error: {err}");
}

#[test]
fn test_the_invariant_is_checked_at_the_end_of_the_revision_not_per_operation() {
    // Deleting a whole subtree in one revision is legitimate even though the
    // parent goes first: what must hold is the state the revision commits, not
    // every intermediate one.
    let (mut conn, _dir) = test_conn();
    let (root, child) = forest_pair(&mut conn);

    let mut w = Writer::begin(&mut conn, None).unwrap();
    w.delete_metarecord(root).unwrap();
    w.delete_metarecord(child).unwrap();
    w.commit().expect("a subtree deleted whole is allowed");
    assert!(conn.metarecord(root).unwrap().is_none());
}

#[test]
fn test_moving_a_position_keeps_its_children_and_is_allowed() {
    let (mut conn, _dir) = test_conn();
    let (root, _child) = forest_pair(&mut conn);

    let mut w = Writer::begin(&mut conn, None).unwrap();
    w.set_field(root, "p", tree_ref(None, "beasts")).unwrap();
    w.commit().expect("a rename keeps the position, so the children keep their parent");
}

#[test]
fn test_a_childless_position_is_removed_freely() {
    let (mut conn, _dir) = test_conn();
    let (_root, child) = forest_pair(&mut conn);

    let mut w = Writer::begin(&mut conn, None).unwrap();
    w.delete_fields_named(child, "p").unwrap();
    w.commit().expect("a leaf has nothing depending on it");
}

#[test]
fn test_the_watcher_still_cascades_a_vanished_directory() {
    // The filesystem is authoritative for `mfr_path`: when a directory is gone
    // it is gone, and the watcher nulls it and its descendants (spec-file-
    // tracking). The restriction is on manual writes, which have no such story.
    let (mut conn, _dir) = test_conn();
    let root = create(&mut conn, vec![Field::new("mfr_path", tree_ref(None, ""))]).uuid;
    let dir = create(&mut conn, vec![Field::new("mfr_path", tree_ref(Some(root), "d"))]).uuid;
    let _file = create(&mut conn, vec![Field::new("mfr_path", tree_ref(Some(dir), "f"))]).uuid;

    let mut w = Writer::begin(&mut conn, None).unwrap();
    w.set_field_as(OpType::FileDeleted, dir, "mfr_path", Value::Nothing).unwrap();
    w.commit().expect("the watcher's cascade is not restricted");
}

// ── No duplicate rows (spec-data-model "No duplicate rows") ───────────────────

fn s(v: &str) -> Value {
    Value::String(v.to_string())
}

/// The values of `name` on `uuid`, in row order.
fn values_of(conn: &KvStore, uuid: Uuid, name: &str) -> Vec<Value> {
    conn.rows_named(uuid, name).unwrap().into_iter().map(|r| r.value).collect()
}

#[test]
fn test_append_of_an_identical_field_is_a_no_op() {
    let (mut conn, _dir) = test_conn();
    let m = create(&mut conn, vec![Field::new("tag", s("jazz"))]);
    let existing_id = m.fields[0].id.unwrap();
    let revisions = conn.counts().unwrap().1;

    let mut w = Writer::begin(&mut conn, None).unwrap();
    let id = w.append_field(m.uuid, "tag", s("jazz")).unwrap().id();
    w.commit().unwrap();

    assert_eq!(id, existing_id, "the row that already holds the value is returned");
    assert_eq!(values_of(&conn, m.uuid, "tag"), vec![s("jazz")]);
    let got = conn.metarecord(m.uuid).unwrap().unwrap();
    assert_eq!(got.version, m.version, "a no-op must not move the version");
    assert_eq!(ops_of_type(&conn, "append_field").len() as i64, 0, "a no-op must not be logged");
    assert_eq!(conn.counts().unwrap().1, revisions, "no empty revision");
}

#[test]
fn test_append_of_an_identical_nothing_is_a_no_op() {
    let (mut conn, _dir) = test_conn();
    let m = create(&mut conn, vec![Field::new("note", Value::Nothing)]);

    let mut w = Writer::begin(&mut conn, None).unwrap();
    w.append_field(m.uuid, "note", Value::Nothing).unwrap();
    w.commit().unwrap();

    assert_eq!(values_of(&conn, m.uuid, "note"), vec![Value::Nothing]);
}

#[test]
fn test_append_of_a_different_value_still_appends() {
    let (mut conn, _dir) = test_conn();
    let m = create(&mut conn, vec![Field::new("tag", s("jazz"))]);

    let mut w = Writer::begin(&mut conn, None).unwrap();
    w.append_field(m.uuid, "tag", s("live")).unwrap();
    w.commit().unwrap();

    assert_eq!(values_of(&conn, m.uuid, "tag"), vec![s("jazz"), s("live")]);
}

#[test]
fn test_set_field_multi_collapses_repeated_values() {
    let (mut conn, _dir) = test_conn();
    let m = create(&mut conn, vec![]);

    let mut w = Writer::begin(&mut conn, None).unwrap();
    w.set_field_multi(m.uuid, "tag", vec![s("a"), s("b"), s("a")]).unwrap();
    w.commit().unwrap();

    assert_eq!(values_of(&conn, m.uuid, "tag"), vec![s("a"), s("b")], "first occurrence wins");
}

#[test]
fn test_create_collapses_repeated_fields() {
    let (mut conn, _dir) = test_conn();
    let m = create(
        &mut conn,
        vec![
            Field::new("tag", s("a")),
            Field::new("tag", s("b")),
            Field::new("tag", s("a")),
            Field::new("note", s("a")),
        ],
    );

    assert_eq!(values_of(&conn, m.uuid, "tag"), vec![s("a"), s("b")]);
    assert_eq!(values_of(&conn, m.uuid, "note"), vec![s("a")], "another name is another row");
    assert_eq!(m.fields.len(), 3, "the returned record mirrors what was stored");
}

#[test]
fn test_set_record_collapses_repeated_fields() {
    let (mut conn, _dir) = test_conn();
    let m = create(&mut conn, vec![Field::new("tag", s("old"))]);

    let mut w = Writer::begin(&mut conn, None).unwrap();
    w.set_record(m.uuid, vec![Field::new("tag", s("a")), Field::new("tag", s("a"))]).unwrap();
    w.commit().unwrap();

    assert_eq!(values_of(&conn, m.uuid, "tag"), vec![s("a")]);
}

#[test]
fn test_by_id_edit_that_would_duplicate_a_sibling_is_rejected() {
    let (mut conn, _dir) = test_conn();
    let m = create(&mut conn, vec![Field::new("tag", s("jazz")), Field::new("tag", s("live"))]);
    let live = m.fields[1].id.unwrap();

    let mut w = Writer::begin(&mut conn, None).unwrap();
    let err = w.replace_field(m.uuid, live, s("jazz")).unwrap_err();
    assert!(err.to_string().to_lowercase().contains("duplicate"), "got: {err}");
    drop(w);

    // A rename onto a name that already holds the value is refused the same way.
    let mut w = Writer::begin(&mut conn, None).unwrap();
    let jazz = m.fields[0].id.unwrap();
    w.append_field(m.uuid, "style", s("jazz")).unwrap();
    let err = w.rename_field(m.uuid, jazz, "style", s("jazz")).unwrap_err();
    assert!(err.to_string().to_lowercase().contains("duplicate"), "got: {err}");
    drop(w);

    // Both rows are still there, untouched.
    assert_eq!(values_of(&conn, m.uuid, "tag"), vec![s("jazz"), s("live")]);
}

#[test]
fn test_retype_collapses_rows_that_convert_to_the_same_value() {
    use metafolder_core::metarecord::FieldType;

    let (mut conn, _dir) = test_conn();
    let m = create(&mut conn, vec![Field::new("n", s("1")), Field::new("n", s("01"))]);

    let mut w = Writer::begin(&mut conn, None).unwrap();
    w.retype_field("n", FieldType::Int).unwrap();
    w.commit().unwrap();

    assert_eq!(values_of(&conn, m.uuid, "n"), vec![Value::Int(1)], "two values became one row");
}
