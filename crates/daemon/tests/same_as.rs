//! Integration tests for the `SameAs` query operator (spec-query "Same-value
//! matching"): the metarecords sharing a value of a field with a metarecord
//! matching the sub-query, without the caller naming the value.

use metafolder_core::metarecord::{Field, Value};
use metafolder_core::query::Query;
use metafolder_daemon::db;
use metafolder_daemon::log::Writer;
use metafolder_daemon::query_exec;
use metafolder_daemon::tree_cache::TreeCache;
use rusqlite::Connection;
use uuid::Uuid;

struct Fixture {
    conn: Connection,
    cache: TreeCache,
}

impl Fixture {
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

    fn run(&mut self, query: &Query) -> Vec<Uuid> {
        let (uuids, _) =
            query_exec::execute(&self.conn, &mut self.cache, query, &[], None, None).unwrap();
        uuids
    }
}

fn same(field: &str, target: Query) -> Query {
    Query::SameAs { field: field.into(), target: Box::new(target) }
}

fn only(uuid: Uuid) -> Query {
    Query::UuidIn { uuids: vec![uuid] }
}

fn s(text: &str) -> Value {
    Value::String(text.to_string())
}

fn assert_same_set(mut got: Vec<Uuid>, mut expected: Vec<Uuid>) {
    got.sort();
    expected.sort();
    assert_eq!(got, expected);
}

// ── one case per value type ─────────────────────────────────────────────────

#[test]
fn same_string_values() {
    let mut f = Fixture::new();
    let a = f.create(vec![Field::new("artist", s("Coltrane"))]);
    let b = f.create(vec![Field::new("artist", s("Coltrane"))]);
    let _c = f.create(vec![Field::new("artist", s("Davis"))]);
    assert_same_set(f.run(&same("artist", only(a))), vec![a, b]);
}

#[test]
fn same_int_values() {
    let mut f = Fixture::new();
    let a = f.create(vec![Field::new("mfr_size", Value::Int(4096))]);
    let b = f.create(vec![Field::new("mfr_size", Value::Int(4096))]);
    let _c = f.create(vec![Field::new("mfr_size", Value::Int(8192))]);
    assert_same_set(f.run(&same("mfr_size", only(a))), vec![a, b]);
}

#[test]
fn same_float_values() {
    let mut f = Fixture::new();
    let a = f.create(vec![Field::new("score", Value::Float(0.5))]);
    let b = f.create(vec![Field::new("score", Value::Float(0.5))]);
    let _c = f.create(vec![Field::new("score", Value::Float(0.75))]);
    assert_same_set(f.run(&same("score", only(a))), vec![a, b]);
}

#[test]
fn same_bool_values() {
    let mut f = Fixture::new();
    let a = f.create(vec![Field::new("seen", Value::Bool(true))]);
    let b = f.create(vec![Field::new("seen", Value::Bool(true))]);
    let _c = f.create(vec![Field::new("seen", Value::Bool(false))]);
    assert_same_set(f.run(&same("seen", only(a))), vec![a, b]);
}

#[test]
fn same_datetime_values() {
    let mut f = Fixture::new();
    let a = f.create(vec![Field::new("when", Value::DateTime(1_700_000_000_000))]);
    let b = f.create(vec![Field::new("when", Value::DateTime(1_700_000_000_000))]);
    let _c = f.create(vec![Field::new("when", Value::DateTime(1_600_000_000_000))]);
    assert_same_set(f.run(&same("when", only(a))), vec![a, b]);
}

#[test]
fn same_ref_values() {
    // The duplicate-group case: two files pointing at one group metarecord.
    let mut f = Fixture::new();
    let group = f.create(vec![Field::new("mf_schema", s("duplicate_group"))]);
    let other = f.create(vec![Field::new("mf_schema", s("duplicate_group"))]);
    let a = f.create(vec![Field::new("mfr_duplicate_group", Value::Ref(group))]);
    let b = f.create(vec![Field::new("mfr_duplicate_group", Value::Ref(group))]);
    let _c = f.create(vec![Field::new("mfr_duplicate_group", Value::Ref(other))]);
    assert_same_set(f.run(&same("mfr_duplicate_group", only(a))), vec![a, b]);
}

#[test]
fn same_tree_ref_compares_parent_and_name_together() {
    // A TreeRef forest is per field name, and a position is UNIQUE within it,
    // so two records can never hold the *same* TreeRef value: `same` on such a
    // field is necessarily just the target itself. What the test pins is that
    // it is *only* that — a sibling (same parent, other name) and a namesake
    // under another parent must not creep in, which they would if the
    // comparison looked at `value_uuid` or `value_name` alone.
    let mut f = Fixture::new();
    let one =
        f.create(vec![Field::new("loc", Value::TreeRef { parent: None, name: "one".into() })]);
    let two =
        f.create(vec![Field::new("loc", Value::TreeRef { parent: None, name: "two".into() })]);
    let a =
        f.create(vec![Field::new("loc", Value::TreeRef { parent: Some(one), name: "x".into() })]);
    let _sibling =
        f.create(vec![Field::new("loc", Value::TreeRef { parent: Some(one), name: "y".into() })]);
    let _namesake =
        f.create(vec![Field::new("loc", Value::TreeRef { parent: Some(two), name: "x".into() })]);
    assert_same_set(f.run(&same("loc", only(a))), vec![a]);
}

#[test]
fn same_external_ref_values_need_both_repo_and_metarecord() {
    let mut f = Fixture::new();
    let repo_one = Uuid::from_u128(1);
    let repo_two = Uuid::from_u128(2);
    let target = Uuid::from_u128(3);
    let ext = |repo: Uuid, metarecord: Uuid| Value::ExternalRef { repo, metarecord };
    let a = f.create(vec![Field::new("link", ext(repo_one, target))]);
    let b = f.create(vec![Field::new("link", ext(repo_one, target))]);
    let _c = f.create(vec![Field::new("link", ext(repo_two, target))]);
    let _d = f.create(vec![Field::new("link", ext(repo_one, Uuid::from_u128(4)))]);
    assert_same_set(f.run(&same("link", only(a))), vec![a, b]);
}

// ── semantics ───────────────────────────────────────────────────────────────

#[test]
fn same_is_reflexive() {
    // A record with no twin still matches itself: this is the equivalence
    // class, not the neighbourhood.
    let mut f = Fixture::new();
    let a = f.create(vec![Field::new("artist", s("Solo"))]);
    assert_same_set(f.run(&same("artist", only(a))), vec![a]);
}

#[test]
fn same_and_not_gives_the_others() {
    let mut f = Fixture::new();
    let a = f.create(vec![Field::new("artist", s("Coltrane"))]);
    let b = f.create(vec![Field::new("artist", s("Coltrane"))]);
    let q = Query::And {
        operands: vec![same("artist", only(a)), Query::Not { operand: Box::new(only(a)) }],
    };
    assert_same_set(f.run(&q), vec![b]);
}

#[test]
fn nothing_rows_never_participate() {
    // "The same absence" is not a resemblance — the same exclusion IsPresent
    // makes.
    let mut f = Fixture::new();
    let a = f.create(vec![Field::new("artist", Value::Nothing)]);
    let _b = f.create(vec![Field::new("artist", Value::Nothing)]);
    assert_same_set(f.run(&same("artist", only(a))), vec![]);
}

#[test]
fn a_target_without_the_field_contributes_nothing() {
    let mut f = Fixture::new();
    let bare = f.create(vec![Field::new("title", s("no artist here"))]);
    let _a = f.create(vec![Field::new("artist", s("Coltrane"))]);
    assert_same_set(f.run(&same("artist", only(bare))), vec![]);
}

#[test]
fn an_empty_target_matches_nothing() {
    let mut f = Fixture::new();
    let _a = f.create(vec![Field::new("artist", s("Coltrane"))]);
    let q = same("artist", Query::UuidIn { uuids: vec![] });
    assert_same_set(f.run(&q), vec![]);
}

#[test]
fn an_unknown_field_matches_nothing() {
    let mut f = Fixture::new();
    let a = f.create(vec![Field::new("artist", s("Coltrane"))]);
    assert_same_set(f.run(&same("never_written", only(a))), vec![]);
}

#[test]
fn a_multi_valued_field_matches_on_any_shared_value() {
    // Fields are multi-maps: one shared value out of several is enough.
    let mut f = Fixture::new();
    let a = f.create(vec![Field::new("tag", s("jazz")), Field::new("tag", s("live"))]);
    let b = f.create(vec![Field::new("tag", s("live")), Field::new("tag", s("studio"))]);
    let _c = f.create(vec![Field::new("tag", s("rock"))]);
    assert_same_set(f.run(&same("tag", only(a))), vec![a, b]);
}

#[test]
fn a_multi_record_target_unions_its_values() {
    let mut f = Fixture::new();
    let a = f.create(vec![Field::new("artist", s("Coltrane"))]);
    let b = f.create(vec![Field::new("artist", s("Davis"))]);
    let c = f.create(vec![Field::new("artist", s("Coltrane"))]);
    let d = f.create(vec![Field::new("artist", s("Davis"))]);
    let _e = f.create(vec![Field::new("artist", s("Monk"))]);
    let q = same("artist", Query::UuidIn { uuids: vec![a, b] });
    assert_same_set(f.run(&q), vec![a, b, c, d]);
}

// ── composition ─────────────────────────────────────────────────────────────

#[test]
fn same_takes_an_arbitrary_sub_query_as_target() {
    let mut f = Fixture::new();
    let a = f.create(vec![Field::new("artist", s("Coltrane")), Field::new("title", s("Naima"))]);
    let b = f.create(vec![Field::new("artist", s("Coltrane"))]);
    let _c = f.create(vec![Field::new("artist", s("Davis"))]);
    let q = same("artist", Query::Eq { field: "title".into(), value: s("Naima") });
    assert_same_set(f.run(&q), vec![a, b]);
}

#[test]
fn same_composes_inside_and_or_not() {
    let mut f = Fixture::new();
    let a =
        f.create(vec![Field::new("artist", s("Coltrane")), Field::new("rating", Value::Int(5))]);
    let b =
        f.create(vec![Field::new("artist", s("Coltrane")), Field::new("rating", Value::Int(2))]);
    let c = f.create(vec![Field::new("artist", s("Davis")), Field::new("rating", Value::Int(5))]);

    let peers = same("artist", only(a));
    assert_same_set(
        f.run(&Query::And {
            operands: vec![
                peers.clone(),
                Query::Gt { field: "rating".into(), value: Value::Int(3) },
            ],
        }),
        vec![a],
    );
    assert_same_set(f.run(&Query::Or { operands: vec![peers.clone(), only(c)] }), vec![a, b, c]);
    assert_same_set(f.run(&Query::Not { operand: Box::new(peers) }), vec![c]);
}

#[test]
fn same_serves_as_a_follows_target() {
    let mut f = Fixture::new();
    let group = f.create(vec![Field::new("mf_schema", s("duplicate_group"))]);
    let a = f.create(vec![Field::new("mfr_duplicate_group", Value::Ref(group))]);
    let b = f.create(vec![Field::new("mfr_duplicate_group", Value::Ref(group))]);
    // "The groups whose members include a peer of `a`" — a follows over a same.
    let q = Query::Follows {
        field: "mfr_duplicate_group".into(),
        target: metafolder_core::query::FollowTarget::Condition(Box::new(Query::UuidIn {
            uuids: vec![group],
        })),
    };
    assert_same_set(f.run(&q), vec![a, b]);
}

#[test]
fn the_dsl_spelling_matches_the_ir() {
    let mut f = Fixture::new();
    let a = f.create(vec![Field::new("artist", s("Coltrane")), Field::new("title", s("Naima"))]);
    let b = f.create(vec![Field::new("artist", s("Coltrane"))]);
    let _c = f.create(vec![Field::new("artist", s("Davis"))]);
    let q = metafolder_core::dsl::parse_query(r#"same(artist, title = "Naima")"#).unwrap();
    assert_same_set(f.run(&q), vec![a, b]);
}
