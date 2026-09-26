//! Cost assertions for the key-value store's query source (spec-storage
//! "Testing": scale is tested by counting, not timing). The same query runs
//! on a repository and on one eight times larger; a query whose answer does
//! not grow must not read more keys. Counting keys cannot flake under load,
//! and needs no large data set.

use metafolder_core::metarecord::{Field, Value};
use metafolder_core::query::{Aspect, OsmMode, Query};
use metafolder_daemon::index::{Eval, PageStrategy, QueryRoots, SortBy};
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::Writer;
use uuid::Uuid;

mod common;
use common::TempDir;

/// `n` files named `file000000.txt`…, a hundred per directory; a `kind` of
/// three common values, and ten files of a rare one.
fn repository(n: usize) -> (KvStore, TempDir) {
    let dir = TempDir::new("kv-cost");
    let mut kv = KvStore::open(dir.path()).unwrap();
    let mut w = Writer::begin(&mut kv, None).unwrap();
    let tree = |parent: Option<Uuid>, name: &str| {
        Field::new("loc", Value::TreeRef { parent, name: name.into() })
    };
    let root = w.create_metarecord(vec![tree(None, "")]).unwrap().uuid;
    let mut folder = root;
    for i in 0..n {
        if i % 100 == 0 {
            folder = w.create_metarecord(vec![tree(Some(root), &format!("d{i}"))]).unwrap().uuid;
        }
        let kind = if i % (n / 10) == 7 { "rare" } else { ["note", "photo", "song"][i % 3] };
        w.create_metarecord(vec![
            tree(Some(folder), &format!("file{i:06}.txt")),
            Field::new("name", Value::String(format!("file{i:06}.txt"))),
            Field::new("kind", Value::String(kind.into())),
        ])
        .unwrap();
    }
    w.commit().unwrap();
    (kv, dir)
}

/// The keys one page of `q` reads (sorted on `sort`, counted when `count`).
fn reads(kv: &KvStore, q: &Query, sort: &[(&str, bool)], count: bool) -> (usize, u64) {
    let src = kv.source().unwrap();
    let e = Eval { src: &src, strategy: PageStrategy::Auto };
    let sort: Vec<SortBy> =
        sort.iter().map(|(f, asc)| SortBy { field: f.to_string(), ascending: *asc }).collect();
    let roots = QueryRoots::new();
    let found = if count {
        e.page_and_count(q, &sort, Some(50), None, &roots).unwrap().2 as usize
    } else {
        e.evaluate_page_with_roots(q, &sort, Some(50), None, &roots).unwrap().0.len()
    };
    assert!(src.take_error().is_none());
    (found, src.reads())
}

/// Runs `q` on both sizes and asserts the larger reads no more than a
/// little over what the smaller reads.
fn bounded(what: &str, q: &Query, sort: &[(&str, bool)], count: bool) {
    let (small, _s) = repository(2_000);
    let (large, _l) = repository(16_000);
    let (found_small, cost_small) = reads(&small, q, sort, count);
    let (found_large, cost_large) = reads(&large, q, sort, count);
    assert_eq!(found_small, found_large, "{what}: the answer should not grow");
    assert!(
        cost_large <= cost_small + cost_small / 2 + 20,
        "{what}: {cost_small} keys on 2 000 records, {cost_large} on 16 000"
    );
}

fn eq(field: &str, value: &str) -> Query {
    Query::Eq { field: field.into(), value: Value::String(value.into()), aspect: Aspect::Raw }
}

fn present(field: &str) -> Query {
    Query::IsPresent { field: field.into(), aspect: Aspect::Raw }
}

fn matches(field: &str, pattern: &str) -> Query {
    Query::Matches { field: field.into(), pattern: pattern.into(), aspect: Aspect::Raw }
}

#[test]
fn a_rare_value_reads_its_own_rows() {
    bounded("rare value", &eq("kind", "rare"), &[], false);
    bounded("rare value, counted", &eq("kind", "rare"), &[], true);
}

#[test]
fn a_page_sorted_on_a_value_reads_the_page() {
    bounded("first page by name", &present("name"), &[("name", true)], false);
    bounded("last names first", &present("name"), &[("name", false)], false);
}

#[test]
fn a_text_search_reads_its_candidates_not_the_field() {
    // One name matches on either size.
    bounded("literal search", &matches("name", "file000123"), &[], false);
    bounded("literal search, counted", &matches("name", "file000123"), &[], true);
    let osm = Query::Osm {
        field: "name".into(),
        terms: vec!["000123".into(), "txt".into()],
        mode: OsmMode::Direct,
    };
    bounded("osm search, counted", &osm, &[], true);
}
