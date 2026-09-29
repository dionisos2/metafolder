//! Postings for frequent values on the key-value store (doc "Store tables", `postings`): a value
//! held by 64 records or more is answered from
//! one chunked bitmap instead of a key per holder. The query suites run on
//! repositories far too small to promote a value, so this file builds ones
//! that do — and holds every answer to the oracle, and the postings
//! themselves to a rebuild, across writes and the log's navigation.

use metafolder_core::metarecord::{Field, Value};
use metafolder_core::query::{Aspect, Query};
use metafolder_daemon::index::{PageStrategy, QueryRoots, SortBy};
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::{self, Writer};
use metafolder_daemon::query_result::{SortKey, SortOrder};
use metafolder_daemon::store::Log;
use metafolder_query_oracle as query_exec;
use uuid::Uuid;

mod common;
use common::TempDir;

/// A text too long to be keyed whole: its partition key is cut and hashed.
fn long_text() -> String {
    "long ".repeat(80)
}

/// 300 records: `kind` cycling over three values (and five of a rare one), a
/// `rating` over ten, a long text shared by seventy, a multi-valued `tag`,
/// and a `Nothing` here and there.
fn fields(i: usize) -> Vec<Field> {
    let kind = if i % 60 == 7 { "rare" } else { ["a", "b", "c"][i % 3] };
    let mut fields = vec![
        Field::new("kind", Value::String(kind.into())),
        Field::new("rating", Value::Int((i % 10) as i64)),
        Field::new("tag", Value::String("x".into())),
    ];
    if i.is_multiple_of(2) {
        fields.push(Field::new("tag", Value::String("y".into())));
    }
    if i.is_multiple_of(5) {
        fields.push(Field::new("tag", Value::String("x".into())));
    }
    if i < 70 {
        fields.push(Field::new("label", Value::String(long_text())));
    } else if i.is_multiple_of(7) {
        fields.push(Field::new("label", Value::Nothing));
    } else {
        fields.push(Field::new("label", Value::String(format!("{}!", long_text()))));
    }
    fields
}

fn string(s: &str) -> Value {
    Value::String(s.into())
}

fn eq(field: &str, value: Value) -> Query {
    Query::Eq { field: field.into(), value, aspect: Aspect::Raw }
}

fn neq(field: &str, value: Value) -> Query {
    Query::Neq { field: field.into(), value, aspect: Aspect::Raw }
}

fn not(q: Query) -> Query {
    Query::Not { operand: Box::new(q) }
}

#[test]
fn frequent_values_answer_as_the_oracle_does() {
    let (conn, _conn_dir) = common::kv::store();
    let mut first = None;
    {
        let mut conn = conn;
        let mut w = Writer::begin(&mut conn, None).unwrap();
        for i in 0..300 {
            let uuid = w.create_metarecord(fields(i)).unwrap().uuid;
            first.get_or_insert(uuid);
        }
        w.commit().unwrap();
        run_battery(&conn, first.unwrap());
    }
}

fn run_battery(conn: &KvStore, first: Uuid) {
    let int = |n: i64| Value::Int(n);
    let rating =
        |op: fn(String, Value, Aspect) -> Query, n: i64| op("rating".into(), int(n), Aspect::Raw);
    let gt = |f, v, a| Query::Gt { field: f, value: v, aspect: a };
    let lte = |f, v, a| Query::Lte { field: f, value: v, aspect: a };
    let battery = vec![
        eq("kind", string("a")),
        eq("kind", string("rare")),
        neq("kind", string("a")),
        not(eq("kind", string("a"))),
        eq("tag", string("x")),
        neq("tag", string("x")),
        eq("label", Value::String(long_text())),
        neq("label", Value::String(long_text())),
        rating(gt, 3),
        rating(lte, 3),
        Query::Gt { field: "kind".into(), value: string("a"), aspect: Aspect::Raw },
        Query::Lt { field: "kind".into(), value: string("c"), aspect: Aspect::Raw },
        Query::And { operands: vec![eq("kind", string("a")), rating(gt, 5)] },
        Query::And { operands: vec![not(eq("kind", string("b"))), eq("tag", string("y"))] },
        Query::Or { operands: vec![eq("kind", string("rare")), rating(lte, 0)] },
        Query::SameAs {
            field: "kind".into(),
            target: Box::new(Query::UuidIn { uuids: vec![first] }),
        },
        Query::SameAs {
            field: "label".into(),
            target: Box::new(Query::UuidIn { uuids: vec![first] }),
        },
    ];
    for q in &battery {
        let found = common::engines::both(conn, q, &[]);
        assert!(!found.is_empty() || matches!(q, Query::Neq { .. }), "{q:?} proves nothing");
    }
    // Sorted, page by page: a frequent value's holders are one long run of
    // ties, ordered by uuid across the pages.
    let sorted = [
        (Query::IsPresent { field: "rating".into(), aspect: Aspect::Raw }, "rating", false),
        (Query::IsPresent { field: "rating".into(), aspect: Aspect::Raw }, "rating", true),
        (not(eq("kind", string("b"))), "rating", false),
        (Query::IsPresent { field: "tag".into(), aspect: Aspect::Raw }, "tag", true),
        (Query::IsPresent { field: "tag".into(), aspect: Aspect::Raw }, "tag", false),
        (Query::IsPresent { field: "kind".into(), aspect: Aspect::Raw }, "kind", true),
    ];
    for (q, field, ascending) in &sorted {
        let order = if *ascending { SortOrder::Asc } else { SortOrder::Desc };
        let key = SortKey { field: field.to_string(), order };
        let (want, _) =
            query_exec::execute(conn, q, std::slice::from_ref(&key), None, None).unwrap();
        for strategy in [PageStrategy::Auto, PageStrategy::Walk] {
            let by = [SortBy { field: field.to_string(), ascending: *ascending }];
            let mut got = Vec::new();
            let mut cursor: Option<String> = None;
            loop {
                let (page, next) = common::kv::with_kv(conn, strategy, |e, _| {
                    e.evaluate_page_with_roots(
                        q,
                        &by,
                        Some(7),
                        cursor.as_deref(),
                        &QueryRoots::new(),
                    )
                    .unwrap()
                });
                got.extend(page);
                match next {
                    Some(c) => cursor = Some(c),
                    None => break,
                }
            }
            assert_eq!(got, want, "{strategy:?} pages of {q:?} by {field} ({order:?})");
        }
    }
}

fn open() -> (KvStore, TempDir) {
    let dir = TempDir::new("kv-postings");
    (KvStore::open(dir.path()).unwrap(), dir)
}

fn check(store: &KvStore) {
    let diff = store.check_derived().unwrap();
    assert!(diff.is_empty(), "derived data diverges from a rebuild:\n{}", diff.join("\n"));
}

/// How many records `kind = value` matches on the store (counted, with a
/// page of one), and the keys that read.
fn holders(store: &KvStore, value: &str) -> (usize, u64) {
    let before = store.reads();
    let roots = QueryRoots::new();
    let found = common::kv::with_kv(store, PageStrategy::Auto, |e, _| {
        e.page_and_count(&eq("kind", string(value)), &[], Some(1), None, &roots).unwrap().2
    });
    (found as usize, store.reads() - before)
}

#[test]
fn a_posting_follows_writes_and_navigation() {
    let (mut store, _dir) = open();
    let start = store.head().unwrap();
    let mut w = Writer::begin(&mut store, None).unwrap();
    let uuids: Vec<Uuid> = (0..200)
        .map(|_| w.create_metarecord(vec![Field::new("kind", string("a"))]).unwrap().uuid)
        .collect();
    w.commit().unwrap();
    check(&store);
    let (found, reads) = holders(&store, "a");
    assert_eq!(found, 200);
    assert!(reads < 20, "a promoted value read {reads} keys for 200 holders");
    let full = store.head().unwrap();

    // Down below the threshold, then to nothing: the posting stays exact.
    let mut w = Writer::begin(&mut store, None).unwrap();
    for &u in &uuids[..150] {
        w.set_field(u, "kind", string("b")).unwrap();
    }
    w.commit().unwrap();
    check(&store);
    assert_eq!(holders(&store, "a").0, 50);
    assert_eq!(holders(&store, "b").0, 150);
    let mut w = Writer::begin(&mut store, None).unwrap();
    for &u in &uuids[150..] {
        w.delete_fields_named(u, "kind").unwrap();
    }
    w.commit().unwrap();
    check(&store);
    assert_eq!(holders(&store, "a").0, 0);

    // The log's navigation writes by the same primitives.
    log::navigate(&mut store, full).unwrap();
    check(&store);
    assert_eq!(holders(&store, "a").0, 200);
    log::navigate(&mut store, start).unwrap();
    check(&store);
    assert_eq!(holders(&store, "a").0, 0);
    log::navigate(&mut store, full).unwrap();
    store.reindex().unwrap();
    check(&store);
    assert_eq!(holders(&store, "a").0, 200);
}
