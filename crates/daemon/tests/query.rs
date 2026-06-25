//! Integration tests for the query engine: predicate compilation, graph
//! traversal, sorting and keyset pagination (spec-query, spec-data-model).

use metafolder_core::metarecord::{Field, Value};
use metafolder_core::query::{FollowTarget, Query};
use metafolder_daemon::db;
use metafolder_daemon::log::Writer;
use metafolder_daemon::query_exec::{self, SortKey, SortOrder};
use metafolder_daemon::tree_cache::TreeCache;
use rusqlite::Connection;
use uuid::Uuid;

struct Fixture {
    conn: Connection,
    cache: TreeCache,
    db_id: Uuid,
}

impl Fixture {
    fn new() -> Self {
        let conn = db::open_in_memory().unwrap();
        db::init_schema(&conn).unwrap();
        Self { conn, cache: TreeCache::new(false), db_id: Uuid::new_v4() }
    }

    fn create(&mut self, fields: Vec<Field>) -> Uuid {
        self.create_in(self.db_id, fields)
    }

    fn create_in(&mut self, db_id: Uuid, fields: Vec<Field>) -> Uuid {
        let mut w = Writer::begin(&mut self.conn, db_id, None).unwrap();
        let m = w.create_metarecord(fields).unwrap();
        w.commit().unwrap();
        m.uuid
    }

    fn run(&mut self, query: &Query) -> Vec<Uuid> {
        let (uuids, _) =
            query_exec::execute(&self.conn, &mut self.cache, self.db_id, query, &[], None, None)
                .unwrap();
        uuids
    }

    fn run_sorted(&mut self, query: &Query, sort: &[SortKey]) -> Vec<Uuid> {
        let (uuids, _) =
            query_exec::execute(&self.conn, &mut self.cache, self.db_id, query, sort, None, None)
                .unwrap();
        uuids
    }
}

fn s(v: &str) -> Value {
    Value::String(v.into())
}

fn dt(iso: &str) -> Value {
    Value::DateTime(metafolder_core::date::iso_to_ms(iso).unwrap())
}

fn sort_asc(field: &str) -> SortKey {
    SortKey { field: field.into(), order: SortOrder::Asc }
}

fn sort_desc(field: &str) -> SortKey {
    SortKey { field: field.into(), order: SortOrder::Desc }
}

fn assert_same_set(mut got: Vec<Uuid>, mut expected: Vec<Uuid>) {
    got.sort();
    expected.sort();
    assert_eq!(got, expected);
}

// ── Three-valued logic ────────────────────────────────────────────────────────

#[test]
fn test_present_absent_unknown() {
    let mut f = Fixture::new();
    let present = f.create(vec![Field::new("rating", Value::Int(5))]);
    let absent = f.create(vec![Field::new("rating", Value::Nothing)]);
    let unknown = f.create(vec![Field::new("other", Value::Int(1))]);

    assert_same_set(f.run(&Query::IsPresent { field: "rating".into() }), vec![present]);
    assert_same_set(f.run(&Query::IsAbsent { field: "rating".into() }), vec![absent]);
    assert_same_set(f.run(&Query::IsUnknown { field: "rating".into() }), vec![unknown]);
}

// ── Comparisons ───────────────────────────────────────────────────────────────

#[test]
fn test_eq_and_multimap_semantics() {
    let mut f = Fixture::new();
    let jazz = f.create(vec![Field::new("tag", s("jazz")), Field::new("tag", s("live"))]);
    let blues = f.create(vec![Field::new("tag", s("blues"))]);

    assert_same_set(
        f.run(&Query::Eq { field: "tag".into(), value: s("jazz") }),
        vec![jazz],
    );
    assert_same_set(
        f.run(&Query::Eq { field: "tag".into(), value: s("live") }),
        vec![jazz],
    );
    assert_same_set(f.run(&Query::Eq { field: "tag".into(), value: s("blues") }), vec![blues]);
    assert!(f.run(&Query::Eq { field: "tag".into(), value: s("rock") }).is_empty());
}

#[test]
fn test_eq_other_types() {
    let mut f = Fixture::new();
    let target = f.create(vec![Field::new("label", s("x"))]);
    let e_bool = f.create(vec![Field::new("seen", Value::Bool(true))]);
    let e_float = f.create(vec![Field::new("score", Value::Float(2.5))]);
    let e_ref = f.create(vec![Field::new("author", Value::Ref(target))]);
    let e_dt = f.create(vec![Field::new("added", dt("2024-01-01T00:00:00Z"))]);

    assert_same_set(f.run(&Query::Eq { field: "seen".into(), value: Value::Bool(true) }), vec![e_bool]);
    assert!(f.run(&Query::Eq { field: "seen".into(), value: Value::Bool(false) }).is_empty());
    assert_same_set(
        f.run(&Query::Eq { field: "score".into(), value: Value::Float(2.5) }),
        vec![e_float],
    );
    assert_same_set(
        f.run(&Query::Eq { field: "author".into(), value: Value::Ref(target) }),
        vec![e_ref],
    );
    assert_same_set(
        f.run(&Query::Eq {
            field: "added".into(),
            value: dt("2024-01-01T00:00:00Z"),
        }),
        vec![e_dt],
    );
}

#[test]
fn test_neq_requires_a_differing_occurrence() {
    let mut f = Fixture::new();
    let jazz = f.create(vec![Field::new("tag", s("jazz"))]);
    let both = f.create(vec![Field::new("tag", s("jazz")), Field::new("tag", s("live"))]);
    let _absent = f.create(vec![Field::new("tag", Value::Nothing)]);
    let _unknown = f.create(vec![Field::new("x", Value::Int(1))]);

    // `both` has an occurrence ≠ jazz; `jazz` does not.
    assert_same_set(f.run(&Query::Neq { field: "tag".into(), value: s("jazz") }), vec![both]);
    assert_same_set(
        f.run(&Query::Neq { field: "tag".into(), value: s("rock") }),
        vec![jazz, both],
    );
}

#[test]
fn test_eq_string_on_tree_ref_compares_the_name() {
    let mut f = Fixture::new();
    let root = f.create(vec![Field::new("mfr_path", Value::TreeRef { parent: None, name: "".into() })]);
    let y2021 = f.create(vec![Field::new(
        "mfr_path",
        Value::TreeRef { parent: Some(root), name: "2021".into() },
    )]);
    let _y2022 = f.create(vec![Field::new(
        "mfr_path",
        Value::TreeRef { parent: Some(root), name: "2022".into() },
    )]);

    // A string operand compares against the TreeRef name component.
    assert_same_set(
        f.run(&Query::Eq { field: "mfr_path".into(), value: s("2021") }),
        vec![y2021],
    );
    // Exact name equality, not a substring match.
    assert!(f.run(&Query::Eq { field: "mfr_path".into(), value: s("202") }).is_empty());
    // Strict (parent, name) equality via a TreeRef operand still works.
    assert_same_set(
        f.run(&Query::Eq {
            field: "mfr_path".into(),
            value: Value::TreeRef { parent: Some(root), name: "2021".into() },
        }),
        vec![y2021],
    );
}

#[test]
fn test_neq_string_on_tree_ref_compares_the_name() {
    let mut f = Fixture::new();
    let root = f.create(vec![Field::new("mfr_path", Value::TreeRef { parent: None, name: "".into() })]);
    let _y2021 = f.create(vec![Field::new(
        "mfr_path",
        Value::TreeRef { parent: Some(root), name: "2021".into() },
    )]);
    let y2022 = f.create(vec![Field::new(
        "mfr_path",
        Value::TreeRef { parent: Some(root), name: "2022".into() },
    )]);

    // A TreeRef named "2021" is *equal* to the string "2021": it must not
    // count as a differing occurrence. root (named "") and 2022 do differ.
    assert_same_set(
        f.run(&Query::Neq { field: "mfr_path".into(), value: s("2021") }),
        vec![root, y2022],
    );
}

#[test]
fn test_ordered_string_comparison_on_tree_ref_name() {
    let mut f = Fixture::new();
    let root = f.create(vec![Field::new("mfr_path", Value::TreeRef { parent: None, name: "".into() })]);
    let y2021 = f.create(vec![Field::new(
        "mfr_path",
        Value::TreeRef { parent: Some(root), name: "2021".into() },
    )]);
    let _y2022 = f.create(vec![Field::new(
        "mfr_path",
        Value::TreeRef { parent: Some(root), name: "2022".into() },
    )]);

    // Same convention as sorting: tree_ref rows order by their name.
    // root's name is "" and also sorts before "2022".
    assert_same_set(
        f.run(&Query::Lt { field: "mfr_path".into(), value: s("2022") }),
        vec![root, y2021],
    );
}

#[test]
fn test_ordered_comparisons_numeric() {
    // Under the one-value-type-per-field invariant a numeric field is uniform
    // (here all Float); query-time comparability across the numeric domain is
    // still exercised by comparing against Int *literals*.
    let mut f = Fixture::new();
    let three = f.create(vec![Field::new("rating", Value::Float(3.0))]);
    let four_half = f.create(vec![Field::new("rating", Value::Float(4.5))]);
    let five = f.create(vec![Field::new("rating", Value::Float(5.0))]);

    assert_same_set(
        f.run(&Query::Gt { field: "rating".into(), value: Value::Int(3) }),
        vec![four_half, five],
    );
    assert_same_set(
        f.run(&Query::Gte { field: "rating".into(), value: Value::Float(4.5) }),
        vec![four_half, five],
    );
    assert_same_set(f.run(&Query::Lt { field: "rating".into(), value: Value::Int(4) }), vec![three]);
    assert_same_set(
        f.run(&Query::Lte { field: "rating".into(), value: Value::Int(3) }),
        vec![three],
    );
}

#[test]
fn test_ordered_comparisons_datetime_and_string() {
    let mut f = Fixture::new();
    let old = f.create(vec![Field::new("added", dt("2023-01-01T00:00:00Z"))]);
    let new = f.create(vec![Field::new("added", dt("2024-06-01T00:00:00Z"))]);
    let a = f.create(vec![Field::new("title", s("alpha"))]);
    let z = f.create(vec![Field::new("title", s("zulu"))]);

    assert_same_set(
        f.run(&Query::Gt {
            field: "added".into(),
            value: dt("2023-12-31T00:00:00Z"),
        }),
        vec![new],
    );
    assert_same_set(
        f.run(&Query::Lte {
            field: "added".into(),
            value: dt("2023-01-01T00:00:00Z"),
        }),
        vec![old],
    );
    assert_same_set(f.run(&Query::Lt { field: "title".into(), value: s("beta") }), vec![a]);
    assert_same_set(f.run(&Query::Gt { field: "title".into(), value: s("beta") }), vec![z]);
}

#[test]
fn test_comparison_with_nothing_is_rejected() {
    let mut f = Fixture::new();
    f.create(vec![Field::new("rating", Value::Int(1))]);
    let err = query_exec::execute(
        &f.conn,
        &mut f.cache,
        f.db_id,
        &Query::Eq { field: "rating".into(), value: Value::Nothing },
        &[],
        None,
        None,
    )
    .unwrap_err();
    assert!(err.message.contains("nothing"), "unexpected error: {}", err.message);
}

#[test]
fn test_oversized_query_is_rejected() {
    let mut f = Fixture::new();
    f.create(vec![Field::new("rating", Value::Int(1))]);
    // A wide Or beyond the node limit: cheap to send, rejected before compiling
    // (our check runs before any SQL is built).
    let huge = Query::Or {
        operands: (0..=query_exec::MAX_QUERY_NODES)
            .map(|_| Query::IsPresent { field: "rating".into() })
            .collect(),
    };
    let err = query_exec::execute(&f.conn, &mut f.cache, f.db_id, &huge, &[], None, None)
        .unwrap_err();
    assert!(err.message.contains("too large"), "unexpected error: {}", err.message);
    // A normal small query is unaffected.
    let ok = Query::Or {
        operands: vec![
            Query::IsPresent { field: "rating".into() },
            Query::IsAbsent { field: "rating".into() },
        ],
    };
    assert!(query_exec::execute(&f.conn, &mut f.cache, f.db_id, &ok, &[], None, None).is_ok());
}

#[test]
fn test_wide_combinator_is_rejected_with_clear_message() {
    let mut f = Fixture::new();
    f.create(vec![Field::new("rating", Value::Int(1))]);
    let leaf = || Query::IsPresent { field: "rating".into() };

    // One past SQLite's compound-select limit: our clear message, not SQLite's
    // opaque "too many terms in compound SELECT". (Node count stays well under
    // MAX_QUERY_NODES, so this is the combinator check firing, not the size one.)
    let over =
        Query::Or { operands: (0..=query_exec::MAX_COMBINATOR_OPERANDS).map(|_| leaf()).collect() };
    let err = query_exec::execute(&f.conn, &mut f.cache, f.db_id, &over, &[], None, None)
        .unwrap_err();
    assert!(err.message.contains("operands"), "unexpected error: {}", err.message);

    // Exactly the limit compiles and runs in SQLite.
    let at_limit =
        Query::Or { operands: (0..query_exec::MAX_COMBINATOR_OPERANDS).map(|_| leaf()).collect() };
    assert!(query_exec::execute(&f.conn, &mut f.cache, f.db_id, &at_limit, &[], None, None).is_ok());
}

// ── Combinators ───────────────────────────────────────────────────────────────

#[test]
fn test_and_or_not() {
    let mut f = Fixture::new();
    let a = f.create(vec![Field::new("tag", s("jazz")), Field::new("rating", Value::Int(5))]);
    let b = f.create(vec![Field::new("tag", s("jazz")), Field::new("rating", Value::Int(2))]);
    let c = f.create(vec![Field::new("tag", s("rock")), Field::new("rating", Value::Int(5))]);

    let jazz = Query::Eq { field: "tag".into(), value: s("jazz") };
    let top = Query::Gte { field: "rating".into(), value: Value::Int(4) };

    assert_same_set(
        f.run(&Query::And { operands: vec![jazz.clone(), top.clone()] }),
        vec![a],
    );
    assert_same_set(
        f.run(&Query::Or { operands: vec![jazz.clone(), top.clone()] }),
        vec![a, b, c],
    );
    assert_same_set(
        f.run(&Query::Not { operand: Box::new(jazz) }),
        vec![c],
    );
}

// ── Repository isolation ──────────────────────────────────────────────────────

#[test]
fn test_other_repo_entries_are_excluded() {
    let mut f = Fixture::new();
    let mine = f.create(vec![Field::new("tag", s("jazz"))]);
    let other_repo = Uuid::new_v4();
    let _other = f.create_in(other_repo, vec![Field::new("tag", s("jazz"))]);

    assert_same_set(f.run(&Query::Eq { field: "tag".into(), value: s("jazz") }), vec![mine]);
}

// ── Matches ───────────────────────────────────────────────────────────────────

#[test]
fn test_matches_on_string_and_tree_ref() {
    let mut f = Fixture::new();
    let live = f.create(vec![Field::new("title", s("Live in Paris"))]);
    let _studio = f.create(vec![Field::new("title", s("Studio takes"))]);
    let root = f.create(vec![Field::new("mfr_path", Value::TreeRef { parent: None, name: "".into() })]);
    let song = f.create(vec![Field::new(
        "mfr_path",
        Value::TreeRef { parent: Some(root), name: "live_set.mp3".into() },
    )]);

    assert_same_set(
        f.run(&Query::Matches { field: "title".into(), pattern: "[Ll]ive".into() }),
        vec![live],
    );
    assert_same_set(
        f.run(&Query::Matches { field: "mfr_path".into(), pattern: "^live.*mp3$".into() }),
        vec![song],
    );
}

#[test]
fn test_matches_invalid_regex_is_rejected() {
    let mut f = Fixture::new();
    f.create(vec![Field::new("title", s("x"))]);
    let res = query_exec::execute(
        &f.conn,
        &mut f.cache,
        f.db_id,
        &Query::Matches { field: "title".into(), pattern: "[unclosed".into() },
        &[],
        None,
        None,
    );
    assert!(res.is_err());
}

// ── Graph traversal ───────────────────────────────────────────────────────────

#[test]
fn test_follows_ref_condition() {
    let mut f = Fixture::new();
    let coltrane = f.create(vec![Field::new("name", s("Coltrane"))]);
    let davis = f.create(vec![Field::new("name", s("Davis"))]);
    let a = f.create(vec![Field::new("author", Value::Ref(coltrane))]);
    let _b = f.create(vec![Field::new("author", Value::Ref(davis))]);

    let q = Query::Follows {
        field: "author".into(),
        target: FollowTarget::Condition(Box::new(Query::Eq {
            field: "name".into(),
            value: s("Coltrane"),
        })),
    };
    assert_same_set(f.run(&q), vec![a]);
}

#[test]
fn test_follows_tree_path_matches_direct_children() {
    let mut f = Fixture::new();
    let root = f.create(vec![Field::new("mfr_path", Value::TreeRef { parent: None, name: "".into() })]);
    let music = f.create(vec![Field::new(
        "mfr_path",
        Value::TreeRef { parent: Some(root), name: "music".into() },
    )]);
    let song = f.create(vec![Field::new(
        "mfr_path",
        Value::TreeRef { parent: Some(music), name: "a.mp3".into() },
    )]);
    let deep_dir = f.create(vec![Field::new(
        "mfr_path",
        Value::TreeRef { parent: Some(music), name: "jazz".into() },
    )]);
    let deep_song = f.create(vec![Field::new(
        "mfr_path",
        Value::TreeRef { parent: Some(deep_dir), name: "b.mp3".into() },
    )]);

    let q = Query::Follows {
        field: "mfr_path".into(),
        target: FollowTarget::Path("/music".into()),
    };
    assert_same_set(f.run(&q), vec![song, deep_dir]);

    // Nonexistent path → empty result, not an error.
    let q = Query::Follows {
        field: "mfr_path".into(),
        target: FollowTarget::Path("/nope".into()),
    };
    assert!(f.run(&q).is_empty());
    let _ = deep_song;
}

#[test]
fn test_follows_tree_empty_path_matches_root_children() {
    // The file-manager panel resolves the tracked children of the repo root
    // by querying Follows with an empty path: "" resolves to the root entry
    // itself (the TreeRef root has the empty string as its name).
    let mut f = Fixture::new();
    let root = f.create(vec![Field::new("mfr_path", Value::TreeRef { parent: None, name: "".into() })]);
    let music = f.create(vec![Field::new(
        "mfr_path",
        Value::TreeRef { parent: Some(root), name: "music".into() },
    )]);
    let docs = f.create(vec![Field::new(
        "mfr_path",
        Value::TreeRef { parent: Some(root), name: "docs".into() },
    )]);
    let _deep = f.create(vec![Field::new(
        "mfr_path",
        Value::TreeRef { parent: Some(music), name: "a.mp3".into() },
    )]);

    let q = Query::Follows { field: "mfr_path".into(), target: FollowTarget::Path("".into()) };
    assert_same_set(f.run(&q), vec![music, docs]);
}

#[test]
fn test_follows_condition_on_tree_ref_matches_children_of_matching_parents() {
    // "all files directly inside any folder named 2021": a condition
    // right-hand side works on TreeRef fields too, matching metarecords whose
    // direct parent satisfies the sub-query.
    let mut f = Fixture::new();
    let root = f.create(vec![Field::new("mfr_path", Value::TreeRef { parent: None, name: "".into() })]);
    let y2021_a = f.create(vec![Field::new(
        "mfr_path",
        Value::TreeRef { parent: Some(root), name: "2021".into() },
    )]);
    let other = f.create(vec![Field::new(
        "mfr_path",
        Value::TreeRef { parent: Some(root), name: "other".into() },
    )]);
    let y2021_b = f.create(vec![Field::new(
        "mfr_path",
        Value::TreeRef { parent: Some(other), name: "2021".into() },
    )]);
    let file_a = f.create(vec![Field::new(
        "mfr_path",
        Value::TreeRef { parent: Some(y2021_a), name: "a.jpg".into() },
    )]);
    let file_b = f.create(vec![Field::new(
        "mfr_path",
        Value::TreeRef { parent: Some(y2021_b), name: "b.jpg".into() },
    )]);
    let _file_other = f.create(vec![Field::new(
        "mfr_path",
        Value::TreeRef { parent: Some(other), name: "c.jpg".into() },
    )]);

    let q = Query::Follows {
        field: "mfr_path".into(),
        target: FollowTarget::Condition(Box::new(Query::Eq {
            field: "mfr_path".into(),
            value: s("2021"),
        })),
    };
    assert_same_set(f.run(&q), vec![file_a, file_b]);
}

#[test]
fn test_directory_entry_lookup_by_path() {
    // The file-manager panel resolves the displayed directory's own entry
    // ("." row) with Matches on the TreeRef name: the root entry is the
    // only one with an empty name, and a subdirectory is pinned down by
    // Follows(parent) AND Matches(^name$).
    let mut f = Fixture::new();
    let root = f.create(vec![Field::new("mfr_path", Value::TreeRef { parent: None, name: "".into() })]);
    let music = f.create(vec![Field::new(
        "mfr_path",
        Value::TreeRef { parent: Some(root), name: "music".into() },
    )]);
    let _musical = f.create(vec![Field::new(
        "mfr_path",
        Value::TreeRef { parent: Some(root), name: "musical".into() },
    )]);
    let _deep = f.create(vec![Field::new(
        "mfr_path",
        Value::TreeRef { parent: Some(music), name: "music".into() },
    )]);

    let q = Query::Matches { field: "mfr_path".into(), pattern: "^$".into() };
    assert_same_set(f.run(&q), vec![root]);

    let q = Query::And {
        operands: vec![
            Query::Follows { field: "mfr_path".into(), target: FollowTarget::Path("".into()) },
            Query::Matches { field: "mfr_path".into(), pattern: "^music$".into() },
        ],
    };
    assert_same_set(f.run(&q), vec![music]);
}

#[test]
fn test_follows_transitive_collects_all_descendants() {
    let mut f = Fixture::new();
    let root = f.create(vec![Field::new("mfr_path", Value::TreeRef { parent: None, name: "".into() })]);
    let music = f.create(vec![Field::new(
        "mfr_path",
        Value::TreeRef { parent: Some(root), name: "music".into() },
    )]);
    let jazz = f.create(vec![Field::new(
        "mfr_path",
        Value::TreeRef { parent: Some(music), name: "jazz".into() },
    )]);
    let song = f.create(vec![Field::new(
        "mfr_path",
        Value::TreeRef { parent: Some(jazz), name: "a.mp3".into() },
    )]);
    let _elsewhere = f.create(vec![Field::new(
        "mfr_path",
        Value::TreeRef { parent: Some(root), name: "docs".into() },
    )]);

    let q = Query::FollowsTransitive {
        field: "mfr_path".into(),
        target: FollowTarget::Path("/music".into()),
    };
    assert_same_set(f.run(&q), vec![jazz, song]);

    let q = Query::FollowsTransitive {
        field: "mfr_path".into(),
        target: FollowTarget::Path("/nope".into()),
    };
    assert!(f.run(&q).is_empty());
}

#[test]
fn test_follows_transitive_condition_collects_descendants_of_matching_roots() {
    // "everything inside any folder named 2021", with one matching folder
    // nested inside another: descendants overlap and must be deduplicated;
    // the matching roots themselves are not descendants.
    let mut f = Fixture::new();
    let root = f.create(vec![Field::new("mfr_path", Value::TreeRef { parent: None, name: "".into() })]);
    let y2021_outer = f.create(vec![Field::new(
        "mfr_path",
        Value::TreeRef { parent: Some(root), name: "2021".into() },
    )]);
    let sub = f.create(vec![Field::new(
        "mfr_path",
        Value::TreeRef { parent: Some(y2021_outer), name: "sub".into() },
    )]);
    let deep = f.create(vec![Field::new(
        "mfr_path",
        Value::TreeRef { parent: Some(sub), name: "deep.jpg".into() },
    )]);
    let y2021_inner = f.create(vec![Field::new(
        "mfr_path",
        Value::TreeRef { parent: Some(y2021_outer), name: "2021".into() },
    )]);
    let inner_file = f.create(vec![Field::new(
        "mfr_path",
        Value::TreeRef { parent: Some(y2021_inner), name: "b.jpg".into() },
    )]);
    let other = f.create(vec![Field::new(
        "mfr_path",
        Value::TreeRef { parent: Some(root), name: "other".into() },
    )]);
    let _outside = f.create(vec![Field::new(
        "mfr_path",
        Value::TreeRef { parent: Some(other), name: "c.jpg".into() },
    )]);

    let q = Query::FollowsTransitive {
        field: "mfr_path".into(),
        target: FollowTarget::Condition(Box::new(Query::Eq {
            field: "mfr_path".into(),
            value: s("2021"),
        })),
    };
    // y2021_inner is both a matching root and a descendant of y2021_outer.
    assert_same_set(f.run(&q), vec![sub, deep, y2021_inner, inner_file]);

    // A condition matching nothing → empty result, not an error.
    let q = Query::FollowsTransitive {
        field: "mfr_path".into(),
        target: FollowTarget::Condition(Box::new(Query::Eq {
            field: "mfr_path".into(),
            value: s("nope"),
        })),
    };
    assert!(f.run(&q).is_empty());
}

// ── Sorting ───────────────────────────────────────────────────────────────────

#[test]
fn test_sort_asc_desc_with_unknown_last() {
    let mut f = Fixture::new();
    let two = f.create(vec![Field::new("rating", Value::Int(2)), Field::new("k", s("x"))]);
    let five = f.create(vec![Field::new("rating", Value::Int(5)), Field::new("k", s("x"))]);
    let unknown = f.create(vec![Field::new("k", s("x"))]);
    let nothing = f.create(vec![Field::new("rating", Value::Nothing), Field::new("k", s("x"))]);

    let all = Query::Eq { field: "k".into(), value: s("x") };
    let asc = f.run_sorted(&all, &[sort_asc("rating")]);
    assert_eq!(&asc[..2], &[two, five]);
    let mut tail = asc[2..].to_vec();
    tail.sort();
    let mut expected_tail = vec![unknown, nothing];
    expected_tail.sort();
    assert_eq!(tail, expected_tail, "unknown/Nothing always sort last");

    let desc = f.run_sorted(&all, &[sort_desc("rating")]);
    assert_eq!(&desc[..2], &[five, two]);
}

#[test]
fn test_sort_over_follows_transitive_subset() {
    // Regression: sorting the descendants of a subtree must (a) only ever
    // return rows inside that subtree, and (b) push descendants lacking the
    // sort field last — even though the sort representative is now computed by
    // joining the *filtered* universe to `field` (the fix for the pathological
    // `FollowsTransitive` + sort plan on large repos).
    let mut f = Fixture::new();
    let root = f.create(vec![Field::new("mfr_path", Value::TreeRef { parent: None, name: "".into() })]);
    let docs = f.create(vec![Field::new(
        "mfr_path",
        Value::TreeRef { parent: Some(root), name: "documents".into() },
    )]);
    let mk = |f: &mut Fixture, name: &str, mtime: Option<i64>| {
        let mut fields =
            vec![Field::new("mfr_path", Value::TreeRef { parent: Some(docs), name: name.into() })];
        if let Some(m) = mtime {
            fields.push(Field::new("mtime", Value::Int(m)));
        }
        f.create(fields)
    };
    let a = mk(&mut f, "a.txt", Some(100));
    let b = mk(&mut f, "b.txt", Some(200));
    let c = mk(&mut f, "c.txt", None); // no mtime → must sort last
    // A file outside /documents with a large mtime: must never appear.
    let music = f.create(vec![Field::new(
        "mfr_path",
        Value::TreeRef { parent: Some(root), name: "music".into() },
    )]);
    let _outside = f.create(vec![
        Field::new("mfr_path", Value::TreeRef { parent: Some(music), name: "x.mp3".into() }),
        Field::new("mtime", Value::Int(999)),
    ]);

    let q = Query::FollowsTransitive {
        field: "mfr_path".into(),
        target: FollowTarget::Path("/documents".into()),
    };
    let desc = f.run_sorted(&q, &[sort_desc("mtime")]);
    assert_eq!(desc, vec![b, a, c], "in-subtree, mtime desc, missing-field last");

    let asc = f.run_sorted(&q, &[sort_asc("mtime")]);
    assert_eq!(asc, vec![a, b, c], "in-subtree, mtime asc, missing-field still last");
}

#[test]
fn test_multi_key_sort_with_missing_secondary_over_subset() {
    // Multi-key sort over a FollowsTransitive subset where some rows lack the
    // secondary key: within a primary tie, present rows order by the secondary
    // and the missing-secondary row still sorts last (independent of direction).
    let mut f = Fixture::new();
    let root = f.create(vec![Field::new("mfr_path", Value::TreeRef { parent: None, name: "".into() })]);
    let docs = f.create(vec![Field::new(
        "mfr_path",
        Value::TreeRef { parent: Some(root), name: "documents".into() },
    )]);
    let mk = |f: &mut Fixture, name: &str, grp: &str, n: Option<i64>| {
        let mut fields = vec![
            Field::new("mfr_path", Value::TreeRef { parent: Some(docs), name: name.into() }),
            Field::new("grp", s(grp)),
        ];
        if let Some(n) = n {
            fields.push(Field::new("n", Value::Int(n)));
        }
        f.create(fields)
    };
    let a = mk(&mut f, "a", "x", Some(2));
    let b = mk(&mut f, "b", "x", Some(1));
    let c = mk(&mut f, "c", "x", None); // missing secondary → last within the x group
    let e = mk(&mut f, "e", "y", Some(5));

    let q = Query::FollowsTransitive {
        field: "mfr_path".into(),
        target: FollowTarget::Path("/documents".into()),
    };
    assert_eq!(
        f.run_sorted(&q, &[sort_asc("grp"), sort_asc("n")]),
        vec![b, a, c, e],
        "grp asc, n asc: x{{b(1),a(2),c-missing}}, then y(e)"
    );
    assert_eq!(
        f.run_sorted(&q, &[sort_asc("grp"), sort_desc("n")]),
        vec![a, b, c, e],
        "grp asc, n desc: x{{a(2),b(1),c-missing}}, then y(e)"
    );
}

#[test]
fn test_sort_multimap_uses_min_for_asc_and_max_for_desc() {
    let mut f = Fixture::new();
    // a: {1, 9}, b: {5}
    let a = f.create(vec![
        Field::new("n", Value::Int(1)),
        Field::new("n", Value::Int(9)),
        Field::new("k", s("x")),
    ]);
    let b = f.create(vec![Field::new("n", Value::Int(5)), Field::new("k", s("x"))]);

    let all = Query::Eq { field: "k".into(), value: s("x") };
    assert_eq!(f.run_sorted(&all, &[sort_asc("n")]), vec![a, b], "asc: min(a)=1 < 5");
    assert_eq!(f.run_sorted(&all, &[sort_desc("n")]), vec![a, b], "desc: max(a)=9 > 5");
}

// NOTE: a former `test_sort_mixed_types_follow_precedence` asserted the
// cross-type sort precedence (bool < int/float < string < datetime) for a field
// holding several value types at once. That state is no longer reachable through
// the Writer: a field name carries a single value type repository-wide (the
// "one value type per field name" invariant, spec-data-model). The query_exec
// cross-type precedence code is kept as a defensive fallback for any pre-invariant
// data, but it has no supported way to be produced, so the test was removed.

#[test]
fn test_sort_secondary_key_and_uuid_tiebreak() {
    let mut f = Fixture::new();
    let mut uuids = Vec::new();
    for (g, n) in [("a", 2), ("a", 1), ("b", 1)] {
        uuids.push(f.create(vec![
            Field::new("g", s(g)),
            Field::new("n", Value::Int(n)),
            Field::new("k", s("x")),
        ]));
    }
    let all = Query::Eq { field: "k".into(), value: s("x") };
    let got = f.run_sorted(&all, &[sort_asc("g"), sort_asc("n")]);
    assert_eq!(got, vec![uuids[1], uuids[0], uuids[2]]);

    // Equal on every key: ordered by UUID.
    let t1 = f.create(vec![Field::new("k", s("tie"))]);
    let t2 = f.create(vec![Field::new("k", s("tie"))]);
    let tie = Query::Eq { field: "k".into(), value: s("tie") };
    let got = f.run_sorted(&tie, &[sort_asc("g")]);
    let mut expected = vec![t1, t2];
    expected.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    assert_eq!(got, expected);
}

// ── Pagination ────────────────────────────────────────────────────────────────

#[test]
fn test_pagination_with_sort_covers_all_without_duplicates() {
    let mut f = Fixture::new();
    for i in 0..23 {
        f.create(vec![
            Field::new("n", Value::Int((i * 7) % 23)),
            Field::new("k", s("x")),
        ]);
    }
    let all = Query::Eq { field: "k".into(), value: s("x") };
    let sort = vec![sort_desc("n")];
    let reference = f.run_sorted(&all, &sort);

    let mut paged = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let (page, next) = query_exec::execute(
            &f.conn,
            &mut f.cache,
            f.db_id,
            &all,
            &sort,
            Some(5),
            cursor.as_deref(),
        )
        .unwrap();
        paged.extend(page);
        match next {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    assert_eq!(paged, reference);
}

#[test]
fn test_cursor_is_rejected_for_different_query_or_sort() {
    let mut f = Fixture::new();
    for i in 0..3 {
        f.create(vec![Field::new("n", Value::Int(i)), Field::new("k", s("x"))]);
    }
    let all = Query::Eq { field: "k".into(), value: s("x") };
    let (_, cursor) =
        query_exec::execute(&f.conn, &mut f.cache, f.db_id, &all, &[sort_asc("n")], Some(2), None)
            .unwrap();
    let cursor = cursor.unwrap();

    // Same cursor with a different sort → 400.
    let err = query_exec::execute(
        &f.conn,
        &mut f.cache,
        f.db_id,
        &all,
        &[sort_desc("n")],
        Some(2),
        Some(&cursor),
    )
    .unwrap_err();
    assert!(err.message.contains("cursor"), "unexpected error: {}", err.message);
}
