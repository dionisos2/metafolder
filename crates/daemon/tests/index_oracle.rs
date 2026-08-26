//! Equivalence oracle for the in-memory bitmap index (spec-indexing.org).
//!
//! Every query in the battery is run through BOTH the SQL engine
//! (`query_exec::execute`, the oracle) and `RepoIndex::evaluate`, asserting an
//! identical result *set* (order is irrelevant — sorting is a later increment).
//! Fixtures are crafted to exercise the correctness pitfalls: present/absent
//! overlap, multi-map min/max, the exclusively-owned universe, ZERO_UUID tree
//! roots.

use metafolder_core::metarecord::{Field, Value};
use metafolder_core::query::{FollowTarget, OsmMode, Query};
use metafolder_daemon::db;
use metafolder_daemon::index::{collect_node_paths, collect_path_targets, QueryRoots, RepoIndex, SortBy};
use metafolder_daemon::log::Writer;
use metafolder_daemon::query_exec::{self, SortKey, SortOrder};
use metafolder_daemon::tree_cache::TreeCache;
use rusqlite::Connection;
use uuid::Uuid;

struct Oracle {
    conn: Connection,
    cache: TreeCache,
}

impl Oracle {
    fn new() -> Self {
        let conn = db::open_in_memory().unwrap();
        db::init_schema(&conn).unwrap();
        Self { conn, cache: TreeCache::new(false) }
    }

    fn create(&mut self, fields: Vec<Field>) -> Uuid {
        let mut w = Writer::begin(&mut self.conn, None).unwrap();
        let m = w.create_metarecord(fields).unwrap();
        w.commit().unwrap();
        m.uuid
    }

    /// Asserts the bitmap index agrees with the SQL engine on `q`.
    fn check(&mut self, q: &Query) {
        let index = RepoIndex::build(&self.conn).unwrap();
        let (mut sql, _) =
            query_exec::execute(&self.conn, &mut self.cache, q, &[], None, None)
                .unwrap();
        let mut got = index.to_uuids(&index.evaluate(q).unwrap());
        sql.sort();
        got.sort();
        assert_eq!(got, sql, "divergence on {q:?}");
    }

    /// Asserts the bitmap index agrees with the SQL engine on the *ordered*,
    /// limited result of `q` (comparison is order-sensitive — a `Vec`, not a set).
    fn check_sorted(&mut self, q: &Query, by: &[(&str, bool)], limit: Option<usize>) {
        let index = RepoIndex::build(&self.conn).unwrap();
        let sql_keys: Vec<SortKey> = by
            .iter()
            .map(|(f, asc)| SortKey {
                field: f.to_string(),
                order: if *asc { SortOrder::Asc } else { SortOrder::Desc },
            })
            .collect();
        let (sql, _) = query_exec::execute(
            &self.conn,
            &mut self.cache,
            q,
            &sql_keys,
            limit,
            None,
        )
        .unwrap();
        let idx_keys: Vec<SortBy> =
            by.iter().map(|(f, asc)| SortBy { field: f.to_string(), ascending: *asc }).collect();
        let got = index.evaluate_sorted(q, &idx_keys, limit).unwrap();
        assert_eq!(got, sql, "sort divergence on {q:?} by {by:?} limit {limit:?}");
    }

    /// Asserts the index `count` matches the SQL `COUNT`.
    fn check_count(&mut self, q: &Query) {
        let index = RepoIndex::build(&self.conn).unwrap();
        let sql = query_exec::count(&self.conn, &mut self.cache, q).unwrap();
        assert_eq!(index.count(q).unwrap() as usize, sql, "count divergence on {q:?}");
    }

    /// Asserts the in-memory field catalog agrees with the SQL
    /// `distinct_field_names` — unfiltered and for each value type present.
    fn check_catalog(&mut self) {
        let index = RepoIndex::build(&self.conn).unwrap();
        let sql = db::distinct_field_names(&self.conn, None).unwrap();
        assert_eq!(index.field_catalog(None), sql, "catalog divergence (unfiltered)");
        let types: std::collections::BTreeSet<&str> = sql.iter().map(|(_, t)| t.as_str()).collect();
        for ty in types {
            let sql = db::distinct_field_names(&self.conn, Some(ty)).unwrap();
            assert_eq!(index.field_catalog(Some(ty)), sql, "catalog divergence (?type={ty})");
        }
    }

    /// Walks both engines page by page through the whole sorted result and
    /// asserts every page (and thus the partitioning) is identical.
    fn check_paginated(&mut self, q: &Query, by: &[(&str, bool)], limit: usize) {
        let index = RepoIndex::build(&self.conn).unwrap();
        let sql_keys: Vec<SortKey> = by
            .iter()
            .map(|(f, asc)| SortKey {
                field: f.to_string(),
                order: if *asc { SortOrder::Asc } else { SortOrder::Desc },
            })
            .collect();
        let idx_keys: Vec<SortBy> =
            by.iter().map(|(f, asc)| SortBy { field: f.to_string(), ascending: *asc }).collect();

        let mut ipages: Vec<Vec<Uuid>> = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let (page, next) =
                index.evaluate_page(q, &idx_keys, Some(limit), cursor.as_deref()).unwrap();
            ipages.push(page);
            match next {
                Some(c) => cursor = Some(c),
                None => break,
            }
            assert!(ipages.len() < 10_000, "runaway index pagination");
        }

        let mut spages: Vec<Vec<Uuid>> = Vec::new();
        let mut scursor: Option<String> = None;
        loop {
            let (page, next) = query_exec::execute(
                &self.conn,
                &mut self.cache,
                q,
                &sql_keys,
                Some(limit),
                scursor.as_deref(),
            )
            .unwrap();
            spages.push(page);
            match next {
                Some(c) => scursor = Some(c),
                None => break,
            }
            assert!(spages.len() < 10_000, "runaway sql pagination");
        }

        assert_eq!(ipages, spages, "pagination divergence on {q:?} by {by:?} limit {limit}");
    }

    /// Like [`Self::check_paginated`] but for a query carrying `Path`-target
    /// follows: the path roots are resolved through the tree cache and supplied
    /// to the index exactly as `run_query_filter` does, so this exercises the
    /// GUI's real scenario (browse a subtree, paginate by a sort key). The SQL
    /// engine resolves paths itself, so it takes the query unchanged.
    fn check_paginated_with_roots(&mut self, q: &Query, by: &[(&str, bool)], limit: usize) {
        let index = RepoIndex::build(&self.conn).unwrap();
        let mut targets = Vec::new();
        collect_path_targets(q, &mut targets);
        let mut roots = QueryRoots::new();
        for (field, path) in targets {
            if let Some(uuid) = self.cache.resolve_path(&self.conn, &field, &path).unwrap() {
                roots.path.insert((field, path), uuid);
            }
        }
        let sql_keys: Vec<SortKey> = by
            .iter()
            .map(|(f, asc)| SortKey {
                field: f.to_string(),
                order: if *asc { SortOrder::Asc } else { SortOrder::Desc },
            })
            .collect();
        let idx_keys: Vec<SortBy> =
            by.iter().map(|(f, asc)| SortBy { field: f.to_string(), ascending: *asc }).collect();

        let mut ipages: Vec<Vec<Uuid>> = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let (page, next) = index
                .evaluate_page_with_roots(q, &idx_keys, Some(limit), cursor.as_deref(), &roots)
                .unwrap();
            ipages.push(page);
            match next {
                Some(c) => cursor = Some(c),
                None => break,
            }
            assert!(ipages.len() < 10_000, "runaway index pagination");
        }

        let mut spages: Vec<Vec<Uuid>> = Vec::new();
        let mut scursor: Option<String> = None;
        loop {
            let (page, next) = query_exec::execute(
                &self.conn,
                &mut self.cache,
                q,
                &sql_keys,
                Some(limit),
                scursor.as_deref(),
            )
            .unwrap();
            spages.push(page);
            match next {
                Some(c) => scursor = Some(c),
                None => break,
            }
            assert!(spages.len() < 10_000, "runaway sql pagination");
        }

        assert_eq!(ipages, spages, "path-target pagination divergence on {q:?} by {by:?}");
    }
}

fn s(v: &str) -> Value {
    Value::String(v.into())
}
fn dt(iso: &str) -> Value {
    Value::DateTime(metafolder_core::date::iso_to_ms(iso).unwrap())
}
fn eq(field: &str, value: Value) -> Query {
    Query::Eq { field: field.into(), value }
}
fn neq(field: &str, value: Value) -> Query {
    Query::Neq { field: field.into(), value }
}
fn lt(field: &str, value: Value) -> Query {
    Query::Lt { field: field.into(), value }
}
fn lte(field: &str, value: Value) -> Query {
    Query::Lte { field: field.into(), value }
}
fn gt(field: &str, value: Value) -> Query {
    Query::Gt { field: field.into(), value }
}
fn gte(field: &str, value: Value) -> Query {
    Query::Gte { field: field.into(), value }
}

fn tref(field: &str, parent: Option<Uuid>, name: &str) -> Field {
    Field::new(field, Value::TreeRef { parent, name: name.into() })
}
fn follows(field: &str, cond: Query) -> Query {
    Query::Follows { field: field.into(), target: FollowTarget::Condition(Box::new(cond)) }
}
fn follows_t(field: &str, cond: Query) -> Query {
    Query::FollowsTransitive {
        field: field.into(),
        target: FollowTarget::Condition(Box::new(cond)),
        inclusive: false,
    }
}
fn follows_ti(field: &str, cond: Query) -> Query {
    Query::FollowsTransitive {
        field: field.into(),
        target: FollowTarget::Condition(Box::new(cond)),
        inclusive: true,
    }
}

fn and(operands: Vec<Query>) -> Query {
    Query::And { operands }
}
fn or(operands: Vec<Query>) -> Query {
    Query::Or { operands }
}
fn not(operand: Query) -> Query {
    Query::Not { operand: Box::new(operand) }
}

fn present(field: &str) -> Query {
    Query::IsPresent { field: field.into() }
}
fn absent(field: &str) -> Query {
    Query::IsAbsent { field: field.into() }
}
fn unknown(field: &str) -> Query {
    Query::IsUnknown { field: field.into() }
}

// ── Three-valued logic ──────────────────────────────────────────────────────

#[test]
fn three_valued_present_absent_unknown() {
    let mut o = Oracle::new();
    o.create(vec![Field::new("rating", Value::Int(5))]); // present
    o.create(vec![Field::new("rating", Value::Nothing)]); // absent
    o.create(vec![Field::new("other", Value::Int(1))]); // unknown for "rating"

    o.check(&present("rating"));
    o.check(&absent("rating"));
    o.check(&unknown("rating"));
}

#[test]
fn three_valued_present_absent_overlap() {
    // One metarecord carries BOTH a real value and a Nothing for "rating":
    // it must appear in IsPresent AND IsAbsent, and NOT in IsUnknown.
    let mut o = Oracle::new();
    o.create(vec![Field::new("rating", Value::Int(5)), Field::new("rating", Value::Nothing)]);
    o.create(vec![Field::new("rating", Value::Nothing)]);
    o.create(vec![Field::new("rating", Value::Int(9))]);
    o.create(vec![Field::new("elsewhere", Value::Int(1))]);

    o.check(&present("rating"));
    o.check(&absent("rating"));
    o.check(&unknown("rating"));
}

// ── Field catalog (GET /repos/:repo/fields) ─────────────────────────────────

#[test]
fn field_catalog_matches_sql() {
    let mut o = Oracle::new();
    // One field of every value type, plus a multi-map name and a name that only
    // ever holds Nothing (must be excluded — it has no usable value type).
    o.create(vec![
        Field::new("tag", s("jazz")),
        Field::new("tag", s("live")),
        Field::new("rating", Value::Int(5)),
        Field::new("weight", Value::Float(1.5)),
        Field::new("fresh", Value::Bool(true)),
        Field::new("seen", Value::DateTime(0)),
        Field::new("author", Value::Ref(Uuid::new_v4())),
        Field::new("base", Value::RefBase(Uuid::new_v4())),
        tref("loc", None, "root"),
        Field::new("note", Value::Nothing),
    ]);
    o.check_catalog();
}

#[test]
fn field_catalog_drops_field_when_last_value_removed() {
    // After *incremental* maintenance empties a field's `present` bitmap, the
    // name must disappear from the catalog (recompute_field empties the bitmap
    // but keeps the key, so the catalog must gate on non-emptiness).
    let mut o = Oracle::new();
    let m = o.create(vec![Field::new("rating", Value::Int(5))]);
    let mut index = RepoIndex::build(&o.conn).unwrap();
    assert_eq!(index.field_catalog(None), vec![("rating".to_string(), "int".to_string())]);

    let mut w = Writer::begin(&mut o.conn, None).unwrap();
    w.set_field(m, "rating", Value::Nothing).unwrap();
    w.commit().unwrap();
    index.refresh(&o.conn, &|| false).unwrap();

    let sql = db::distinct_field_names(&o.conn, None).unwrap();
    assert!(sql.is_empty(), "SQL reference no longer lists the field");
    assert_eq!(index.field_catalog(None), sql, "catalog must drop the emptied field");
}

#[test]
fn refresh_over_set_record_stays_incremental() {
    // `apply_ops` handles the `set_metarecord` op (whole-record set), so it must
    // also be in `forward_delta`'s KNOWN list — otherwise a whole-record set
    // (CLI `mf metarecord set`, or the whole-record PUT) forces a full index
    // rebuild on the next refresh, the multi-second stall this guards against on
    // a large repository. Observed through the dense-id count: the incremental
    // path keeps a deleted metarecord's tombstone id, whereas a full rebuild
    // re-interns only the live set and reclaims it.
    let mut o = Oracle::new();
    let a = o.create(vec![Field::new("tag", s("a"))]);
    let b = o.create(vec![Field::new("tag", s("b"))]);
    let mut index = RepoIndex::build(&o.conn).unwrap();
    assert_eq!(index.dense_id_count(), 2, "both metarecords interned");

    // Delete A (incremental, leaves a tombstone id) then set B whole-record.
    let mut w = Writer::begin(&mut o.conn, None).unwrap();
    w.delete_metarecord(a).unwrap();
    w.commit().unwrap();
    let mut w = Writer::begin(&mut o.conn, None).unwrap();
    w.set_record(b, vec![Field::new("tag", s("b2")), Field::new("rating", Value::Int(7))]).unwrap();
    w.commit().unwrap();

    index.refresh(&o.conn, &|| false).unwrap();

    // A full rebuild would reclaim A's id (dense_id_count == 1); the incremental
    // path keeps the tombstone (== 2).
    assert_eq!(
        index.dense_id_count(),
        2,
        "set_metarecord must refresh incrementally, not trigger a full rebuild",
    );
    // Correctness of the incremental set_metarecord handling.
    let fresh = RepoIndex::build(&o.conn).unwrap();
    assert_eq!(
        index.to_uuids(&index.evaluate(&eq("rating", Value::Int(7))).unwrap()),
        fresh.to_uuids(&fresh.evaluate(&eq("rating", Value::Int(7))).unwrap()),
    );
    assert_eq!(index.field_catalog(None), db::distinct_field_names(&o.conn, None).unwrap());
}

// ── Categorical: string ─────────────────────────────────────────────────────

#[test]
fn categorical_string_eq_multimap() {
    let mut o = Oracle::new();
    o.create(vec![Field::new("tag", s("jazz")), Field::new("tag", s("live"))]);
    o.create(vec![Field::new("tag", s("blues"))]);

    o.check(&eq("tag", s("jazz")));
    o.check(&eq("tag", s("live")));
    o.check(&eq("tag", s("blues")));
    o.check(&eq("tag", s("rock"))); // empty
}

#[test]
fn uuid_in_explicit_set() {
    let mut o = Oracle::new();
    let a = o.create(vec![Field::new("tag", s("a"))]);
    let _b = o.create(vec![Field::new("tag", s("b"))]);
    let c = o.create(vec![Field::new("tag", s("c"))]);
    let bogus = Uuid::from_u128(0x99);

    o.check(&Query::UuidIn { uuids: vec![a, c, bogus] });
    o.check(&Query::UuidIn { uuids: vec![] }); // empty
    // Combined with another predicate (intersection).
    o.check(&Query::And {
        operands: vec![Query::UuidIn { uuids: vec![a, c] }, eq("tag", s("a"))],
    });
}

#[test]
fn categorical_string_neq_multimap() {
    // {jazz, live} must match Neq("jazz") via the "live" row; {jazz} alone
    // must not. A type-mismatched operand differs from every row.
    let mut o = Oracle::new();
    o.create(vec![Field::new("tag", s("jazz")), Field::new("tag", s("live"))]);
    o.create(vec![Field::new("tag", s("jazz"))]);
    o.create(vec![Field::new("tag", s("blues"))]);

    o.check(&neq("tag", s("jazz")));
    o.check(&neq("tag", s("blues")));
    o.check(&neq("tag", s("rock")));
    o.check(&neq("tag", Value::Int(1))); // mismatched type: all differ
}

#[test]
fn categorical_string_ordered() {
    let mut o = Oracle::new();
    o.create(vec![Field::new("name", s("alice"))]);
    o.create(vec![Field::new("name", s("bob"))]);
    o.create(vec![Field::new("name", s("carol"))]);
    // multi-map: matches if ANY value satisfies
    o.create(vec![Field::new("name", s("aaron")), Field::new("name", s("zoe"))]);

    o.check(&lt("name", s("bob")));
    o.check(&lte("name", s("bob")));
    o.check(&gt("name", s("bob")));
    o.check(&gte("name", s("bob")));
}

// ── Categorical: bool ───────────────────────────────────────────────────────

#[test]
fn categorical_bool_eq_neq() {
    let mut o = Oracle::new();
    o.create(vec![Field::new("seen", Value::Bool(true))]);
    o.create(vec![Field::new("seen", Value::Bool(false))]);
    // multi-map both: matches Eq(true), Eq(false), and Neq(either)
    o.create(vec![Field::new("seen", Value::Bool(true)), Field::new("seen", Value::Bool(false))]);

    o.check(&eq("seen", Value::Bool(true)));
    o.check(&eq("seen", Value::Bool(false)));
    o.check(&neq("seen", Value::Bool(true)));
    o.check(&neq("seen", Value::Bool(false)));
}

// ── BSI: int / float / datetime ─────────────────────────────────────────────

fn i(n: i64) -> Value {
    Value::Int(n)
}

#[test]
fn bsi_int_ranges_multimap() {
    let mut o = Oracle::new();
    // multi-map {3,7}: 5 is strictly between min and max — Eq(5) must be empty,
    // yet Gte(5) and Lte(5) both match (max 7 ≥ 5, min 3 ≤ 5).
    o.create(vec![Field::new("rate", i(3)), Field::new("rate", i(7))]);
    o.create(vec![Field::new("rate", i(5))]);
    o.create(vec![Field::new("rate", i(10))]);
    o.create(vec![Field::new("rate", i(-4))]); // negative: order-preserving key
    o.create(vec![Field::new("other", i(1))]);

    for v in [-4, 0, 3, 5, 7, 10, 11] {
        o.check(&eq("rate", i(v)));
        o.check(&neq("rate", i(v)));
        o.check(&lt("rate", i(v)));
        o.check(&lte("rate", i(v)));
        o.check(&gt("rate", i(v)));
        o.check(&gte("rate", i(v)));
    }
}

#[test]
fn bsi_int_only_max_or_min_satisfies() {
    // {1, 100}: Gte(50) matches only via the max; Lte(50) only via the min.
    let mut o = Oracle::new();
    o.create(vec![Field::new("n", i(1)), Field::new("n", i(100))]);
    o.create(vec![Field::new("n", i(40))]);
    o.create(vec![Field::new("n", i(60))]);

    o.check(&gte("n", i(50)));
    o.check(&gt("n", i(50)));
    o.check(&lte("n", i(50)));
    o.check(&lt("n", i(50)));
}

#[test]
fn bsi_float_ranges() {
    let mut o = Oracle::new();
    o.create(vec![Field::new("score", Value::Float(2.5))]);
    o.create(vec![Field::new("score", Value::Float(-1.5)), Field::new("score", Value::Float(3.5))]);
    o.create(vec![Field::new("score", Value::Float(0.0))]);

    for v in [-1.5_f64, 0.0, 2.5, 3.0, 3.5] {
        o.check(&eq("score", Value::Float(v)));
        o.check(&neq("score", Value::Float(v)));
        o.check(&lt("score", Value::Float(v)));
        o.check(&gte("score", Value::Float(v)));
    }
    // Int operand against a float field compares numerically (f64 space).
    o.check(&gte("score", i(3)));
    o.check(&eq("score", i(0)));
}

#[test]
fn bsi_datetime_ranges() {
    let mut o = Oracle::new();
    o.create(vec![Field::new("added", dt("2024-01-01T00:00:00Z"))]);
    o.create(vec![Field::new("added", dt("2024-06-15T12:00:00Z"))]);
    o.create(vec![
        Field::new("added", dt("2023-01-01T00:00:00Z")),
        Field::new("added", dt("2025-01-01T00:00:00Z")),
    ]);

    let pivot = dt("2024-06-15T12:00:00Z");
    o.check(&eq("added", pivot.clone()));
    o.check(&neq("added", pivot.clone()));
    o.check(&lt("added", pivot.clone()));
    o.check(&lte("added", pivot.clone()));
    o.check(&gt("added", pivot.clone()));
    o.check(&gte("added", pivot));
    // Mismatched operand family: int vs a datetime field → empty / present.
    o.check(&eq("added", i(0)));
    o.check(&neq("added", i(0)));
}

// ── Boolean algebra ─────────────────────────────────────────────────────────

#[test]
fn boolean_and_or_not() {
    let mut o = Oracle::new();
    o.create(vec![Field::new("kind", s("film")), Field::new("rate", i(8))]);
    o.create(vec![Field::new("kind", s("film")), Field::new("rate", i(3))]);
    o.create(vec![Field::new("kind", s("book")), Field::new("rate", i(9))]);
    o.create(vec![Field::new("kind", s("book"))]); // no rate
    o.create(vec![Field::new("other", i(1))]);

    o.check(&and(vec![eq("kind", s("film")), gte("rate", i(5))]));
    o.check(&or(vec![eq("kind", s("book")), gte("rate", i(8))]));
    o.check(&not(eq("kind", s("film"))));
    // Not over a three-valued predicate: complement within the universe.
    o.check(&not(present("rate")));
    o.check(&not(unknown("rate")));
    // Nested.
    o.check(&and(vec![
        or(vec![eq("kind", s("film")), eq("kind", s("book"))]),
        not(lt("rate", i(8))),
    ]));
}

// ── Reverse: Ref ────────────────────────────────────────────────────────────

#[test]
fn reverse_ref_eq_neq_follows() {
    let mut o = Oracle::new();
    let target = o.create(vec![Field::new("name", s("target"))]);
    let other = o.create(vec![Field::new("name", s("other"))]);
    let r1 = o.create(vec![Field::new("author", Value::Ref(target))]);
    let _r2 = o.create(vec![Field::new("author", Value::Ref(target))]);
    let _r3 = o.create(vec![Field::new("author", Value::Ref(other))]);

    o.check(&eq("author", Value::Ref(target)));
    o.check(&neq("author", Value::Ref(target)));
    o.check(&eq("author", Value::Ref(r1))); // referenced by nobody → empty
    o.check(&follows("author", eq("name", s("target"))));
    o.check(&follows("author", eq("name", s("other"))));
    // Follows on a non-reference field is empty.
    o.check(&follows("name", eq("name", s("target"))));
}

// ── Reverse: TreeRef forest ─────────────────────────────────────────────────

/// root ─┬─ b ── c
///       └─ d
fn forest() -> (Oracle, [Uuid; 4]) {
    let mut o = Oracle::new();
    let root = o.create(vec![
        Field::new("tag", s("root")),
        Field::new("kind", s("dir")),
        Field::new("rate", i(1)),
        tref("loc", None, "root"),
    ]);
    let b = o.create(vec![
        Field::new("kind", s("dir")),
        Field::new("rate", i(7)),
        tref("loc", Some(root), "b"),
    ]);
    let c = o.create(vec![
        Field::new("kind", s("file")),
        Field::new("rate", i(9)),
        tref("loc", Some(b), "c"),
    ]);
    let d = o.create(vec![
        Field::new("kind", s("file")),
        Field::new("rate", i(3)),
        tref("loc", Some(root), "d"),
    ]);
    (o, [root, b, c, d])
}

#[test]
fn reverse_tree_eq_by_value_and_by_name() {
    let (mut o, [root, _b, _c, _d]) = forest();

    // Full TreeRef equality (parent + name).
    o.check(&eq("loc", Value::TreeRef { parent: Some(root), name: "b".into() }));
    o.check(&eq("loc", Value::TreeRef { parent: None, name: "root".into() }));
    // String operand compares the name component (any parent).
    o.check(&eq("loc", s("b")));
    o.check(&eq("loc", s("root")));
    o.check(&neq("loc", s("b")));
    // Mismatched: an int operand on a tree_ref field.
    o.check(&eq("loc", i(0)));
    o.check(&neq("loc", i(0)));
}

#[test]
fn reverse_tree_follows_direct() {
    let (mut o, [_root, _b, _c, _d]) = forest();
    o.check(&follows("loc", eq("tag", s("root")))); // direct children of root: b, d
    o.check(&follows("loc", eq("kind", s("file")))); // children of c,d (none) → empty
}

#[test]
fn reverse_tree_follows_transitive() {
    let (mut o, [_root, _b, _c, _d]) = forest();
    o.check(&follows_t("loc", eq("tag", s("root")))); // b, c, d
    // FollowsTransitive on a ref field has no descendants → empty.
    o.check(&follows_t("author", eq("tag", s("root"))));
    // Inclusive (`=>*`): the matching roots plus their descendants — parity with
    // the SQL engine, which adds the roots to the result.
    o.check(&follows_ti("loc", eq("tag", s("root")))); // root, b, c, d
    o.check(&follows_ti("loc", eq("kind", s("file")))); // matching leaves + their (no) descendants
    o.check(&follows_ti("author", eq("tag", s("root")))); // ref field → empty
}

#[test]
fn exact_node_path_equality_defers_to_sql_without_roots() {
    // On a tree_ref field, an Eq/Neq string operand containing '/' is an
    // exact-node match resolved through the tree cache — outside the index. With
    // no caller-resolved node it must report Unsupported so the route falls back
    // to SQL (rather than answer with the wrong value_name-based bitmap).
    let (o, _) = forest();
    let index = RepoIndex::build(&o.conn).unwrap();
    assert!(index.evaluate(&eq("loc", s("root/b"))).is_err());
    assert!(index.evaluate(&Query::Neq { field: "loc".into(), value: s("root/b") }).is_err());
    // A separator-free operand stays a value_name compare (index handles it).
    assert!(index.evaluate(&eq("loc", s("b"))).is_ok());
    // On a plain string field, '/' is literal equality — still the index's job.
    assert!(index.evaluate(&eq("tag", s("a/b"))).is_ok());
}

#[test]
fn exact_node_path_equality_matches_sql_with_node_roots() {
    // The shape a "find this one file" query takes (`mfr_path = "/a/b.txt"`).
    // Once the caller resolves the node through the tree cache and hands it in
    // as a node root, the index serves it — and must agree with the SQL engine,
    // including on a path that resolves to nothing and inside a boolean.
    let (mut o, [_root, _b, _c, _d]) = forest();
    for path in ["root/b", "root/b/c", "root/d", "root/nope", "nope/at/all"] {
        for q in [
            eq("loc", s(path)),
            Query::And { operands: vec![eq("loc", s(path)), eq("kind", s("file"))] },
            Query::Not { operand: Box::new(eq("loc", s(path))) },
        ] {
            // Resolve the node exactly as `run_query_filter` does.
            let mut targets = Vec::new();
            collect_node_paths(&q, &mut targets);
            assert!(!targets.is_empty(), "the collector must see the Eq in {q:?}");
            let mut roots = QueryRoots::new();
            for (field, target) in targets {
                let node = o.cache.resolve_path(&o.conn, &field, &target).unwrap();
                roots.node.insert((field, target), node);
            }
            let index = RepoIndex::build(&o.conn).unwrap();

            let (mut sql, _) =
                query_exec::execute(&o.conn, &mut o.cache, &q, &[], None, None).unwrap();
            let (mut got, _) =
                index.evaluate_page_with_roots(&q, &[], None, None, &roots).unwrap();
            sql.sort();
            got.sort();
            assert_eq!(got, sql, "exact-node divergence on {q:?}");

            assert_eq!(
                index.count_with_roots(&q, &roots).unwrap() as usize,
                query_exec::count(&o.conn, &mut o.cache, &q).unwrap(),
                "count divergence on {q:?}"
            );
        }
    }
}

#[test]
fn exact_node_path_inequality_matches_sql_with_node_roots() {
    // `Neq` on an exact-node path is *not* the complement of `Eq`: the SQL
    // engine compiles it as "at least one non-Nothing row that is not the Eq
    // match", so on a tree_ref field it is every path-bearing metarecord except
    // the node — a metarecord with no value for the field is in neither.
    let (mut o, [_root, _b, _c, _d]) = forest();
    // A metarecord with an explicit Nothing and one with no `loc` at all: both
    // are outside `Neq`, though they are in the universe (so a naive
    // `universe − Eq` would wrongly include them).
    let _nothing = o.create(vec![Field::new("loc", Value::Nothing)]);
    let _unrelated = o.create(vec![Field::new("kind", s("file"))]);

    for path in ["root/b", "root/b/c", "root/nope"] {
        for q in [
            Query::Neq { field: "loc".into(), value: s(path) },
            Query::And {
                operands: vec![
                    Query::Neq { field: "loc".into(), value: s(path) },
                    eq("kind", s("file")),
                ],
            },
        ] {
            let mut targets = Vec::new();
            collect_node_paths(&q, &mut targets);
            assert!(!targets.is_empty(), "the collector must see the Neq in {q:?}");
            let mut roots = QueryRoots::new();
            for (field, target) in targets {
                let node = o.cache.resolve_path(&o.conn, &field, &target).unwrap();
                roots.node.insert((field, target), node);
            }
            let index = RepoIndex::build(&o.conn).unwrap();

            let (mut sql, _) =
                query_exec::execute(&o.conn, &mut o.cache, &q, &[], None, None).unwrap();
            let (mut got, _) =
                index.evaluate_page_with_roots(&q, &[], None, None, &roots).unwrap();
            sql.sort();
            got.sort();
            assert_eq!(got, sql, "exact-node Neq divergence on {q:?}");

            assert_eq!(
                index.count_with_roots(&q, &roots).unwrap() as usize,
                query_exec::count(&o.conn, &mut o.cache, &q).unwrap(),
                "count divergence on {q:?}"
            );
        }
    }
    // Without a resolved node it still defers to SQL.
    let index = RepoIndex::build(&o.conn).unwrap();
    assert!(index.evaluate(&Query::Neq { field: "loc".into(), value: s("root/b") }).is_err());
}

#[test]
fn reverse_tree_follows_path_target_matches_sql() {
    // The path-target shape the GUI uses (`mfr_path ->* "/dir"`): the index
    // serves it once the caller resolves the path to its root through the tree
    // cache. Each path must agree with the SQL engine, and an unresolved path
    // (no roots supplied) must stay `Unsupported` so the route falls back.
    let (mut o, [_root, _b, _c, _d]) = forest();
    // shape: 0 = Follows, 1 = FollowsTransitive strict, 2 = FollowsTransitive
    // inclusive (`=>*`, the subtree including its root).
    for path in ["root", "root/b", "root/d", "root/nope"] {
        for shape in [0, 1, 2] {
            let target = FollowTarget::Path(path.to_string());
            let q = match shape {
                0 => Query::Follows { field: "loc".into(), target },
                1 => Query::FollowsTransitive { field: "loc".into(), target, inclusive: false },
                _ => Query::FollowsTransitive { field: "loc".into(), target, inclusive: true },
            };
            // Resolve the path root exactly as `run_query_filter` does.
            let mut roots = QueryRoots::new();
            if let Some(uuid) = o.cache.resolve_path(&o.conn, "loc", path).unwrap() {
                roots.path.insert(("loc".to_string(), path.to_string()), uuid);
            }
            let index = RepoIndex::build(&o.conn).unwrap();

            let (mut sql, _) =
                query_exec::execute(&o.conn, &mut o.cache, &q, &[], None, None).unwrap();
            let (mut got, _) = index.evaluate_page_with_roots(&q, &[], None, None, &roots).unwrap();
            sql.sort();
            got.sort();
            assert_eq!(got, sql, "path divergence on {q:?}");

            let sql_count = query_exec::count(&o.conn, &mut o.cache, &q).unwrap();
            assert_eq!(
                index.count_with_roots(&q, &roots).unwrap() as usize,
                sql_count,
                "count divergence on {q:?}"
            );

            // Without resolved roots the bitmap path defers to SQL.
            assert!(index.evaluate(&q).is_err(), "path target needs roots: {q:?}");
        }
    }
}

#[test]
fn keyset_pagination_over_path_target_with_sort() {
    // The GUI's real scenario: browse a subtree and paginate by a sort key, with
    // some descendants lacking the key (sort last). The index (path resolved via
    // the tree cache → PathRoots) must page identically to the SQL engine.
    let mut o = Oracle::new();
    let root = o.create(vec![tref("loc", None, "root")]);
    // Children of root with varied rate; `c` lacks rate (must sort last).
    o.create(vec![tref("loc", Some(root), "a"), Field::new("rate", i(5))]);
    o.create(vec![tref("loc", Some(root), "b"), Field::new("rate", i(2))]);
    o.create(vec![tref("loc", Some(root), "c")]); // no rate
    o.create(vec![tref("loc", Some(root), "d"), Field::new("rate", i(8))]);
    // A grandchild, so the transitive set is more than the direct children.
    let a_uuid = o.cache.resolve_path(&o.conn, "loc", "root/a").unwrap().unwrap();
    o.create(vec![tref("loc", Some(a_uuid), "deep"), Field::new("rate", i(9))]);

    let q = Query::FollowsTransitive {
        field: "loc".into(),
        target: FollowTarget::Path("root".into()),
        inclusive: false,
    };
    for &asc in &[true, false] {
        for &limit in &[1usize, 2, 3] {
            o.check_paginated_with_roots(&q, &[("rate", asc)], limit);
        }
    }
    // Multi-key: rate then loc-name, still over the filtered subtree.
    o.check_paginated_with_roots(&q, &[("rate", false), ("loc", true)], 2);
}

fn osm_path_q(field: &str, terms: &[&str]) -> Query {
    Query::Osm {
        field: field.into(),
        terms: terms.iter().map(|t| t.to_string()).collect(),
        mode: OsmMode::Path,
    }
}

#[test]
fn osm_path_empty_terms_matches_sql() {
    // A blank OSM path query (the search box emptied) matches every metarecord
    // with a path in the forest — the same set as `is_present`. The index must
    // serve it on its own, with no caller-resolved nodes: it used to defer to
    // SQL, which made "everything" the slowest query in the repository.
    let mut o = Oracle::new();
    let root = o.create(vec![tref("loc", None, "root")]);
    let sci = o.create(vec![tref("loc", Some(root), "science")]);
    let _file = o.create(vec![tref("loc", Some(sci), "ep.mkv")]);
    // A metarecord whose `loc` is explicitly Nothing, and one with no `loc` at
    // all: neither has a path, so neither matches.
    let _nothing = o.create(vec![Field::new("loc", Value::Nothing)]);
    let _unrelated = o.create(vec![Field::new("kind", s("file"))]);

    let q = osm_path_q("loc", &[]);
    let index = RepoIndex::build(&o.conn).unwrap();
    let (mut sql, _) = query_exec::execute(&o.conn, &mut o.cache, &q, &[], None, None).unwrap();
    let mut got = index.to_uuids(&index.evaluate(&q).unwrap());
    sql.sort();
    got.sort();
    assert_eq!(got, sql, "empty-terms osm path divergence");
    assert_eq!(
        index.count(&q).unwrap() as usize,
        query_exec::count(&o.conn, &mut o.cache, &q).unwrap(),
        "empty-terms osm path count divergence"
    );
    // It is exactly `is_present` on the field.
    assert_eq!(got, {
        let mut p = index.to_uuids(&index.evaluate(&Query::IsPresent { field: "loc".into() }).unwrap());
        p.sort();
        p
    });
}

#[test]
fn osm_path_single_term_matches_sql() {
    // A single-term OSM path ("every metarecord whose path contains the term")
    // is a union of subtrees, which the index expands from the nodes whose name
    // contains the term. It resolves those itself from the in-memory name map —
    // no caller-supplied roots, and *no minimum term length*: the one- and
    // two-character terms are the first keystrokes of every finder search, and
    // they used to be the slowest (no FTS trigram below three characters).
    let mut o = Oracle::new();
    let root = o.create(vec![tref("loc", None, "root")]);
    let sci = o.create(vec![tref("loc", Some(root), "science")]);
    let sub = o.create(vec![tref("loc", Some(sci), "fiction")]);
    let _file = o.create(vec![tref("loc", Some(sub), "ep.mkv")]);
    let _music = o.create(vec![tref("loc", Some(root), "music")]);
    // Case folding and a regex metacharacter in the term must behave like the
    // SQL `(?i)` + `regex::escape` convention.
    let _caps = o.create(vec![tref("loc", Some(root), "SCIENCE.and.Co")]);

    for term in ["s", "sc", "sci", "science", "SCI", "root", "fic", "nope", "mus", ".", "e.a"] {
        let q = osm_path_q("loc", &[term]);
        let index = RepoIndex::build(&o.conn).unwrap();

        let (mut sql, _) =
            query_exec::execute(&o.conn, &mut o.cache, &q, &[], None, None).unwrap();
        let mut got = index.to_uuids(&index.evaluate(&q).unwrap());
        sql.sort();
        got.sort();
        assert_eq!(got, sql, "osm path divergence on term {term:?}");

        let sql_count = query_exec::count(&o.conn, &mut o.cache, &q).unwrap();
        assert_eq!(
            index.count(&q).unwrap() as usize,
            sql_count,
            "osm path count divergence on term {term:?}"
        );
    }

    let index = RepoIndex::build(&o.conn).unwrap();
    // Multi-term OSM path (order-sensitive) is not accelerated — index defers.
    let multi = osm_path_q("loc", &["science", "fiction"]);
    assert!(
        index.evaluate_page_with_roots(&multi, &[], None, None, &QueryRoots::new()).is_err(),
        "multi-term osm path defers to SQL"
    );
}

#[test]
fn osm_path_separator_term_defers_and_matches_sql() {
    // `path = "music/jazz"` is *one* term containing the separator (the tag
    // syntax's anchored form). It can only match across segments, so no single
    // node name contains it: seeding the subtree expansion from name matches
    // would answer "nothing". The index must defer, and the leaf rewrite must
    // then produce the same set as the SQL engine.
    let mut o = Oracle::new();
    let root = o.create(vec![tref("loc", None, "root")]);
    let music = o.create(vec![tref("loc", Some(root), "music")]);
    let jazz = o.create(vec![tref("loc", Some(music), "jazz")]);
    let _track = o.create(vec![tref("loc", Some(jazz), "take-five.flac")]);
    let _other = o.create(vec![tref("loc", Some(root), "jazz")]);

    for term in ["music/jazz", "root/music", "zz/take"] {
        let q = osm_path_q("loc", &[term]);
        let index = RepoIndex::build(&o.conn).unwrap();
        assert!(index.evaluate(&q).is_err(), "a separator-bearing term must defer: {term:?}");

        let rewritten = query_exec::resolve_index_leaves(&o.conn, &mut o.cache, &q).unwrap();
        let (mut sql, _) = query_exec::execute(&o.conn, &mut o.cache, &q, &[], None, None).unwrap();
        let (mut got, _) =
            index.evaluate_page_with_roots(&rewritten, &[], None, None, &QueryRoots::new()).unwrap();
        sql.sort();
        got.sort();
        assert_eq!(got, sql, "separator-term divergence on {term:?}");
        assert!(!sql.is_empty(), "term {term:?} should match something");
    }
}

#[test]
fn osm_path_multi_term_via_leaf_rewrite_matches_sql() {
    // A multi-term OSM path is order-sensitive, so the index can't do it alone;
    // `resolve_index_leaves` pre-resolves it to a UuidIn (without the SQL VALUES
    // inlining) and the index composes. The set and count must match SQL, and the
    // ordered semantics must hold (a reversed term order matches nothing here).
    let mut o = Oracle::new();
    let root = o.create(vec![tref("loc", None, "root")]);
    let video = o.create(vec![tref("loc", Some(root), "video")]);
    let series = o.create(vec![tref("loc", Some(video), "series")]);
    let _scifi = o.create(vec![tref("loc", Some(series), "science-fiction")]);
    let _music = o.create(vec![tref("loc", Some(root), "music")]);

    for terms in [vec!["video", "scien"], vec!["scien", "video"], vec!["ser", "vid"]] {
        let q = osm_path_q("loc", &terms);
        let rewritten = query_exec::resolve_index_leaves(&o.conn, &mut o.cache, &q).unwrap();
        let index = RepoIndex::build(&o.conn).unwrap();
        let (mut sql, _) = query_exec::execute(&o.conn, &mut o.cache, &q, &[], None, None).unwrap();
        // A rewritten multi-term OSM path is a bare UuidIn — the index serves it
        // with no roots needed.
        let (mut got, _) =
            index.evaluate_page_with_roots(&rewritten, &[], None, None, &QueryRoots::new()).unwrap();
        sql.sort();
        got.sort();
        assert_eq!(got, sql, "multi-term osm path divergence on {terms:?}");
    }
}

#[test]
fn finder_shaped_query_via_leaf_rewrite_matches_sql() {
    // The GUI finder runs `or(osm_path(mfr_path), osmd(label), osmd(name))`. The
    // index can't do the `osmd` (Direct) leaves, but after `resolve_index_leaves`
    // rewrites them to UuidIn sets, the index serves the whole query — the
    // single-term osm_path natively — and must agree with the SQL engine.
    let mut o = Oracle::new();
    let root = o.create(vec![tref("loc", None, "root")]);
    let sci = o.create(vec![tref("loc", Some(root), "science")]);
    let _f = o.create(vec![tref("loc", Some(sci), "ep.mkv"), Field::new("label", s("plain"))]);
    // A record OUTSIDE the science subtree, matched only by the osmd(label) leaf.
    let _tagged = o.create(vec![tref("loc", Some(root), "misc"), Field::new("label", s("scifi"))]);

    let q = Query::Or {
        operands: vec![
            osm_path_q("loc", &["science"]),
            Query::Osm { field: "label".into(), terms: vec!["sci".into()], mode: OsmMode::Direct },
        ],
    };

    // Rewrite the index-unsupported osmd leaf, exactly as `run_query_filter`
    // does; the osm-path leaf needs no preparation, the index resolves its term
    // nodes itself.
    let rewritten = query_exec::resolve_index_leaves(&o.conn, &mut o.cache, &q).unwrap();
    let roots = QueryRoots::new();

    let index = RepoIndex::build(&o.conn).unwrap();
    let (mut sql, _) = query_exec::execute(&o.conn, &mut o.cache, &q, &[], None, None).unwrap();
    let (mut got, _) =
        index.evaluate_page_with_roots(&rewritten, &[], None, None, &roots).unwrap();
    sql.sort();
    got.sort();
    assert_eq!(got, sql, "finder-shaped query divergence");
    assert_eq!(
        index.count_with_roots(&rewritten, &roots).unwrap() as usize,
        query_exec::count(&o.conn, &mut o.cache, &q).unwrap(),
        "finder-shaped count divergence"
    );
}

#[test]
fn reverse_tree_transitive_conjunction() {
    // The spec's motivating shape: descendants ∧ value predicate ∧ category.
    let (mut o, [_root, _b, _c, _d]) = forest();
    o.check(&and(vec![
        follows_t("loc", eq("tag", s("root"))),
        gte("rate", i(5)),
        eq("kind", s("file")),
    ]));
}

// ── Sorting (ORDER BY) ──────────────────────────────────────────────────────

/// Five records all carrying `all`, with varied `rate` (incl. a multi-map and
/// a missing one) plus a `kind`, to exercise representative selection, ties,
/// and field-missing-last.
fn sortable() -> Oracle {
    let mut o = Oracle::new();
    let all = || Field::new("all", Value::Bool(true));
    o.create(vec![all(), Field::new("kind", s("film")), Field::new("rate", i(5))]);
    o.create(vec![all(), Field::new("kind", s("film")), Field::new("rate", i(2))]);
    o.create(vec![all(), Field::new("kind", s("book")), Field::new("rate", i(8))]);
    // multi-map rate {2, 9}: asc rep = 2 (ties with the i(2) record), desc rep = 9
    o.create(vec![all(), Field::new("kind", s("book")), Field::new("rate", i(2)), Field::new("rate", i(9))]);
    o.create(vec![all(), Field::new("kind", s("film")), Field::new("rate", Value::Nothing)]);
    o.create(vec![all(), Field::new("kind", s("book"))]); // no rate → sorts last
    o
}

#[test]
fn sort_single_key_int_asc_desc() {
    let mut o = sortable();
    o.check_sorted(&present("all"), &[("rate", true)], None);
    o.check_sorted(&present("all"), &[("rate", false)], None);
}

#[test]
fn sort_with_limit() {
    let mut o = sortable();
    o.check_sorted(&present("all"), &[("rate", false)], Some(3));
    o.check_sorted(&present("all"), &[("rate", true)], Some(2));
}

#[test]
fn sort_string_key() {
    let mut o = sortable();
    o.check_sorted(&present("all"), &[("kind", true)], None);
    o.check_sorted(&present("all"), &[("kind", false)], None);
}

#[test]
fn sort_multi_key() {
    let mut o = sortable();
    o.check_sorted(&present("all"), &[("kind", true), ("rate", false)], None);
    o.check_sorted(&present("all"), &[("kind", false), ("rate", true)], None);
}

#[test]
fn sort_datetime_latest_first() {
    // The motivating "latest modified files" query: descending datetime.
    let mut o = Oracle::new();
    o.create(vec![Field::new("all", Value::Bool(true)), Field::new("added", dt("2024-01-01T00:00:00Z"))]);
    o.create(vec![Field::new("all", Value::Bool(true)), Field::new("added", dt("2025-06-15T12:00:00Z"))]);
    o.create(vec![Field::new("all", Value::Bool(true)), Field::new("added", dt("2023-03-03T03:03:03Z"))]);
    o.check_sorted(&present("all"), &[("added", false)], Some(2));
    o.check_sorted(&present("all"), &[("added", true)], None);
}

// ── COUNT ───────────────────────────────────────────────────────────────────

#[test]
fn count_matches_sql() {
    let mut o = sortable();
    o.check_count(&present("all"));
    o.check_count(&present("rate"));
    o.check_count(&unknown("rate"));
    o.check_count(&and(vec![eq("kind", s("film")), gte("rate", i(3))]));
    o.check_count(&not(eq("kind", s("film"))));
    o.check_count(&eq("kind", s("nope"))); // zero
}

// ── Pagination ──────────────────────────────────────────────────────────────

#[test]
fn pagination_matches_sql_pages() {
    let mut o = sortable();
    // limits that do and do not divide the total
    for limit in [1usize, 2, 3, 100] {
        o.check_paginated(&present("all"), &[("rate", true)], limit);
        o.check_paginated(&present("all"), &[("rate", false)], limit);
        o.check_paginated(&present("all"), &[("kind", true), ("rate", false)], limit);
    }
}

#[test]
fn keyset_pagination_is_stable_under_insertion() {
    // Page through ascending rate; between pages insert a row that sorts BEFORE
    // the cursor. With keyset (not offset) the next page is unaffected — and it
    // matches the SQL engine, which is also keyset.
    let mut o = Oracle::new();
    for n in [10, 20, 30, 40, 50] {
        o.create(vec![Field::new("all", Value::Bool(true)), Field::new("rate", i(n))]);
    }
    let q = present("all");
    let idx_keys = [SortBy { field: "rate".into(), ascending: true }];
    let sql_keys = [SortKey { field: "rate".into(), order: SortOrder::Asc }];

    let index = RepoIndex::build(&o.conn).unwrap();
    let (_p1, icur) = index.evaluate_page(&q, &idx_keys, Some(2), None).unwrap();
    let (_s1, scur) = query_exec::execute(
        &o.conn, &mut o.cache, &q, &sql_keys, Some(2), None,
    )
    .unwrap();

    // Insert a row (rate 15) that sorts within the already-returned region.
    o.create(vec![Field::new("all", Value::Bool(true)), Field::new("rate", i(15))]);

    let index2 = RepoIndex::build(&o.conn).unwrap();
    let (ip2, _) = index2.evaluate_page(&q, &idx_keys, Some(2), icur.as_deref()).unwrap();
    let (sp2, _) = query_exec::execute(
        &o.conn, &mut o.cache, &q, &sql_keys, Some(2), scur.as_deref(),
    )
    .unwrap();

    // Both resume strictly after rate=20 → rates 30, 40 (never re-showing 15).
    assert_eq!(ip2, sp2, "index keyset page must match the SQL keyset page");
}

#[test]
fn cursor_is_bound_to_query_and_sort() {
    let o = sortable();
    let index = RepoIndex::build(&o.conn).unwrap();
    let by_rate = [SortBy { field: "rate".into(), ascending: true }];
    let (_p, next) = index.evaluate_page(&present("all"), &by_rate, Some(2), None).unwrap();
    let cursor = next.expect("more pages");
    // Reusing the cursor against a different sort is rejected.
    let by_kind = [SortBy { field: "kind".into(), ascending: true }];
    assert!(index.evaluate_page(&present("all"), &by_kind, Some(2), Some(&cursor)).is_err());
    // Against the original query+sort it is accepted.
    assert!(index.evaluate_page(&present("all"), &by_rate, Some(2), Some(&cursor)).is_ok());
}
