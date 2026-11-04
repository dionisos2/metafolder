#[test]
fn watch_status_names_each_mount_the_broker_cannot_cover() {
    let resp = json!({
        "uncovered": {
            "mounts": [{"path": "/enc", "reason": "the filesystem has no fsid"}],
            "watched_dirs": 3,
        },
    });
    assert_eq!(
        uncovered_lines(&resp),
        [
            "uncovered \t/enc: watched with inotify (the filesystem has no fsid)",
            "          \t3 directory(ies) watched inside them",
        ]
    );
    assert!(uncovered_lines(&json!({"uncovered": null})).is_empty());
    assert!(uncovered_lines(&json!({})).is_empty(), "an older daemon");
}

#[test]
fn watch_activity_names_the_path_and_ranks_its_children_against_the_total() {
    let resp = json!({
        "since_ms": 0, "total": 200, "total_operations": 40,
        "path": "/dev", "events": 150, "operations": 10,
        "children": [
            {"path": "/dev/proj", "events": 120, "operations": 2},
            {"path": "/dev/x", "events": 30, "operations": 8},
        ],
    });
    assert_eq!(
        format_watch_activity(&resp),
        "/dev: 150 event(s) (75.0% of 200), 10 operation(s) (25.0% of 40) \
         since 1970-01-01T00:00:00Z\n\
         \x20 events          operations\n\
         \x20    120   60.0%         2    5.0%  /dev/proj\n\
         \x20     30   15.0%         8   20.0%  /dev/x\n"
    );
    let root = json!({
        "since_ms": 0, "total": 0, "total_operations": 0,
        "path": "", "events": 0, "operations": 0, "children": [],
    });
    assert_eq!(
        format_watch_activity(&root),
        "/: 0 event(s), 0 operation(s) since 1970-01-01T00:00:00Z\n"
    );
}

/// A daemon older than the operations count answers without it: the
/// column reads as unknown, never as zero.
#[test]
fn watch_activity_from_a_daemon_without_operations_shows_events_only() {
    let resp = json!({
        "since_ms": 0, "total": 200, "path": "", "events": 200,
        "children": [{"path": "/dev", "events": 120}],
    });
    assert_eq!(
        format_watch_activity(&resp),
        "/: 200 event(s) since 1970-01-01T00:00:00Z\n\
         \x20    120   60.0%  /dev\n"
    );
}
use super::*;

// ── --eq / --tsv helpers ─────────────────────────────────────────────────

#[test]
fn test_eq_to_dsl_types_and_and_join() {
    assert_eq!(
        eq_to_dsl(&["type=tag".into(), "name=musique/jazz".into()]).unwrap(),
        r#"(type = "tag") AND (name = "musique/jazz")"#
    );
    assert_eq!(eq_to_dsl(&["rate:int=3".into()]).unwrap(), "(rate = 3)");
    assert_eq!(eq_to_dsl(&["seen:bool=true".into()]).unwrap(), "(seen = true)");
}

#[test]
fn test_eq_to_dsl_escapes_injection() {
    // A value carrying quotes/backslashes cannot break out of the string.
    assert_eq!(eq_to_dsl(&[r#"name=a"b\c"#.into()]).unwrap(), r#"(name = "a\"b\\c")"#);
}

#[test]
fn test_eq_to_dsl_rejects_malformed() {
    assert!(eq_to_dsl(&["noequals".into()]).is_err());
    assert!(eq_to_dsl(&["ref:ref=8f3a2b1c4d5e6f708192a3b4c5d6e7f8".into()]).is_err());
}

#[test]
fn test_tsv_row_first_value_per_field() {
    let entry = json!({"fields": [
        {"name": "name", "value": {"type": "string", "value": "jazz"}},
        {"name": "exclusive", "value": {"type": "bool", "value": true}},
        {"name": "name", "value": {"type": "string", "value": "dup"}},
    ]});
    // partition is absent → empty cell; the first `name` row wins.
    assert_eq!(
        tsv_row(&entry, &["name".into(), "partition".into(), "exclusive".into()]),
        "jazz\t\ttrue"
    );
}

// ── format_reconcile (doc "Running a reconcile" sample output) ──────────────────

#[test]
fn test_format_reconcile_summary_only() {
    let resp = json!({"created": 2, "moved": 1, "candidates": []});
    assert_eq!(format_reconcile(&resp), "created: 2  moved: 1");
}

#[test]
fn test_format_reconcile_with_candidates() {
    let resp = json!({
        "created": 2,
        "moved": 1,
        "candidates": [{
            "metarecord_uuid": "abc00000000000000000000000000001",
            "stale_path": "/music/jazz/old.mp3",
            "matches": [
                {"path": "/music2/jazz_copy.mp3", "fingerprint": "partial_hash"},
                {"path": "/backup/unknown.mp3", "fingerprint": "size"},
            ],
        }],
    });
    let text = format_reconcile(&resp);
    // The command a candidate is confirmed with is one `mf` has: a forced
    // write of the one field (`mf set` is long gone, and `mf metarecord set`
    // would replace every field of the record).
    assert!(text.starts_with(
        "created: 2  moved: 1\n\nCandidates (confirm with: mf metarecord -i <uuid> field set \
         'mfr_path:tree_ref=<parent_uuid>/<name>' -f):"
    ));
    assert!(text.contains("\n  abc00000000000000000000000000001  /music/jazz/old.mp3"));
    assert!(text.contains("\n      → /music2/jazz_copy.mp3   (partial_hash)"));
    assert!(text.contains("\n      → /backup/unknown.mp3   (size)"));
}

// ── format_violation (doc "Schema" sample output) ─────────────────────────

#[test]
fn test_format_violation_with_type() {
    let v = json!({
        "metarecord_uuid": "abc00000000000000000000000000001",
        "type": "film",
        "field": "rating",
        "kind": "type",
        "message": "value of type string not allowed (expected: int)",
    });
    assert_eq!(
        format_violation(&v),
        "abc00000000000000000000000000001  film  rating  type  value of type string not allowed (expected: int)"
    );
}

#[test]
fn test_format_violation_global_constraint_dash() {
    let v = json!({
        "metarecord_uuid": "abc00000000000000000000000000001",
        "type": null,
        "field": "rating",
        "kind": "max_cardinality",
        "message": "3 rows, maximum is 1",
    });
    assert!(format_violation(&v).contains("  -  rating  max_cardinality  "));
}

// ── parse_sort ────────────────────────────────────────────────────────────

#[test]
fn test_parse_sort_default_is_asc() {
    let keys = parse_sort(&["rating".into()]).unwrap();
    assert_eq!(keys, json!([{"field": "rating", "order": "asc"}]));
}

#[test]
fn test_parse_sort_explicit_orders() {
    let keys = parse_sort(&["a:desc".into(), "b:asc".into()]).unwrap();
    assert_eq!(keys, json!([{"field": "a", "order": "desc"}, {"field": "b", "order": "asc"}]));
}

#[test]
fn test_parse_sort_rejects_bad_order() {
    assert!(parse_sort(&["a:up".into()]).is_err());
}

#[test]
fn test_parse_sort_rejects_an_empty_field_name() {
    let err = parse_sort(&[":desc".into()]).unwrap_err();
    assert!(matches!(err, CliError::Usage(ref m) if m.contains("empty field name")), "{err:?}");
}

#[test]
fn test_resolve_selector_takes_one_source() {
    let eq = ["name=x".to_string()];
    for (q, i, eq) in [
        (Some("a = 1"), Some("0"), &[][..]),
        (Some("a = 1"), None, &eq[..]),
        (None, Some("0"), &eq[..]),
    ] {
        let err = resolve_selector(q, i, eq, false).unwrap_err();
        assert!(matches!(err, CliError::Usage(ref m) if m.contains("mutually exclusive")));
    }
}
/// Every reason `POST /watch/check` gives for a path not watched reads as
/// a phrase naming what to look at.
#[test]
fn not_watched_phrase_names_each_reason() {
    let phrase = |entry: Json| not_watched_phrase(&entry);
    assert_eq!(
        phrase(json!({"reason": "offline", "offline_mount": "/media/usb"})),
        "on the volume mounted at /media/usb, which is unplugged"
    );
    assert!(phrase(json!({"reason": "second_mount", "second_mount": "/m2"}))
        .starts_with("inside /m2, a second mount"));
    assert_eq!(
        phrase(json!({"reason": "internal"})),
        "inside the daemon's own runtime directory (.metafolder/internal)"
    );
    assert!(phrase(json!({"reason": "unwatched", "watched_dir": ""}))
        .starts_with("no watch on / right now"));
    assert_eq!(phrase(json!({"reason": "novel"})), "unknown state");
    // Eligible itself, but its directory is not: the directory is named.
    assert_eq!(
        phrase(json!({
            "reason": "untracked", "eligible": true, "watched_dir": "/music",
            "dir_eligibility_reason": "something_new",
        })),
        "its directory /music is untracked: not tracked (\"something_new\")"
    );
}

#[test]
fn format_age_picks_the_coarsest_unit() {
    let now = metafolder_core::date::now_ms();
    assert_eq!(format_age(now - 5_000), "5s ago");
    assert_eq!(format_age(now - 3 * 60_000), "3m ago");
    assert_eq!(format_age(now - 2 * 3_600_000), "2h ago");
    assert_eq!(format_age(now - 4 * 86_400_000), "4d ago");
    assert_eq!(format_age(now + 60_000), "0s ago", "a clock ahead is not negative");
}
