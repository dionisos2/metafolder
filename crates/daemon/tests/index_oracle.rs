//! Equivalence oracle for the in-memory bitmap index (spec-indexing.org).
//!
//! Every query in the battery is run through BOTH the SQL engine
//! (`query_exec::execute`, the oracle) and `RepoIndex::evaluate`, asserting an
//! identical result *set* (order is irrelevant — sorting is a later increment).
//! Fixtures are crafted to exercise the correctness pitfalls: present/absent
//! overlap, multi-map min/max, the exclusively-owned universe, ZERO_UUID tree
//! roots.

use metafolder_core::metarecord::{Field, Value};
use metafolder_core::query::{FollowTarget, Query};
use metafolder_daemon::db;
use metafolder_daemon::index::{collect_path_targets, PathRoots, RepoIndex, SortBy};
use metafolder_daemon::log::Writer;
use metafolder_daemon::query_exec::{self, SortKey, SortOrder};
use metafolder_daemon::tree_cache::TreeCache;
use rusqlite::Connection;
use uuid::Uuid;

struct Oracle {
    conn: Connection,
    cache: TreeCache,
    db_id: Uuid,
}

impl Oracle {
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

    /// Makes `uuid` a link metarecord by adding a second owning repository, so
    /// it drops out of the exclusively-owned universe. A direct insert (no log)
    /// suffices for the fixture: both engines read `metarecord_db` directly.
    fn add_owner(&mut self, uuid: Uuid, db_id: Uuid) {
        self.conn
            .execute(
                "INSERT INTO metarecord_db (metarecord_uuid, db_id) VALUES (?1, ?2)",
                rusqlite::params![db::uuid_to_bytes(uuid), db::uuid_to_bytes(db_id)],
            )
            .unwrap();
    }

    /// Asserts the bitmap index agrees with the SQL engine on `q`.
    fn check(&mut self, q: &Query) {
        let index = RepoIndex::build(&self.conn, self.db_id).unwrap();
        let (mut sql, _) =
            query_exec::execute(&self.conn, &mut self.cache, self.db_id, q, &[], None, None)
                .unwrap();
        let mut got = index.to_uuids(&index.evaluate(q).unwrap());
        sql.sort();
        got.sort();
        assert_eq!(got, sql, "divergence on {q:?}");
    }

    /// Asserts the bitmap index agrees with the SQL engine on the *ordered*,
    /// limited result of `q` (comparison is order-sensitive — a `Vec`, not a set).
    fn check_sorted(&mut self, q: &Query, by: &[(&str, bool)], limit: Option<usize>) {
        let index = RepoIndex::build(&self.conn, self.db_id).unwrap();
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
            self.db_id,
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
        let index = RepoIndex::build(&self.conn, self.db_id).unwrap();
        let sql = query_exec::count(&self.conn, &mut self.cache, self.db_id, q).unwrap();
        assert_eq!(index.count(q).unwrap() as usize, sql, "count divergence on {q:?}");
    }

    /// Asserts the in-memory field catalog agrees with the SQL
    /// `distinct_field_names` — unfiltered and for each value type present.
    fn check_catalog(&mut self) {
        let index = RepoIndex::build(&self.conn, self.db_id).unwrap();
        let sql = db::distinct_field_names(&self.conn, self.db_id, None).unwrap();
        assert_eq!(index.field_catalog(None), sql, "catalog divergence (unfiltered)");
        let types: std::collections::BTreeSet<&str> = sql.iter().map(|(_, t)| t.as_str()).collect();
        for ty in types {
            let sql = db::distinct_field_names(&self.conn, self.db_id, Some(ty)).unwrap();
            assert_eq!(index.field_catalog(Some(ty)), sql, "catalog divergence (?type={ty})");
        }
    }

    /// Walks both engines page by page through the whole sorted result and
    /// asserts every page (and thus the partitioning) is identical.
    fn check_paginated(&mut self, q: &Query, by: &[(&str, bool)], limit: usize) {
        let index = RepoIndex::build(&self.conn, self.db_id).unwrap();
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
                self.db_id,
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
        let index = RepoIndex::build(&self.conn, self.db_id).unwrap();
        let mut targets = Vec::new();
        collect_path_targets(q, &mut targets);
        let mut roots = PathRoots::new();
        for (field, path) in targets {
            if let Some(uuid) = self.cache.resolve_path(&self.conn, &field, &path).unwrap() {
                roots.insert((field, path), uuid);
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
                self.db_id,
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
    Query::FollowsTransitive { field: field.into(), target: FollowTarget::Condition(Box::new(cond)) }
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

#[test]
fn universe_excludes_link_metarecords() {
    // A link metarecord (two owners) is outside the exclusively-owned universe,
    // so it must never appear — including in IsUnknown's complement.
    let mut o = Oracle::new();
    let own = o.create(vec![Field::new("rating", Value::Int(5))]);
    let link = o.create(vec![Field::new("rating", Value::Int(7))]);
    o.add_owner(link, Uuid::new_v4());
    let _ = (own, link);

    o.check(&present("rating"));
    o.check(&absent("rating"));
    o.check(&unknown("rating"));
    o.check(&unknown("never_used"));
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
    // A link metarecord introducing a field name absent from the owned set: it
    // must not leak into the catalog (universe isolation).
    let link = o.create(vec![Field::new("link_only", s("x"))]);
    o.add_owner(link, Uuid::new_v4());

    o.check_catalog();
}

#[test]
fn field_catalog_drops_field_when_last_value_removed() {
    // After *incremental* maintenance empties a field's `present` bitmap, the
    // name must disappear from the catalog (recompute_field empties the bitmap
    // but keeps the key, so the catalog must gate on non-emptiness).
    let mut o = Oracle::new();
    let m = o.create(vec![Field::new("rating", Value::Int(5))]);
    let mut index = RepoIndex::build(&o.conn, o.db_id).unwrap();
    assert_eq!(index.field_catalog(None), vec![("rating".to_string(), "int".to_string())]);

    let mut w = Writer::begin(&mut o.conn, o.db_id, None).unwrap();
    w.set_field(m, "rating", Value::Nothing).unwrap();
    w.commit().unwrap();
    index.refresh(&o.conn, o.db_id).unwrap();

    let sql = db::distinct_field_names(&o.conn, o.db_id, None).unwrap();
    assert!(sql.is_empty(), "SQL reference no longer lists the field");
    assert_eq!(index.field_catalog(None), sql, "catalog must drop the emptied field");
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
}

#[test]
fn reverse_tree_follows_path_target_matches_sql() {
    // The path-target shape the GUI uses (`mfr_path ->* "/dir"`): the index
    // serves it once the caller resolves the path to its root through the tree
    // cache. Each path must agree with the SQL engine, and an unresolved path
    // (no roots supplied) must stay `Unsupported` so the route falls back.
    let (mut o, [_root, _b, _c, _d]) = forest();
    for path in ["root", "root/b", "root/d", "root/nope"] {
        for transitive in [true, false] {
            let target = FollowTarget::Path(path.to_string());
            let q = if transitive {
                Query::FollowsTransitive { field: "loc".into(), target }
            } else {
                Query::Follows { field: "loc".into(), target }
            };
            // Resolve the path root exactly as `run_query_filter` does.
            let mut roots = metafolder_daemon::index::PathRoots::new();
            if let Some(uuid) = o.cache.resolve_path(&o.conn, "loc", path).unwrap() {
                roots.insert(("loc".to_string(), path.to_string()), uuid);
            }
            let index = RepoIndex::build(&o.conn, o.db_id).unwrap();

            let (mut sql, _) =
                query_exec::execute(&o.conn, &mut o.cache, o.db_id, &q, &[], None, None).unwrap();
            let (mut got, _) = index.evaluate_page_with_roots(&q, &[], None, None, &roots).unwrap();
            sql.sort();
            got.sort();
            assert_eq!(got, sql, "path divergence on {q:?}");

            let sql_count = query_exec::count(&o.conn, &mut o.cache, o.db_id, &q).unwrap();
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
    };
    for &asc in &[true, false] {
        for &limit in &[1usize, 2, 3] {
            o.check_paginated_with_roots(&q, &[("rate", asc)], limit);
        }
    }
    // Multi-key: rate then loc-name, still over the filtered subtree.
    o.check_paginated_with_roots(&q, &[("rate", false), ("loc", true)], 2);
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

    let index = RepoIndex::build(&o.conn, o.db_id).unwrap();
    let (_p1, icur) = index.evaluate_page(&q, &idx_keys, Some(2), None).unwrap();
    let (_s1, scur) = query_exec::execute(
        &o.conn, &mut o.cache, o.db_id, &q, &sql_keys, Some(2), None,
    )
    .unwrap();

    // Insert a row (rate 15) that sorts within the already-returned region.
    o.create(vec![Field::new("all", Value::Bool(true)), Field::new("rate", i(15))]);

    let index2 = RepoIndex::build(&o.conn, o.db_id).unwrap();
    let (ip2, _) = index2.evaluate_page(&q, &idx_keys, Some(2), icur.as_deref()).unwrap();
    let (sp2, _) = query_exec::execute(
        &o.conn, &mut o.cache, o.db_id, &q, &sql_keys, Some(2), scur.as_deref(),
    )
    .unwrap();

    // Both resume strictly after rate=20 → rates 30, 40 (never re-showing 15).
    assert_eq!(ip2, sp2, "index keyset page must match the SQL keyset page");
}

#[test]
fn cursor_is_bound_to_query_and_sort() {
    let o = sortable();
    let index = RepoIndex::build(&o.conn, o.db_id).unwrap();
    let by_rate = [SortBy { field: "rate".into(), ascending: true }];
    let (_p, next) = index.evaluate_page(&present("all"), &by_rate, Some(2), None).unwrap();
    let cursor = next.expect("more pages");
    // Reusing the cursor against a different sort is rejected.
    let by_kind = [SortBy { field: "kind".into(), ascending: true }];
    assert!(index.evaluate_page(&present("all"), &by_kind, Some(2), Some(&cursor)).is_err());
    // Against the original query+sort it is accepted.
    assert!(index.evaluate_page(&present("all"), &by_rate, Some(2), Some(&cursor)).is_ok());
}
