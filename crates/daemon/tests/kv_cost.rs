//! Cost assertions for the key-value store's query source (spec-storage
//! "Testing": scale is tested by counting, not timing). The same query runs
//! on a repository and on one eight times larger; a query whose answer does
//! not grow must not read more keys. Counting keys cannot flake under load,
//! and needs no large data set.

use metafolder_core::metarecord::{Field, Value};
use metafolder_core::query::FollowTarget;
use metafolder_core::query::{Aspect, OsmMode, Query};
use metafolder_daemon::forest_query;
use metafolder_daemon::index::{collect_path_targets, Eval, PageStrategy, QueryRoots, SortBy};
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::Writer;
use metafolder_daemon::tree_cache::{SortKeys, TreeCache};
use uuid::Uuid;

mod common;
use common::TempDir;

/// `n` files named `file000000.txt`…, `per_folder` per directory (`d0`, …);
/// a `kind` of three common values, and ten files of a rare one.
fn repository_in(n: usize, per_folder: usize) -> (KvStore, TempDir) {
    let dir = TempDir::new("kv-cost");
    let mut kv = KvStore::open(dir.path()).unwrap();
    let mut w = Writer::begin(&mut kv, None).unwrap();
    let tree = |parent: Option<Uuid>, name: &str| {
        Field::new("loc", Value::TreeRef { parent, name: name.into() })
    };
    let root = w.create_metarecord(vec![tree(None, "")]).unwrap().uuid;
    let mut folder = root;
    for i in 0..n {
        if i % per_folder == 0 {
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

/// The keys one page of `q` reads (sorted on `sort`, counted when `count`):
/// the query source's and the store's own (the forest, read by the path
/// resolution and the path sort keys), prepared as the route prepares it.
fn reads(kv: &KvStore, q: &Query, sort: &[(&str, bool)], count: bool) -> (usize, u64) {
    let before = kv.reads();
    let mut cache = TreeCache::new(false).without_forest();
    let mut roots = QueryRoots::new();
    let mut targets = Vec::new();
    collect_path_targets(q, &mut targets);
    for (field, path) in targets {
        if let Some(uuid) = cache.resolve_path(kv, &field, &path).unwrap() {
            roots.path.insert((field, path), uuid);
        }
    }
    let src = kv.source().unwrap();
    let e = Eval { src: &src, strategy: PageStrategy::Auto };
    // The forest's own leaves (an order-sensitive `osm` path), rewritten as
    // the route rewrites them.
    let q = &forest_query::resolve_path_leaves(&cache, kv, Some(&e), q).unwrap();
    let keys = SortKeys::with_store(&cache, kv);
    roots.keys = Some(&keys);
    let sort: Vec<SortBy> =
        sort.iter().map(|(f, asc)| SortBy { field: f.to_string(), ascending: *asc }).collect();
    let found = if count {
        e.page_and_count(q, &sort, Some(50), None, &roots).unwrap().2 as usize
    } else {
        e.evaluate_page_with_roots(q, &sort, Some(50), None, &roots).unwrap().0.len()
    };
    assert!(src.take_error().is_none());
    assert!(keys.take_error().is_none());
    (found, kv.reads() - before)
}

/// Runs `q` on both sizes and asserts the larger reads no more than a
/// little over what the smaller reads.
fn bounded(what: &str, q: &Query, sort: &[(&str, bool)], count: bool) {
    bounded_on(what, q, sort, count, 100);
}

/// [`bounded`] on repositories of `per_folder` files per directory.
fn bounded_on(what: &str, q: &Query, sort: &[(&str, bool)], count: bool, per_folder: usize) {
    let (small, _s) = repository_in(2_000, per_folder.min(2_000));
    let (large, _l) = repository_in(16_000, per_folder);
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

#[test]
fn a_folder_page_sorted_by_path_reads_the_page() {
    // One folder holding every file: 2 000, then 16 000 entries.
    let folder = Query::Follows { field: "loc".into(), target: FollowTarget::Path("/d0".into()) };
    bounded_on("a folder by path", &folder, &[("loc", true)], false, 16_000);
    bounded_on("a folder by path, descending", &folder, &[("loc", false)], false, 16_000);
    let below = Query::FollowsTransitive {
        field: "loc".into(),
        target: FollowTarget::Path("/d0".into()),
        inclusive: false,
    };
    bounded_on("a subtree by path", &below, &[("loc", true)], false, 16_000);
}

#[test]
fn a_multi_term_path_search_reads_its_candidates_not_the_forest() {
    // `d100` holds `file000123.txt` on either size; the other folders whose
    // name contains `d100` (`d1000`, `d10000`, …) hold no `000123`.
    let osm = Query::Osm {
        field: "loc".into(),
        terms: vec!["d100".into(), "000123".into()],
        mode: OsmMode::Path,
    };
    bounded("multi-term path search", &osm, &[], false);
    bounded("multi-term path search, counted", &osm, &[], true);
}

/// `n` files under `/top`, ten per folder: a subtree whose folders grow with
/// the repository.
fn nested(n: usize) -> (KvStore, TempDir) {
    let dir = TempDir::new("kv-cost-nested");
    let mut kv = KvStore::open(dir.path()).unwrap();
    let mut w = Writer::begin(&mut kv, None).unwrap();
    let tree = |parent: Option<Uuid>, name: &str| {
        Field::new("mfr_path", Value::TreeRef { parent, name: name.into() })
    };
    let root = w.create_metarecord(vec![tree(None, "")]).unwrap().uuid;
    let top = w.create_metarecord(vec![tree(Some(root), "top")]).unwrap().uuid;
    let mut folder = top;
    for i in 0..n {
        if i % 10 == 0 {
            folder = w.create_metarecord(vec![tree(Some(top), &format!("f{i}"))]).unwrap().uuid;
        }
        w.create_metarecord(vec![tree(Some(folder), &format!("file{i}"))]).unwrap();
    }
    w.commit().unwrap();
    (kv, dir)
}

#[test]
fn a_subtree_count_reads_no_more_for_more_folders() {
    let below = Query::FollowsTransitive {
        field: "mfr_path".into(),
        target: FollowTarget::Path("/top".into()),
        inclusive: false,
    };
    let (small, _s) = nested(1_000);
    let (large, _l) = nested(8_000);
    let (found_small, cost_small) = reads(&small, &below, &[], true);
    let (found_large, cost_large) = reads(&large, &below, &[], true);
    assert_eq!((found_small, found_large), (1_100, 8_800), "files and folders below /top");
    assert!(
        cost_large <= cost_small + cost_small / 2 + 20,
        "subtree count: {cost_small} keys for 100 folders, {cost_large} for 800"
    );
}
