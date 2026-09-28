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
    repository_with(n, per_folder, 0)
}

/// [`repository_in`] whose files each carry `extra` more fields, as a real
/// file carries its `mfr_*` and metadata fields.
fn repository_with(n: usize, per_folder: usize, extra: usize) -> (KvStore, TempDir) {
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
        w.create_metarecord(
            vec![
                tree(Some(folder), &format!("file{i:06}.txt")),
                Field::new("name", Value::String(format!("file{i:06}.txt"))),
                Field::new("kind", Value::String(kind.into())),
                Field::new("rating", Value::Int((i % 10) as i64)),
            ]
            .into_iter()
            .chain(
                (0..extra).map(|j| Field::new(format!("extra{j:02}"), Value::Int((i + j) as i64))),
            )
            .collect(),
        )
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
    let mut cache = TreeCache::new(false);
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
    let keys = SortKeys::new(kv);
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
    bounded_sizes(what, q, sort, count, per_folder, (2_000, 16_000));
}

/// [`bounded_on`] between two repository sizes.
fn bounded_sizes(
    what: &str,
    q: &Query,
    sort: &[(&str, bool)],
    count: bool,
    per_folder: usize,
    (small_n, large_n): (usize, usize),
) {
    let (small, _s) = repository_in(small_n, per_folder.min(small_n));
    let (large, _l) = repository_in(large_n, per_folder);
    let (found_small, cost_small) = reads(&small, q, sort, count);
    let (found_large, cost_large) = reads(&large, q, sort, count);
    assert_eq!(found_small, found_large, "{what}: the answer should not grow");
    assert!(
        cost_large <= cost_small + cost_small / 2 + 20,
        "{what}: {cost_small} keys on {small_n} records, {cost_large} on {large_n}"
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

fn and(operands: Vec<Query>) -> Query {
    Query::And { operands }
}

fn osm(field: &str, terms: &[&str], mode: OsmMode) -> Query {
    Query::Osm { field: field.into(), terms: terms.iter().map(|t| t.to_string()).collect(), mode }
}

fn value_matches(field: &str, pattern: &str) -> Query {
    Query::Matches { field: field.into(), pattern: pattern.into(), aspect: Aspect::Value }
}

fn int(op: fn(String, Value) -> Query, field: &str, n: i64) -> Query {
    op(field.into(), Value::Int(n))
}

fn gt(field: String, value: Value) -> Query {
    Query::Gt { field, value, aspect: Aspect::Raw }
}

/// The folder `/d100` and everything below it: a hundred files on either size.
fn subtree() -> Query {
    Query::FollowsTransitive {
        field: "loc".into(),
        target: FollowTarget::Path("/d100".into()),
        inclusive: true,
    }
}

// Combinations of operands of different types. What the bitmaps buy is that
// a query combining a wide operand with a narrow one costs about the narrow
// one: none of these answers grows with the repository, so neither may the
// keys read.

#[test]
fn the_finder_search_reads_its_candidates() {
    // The GUI's search box: the same terms against the path, and the text of
    // two fields.
    let terms = ["d100", "000123"];
    let finder = Query::Or {
        operands: vec![
            osm("loc", &terms, OsmMode::Path),
            osm("name", &terms, OsmMode::Direct),
            osm("kind", &terms, OsmMode::Direct),
        ],
    };
    bounded("finder search", &finder, &[], false);
    bounded("finder search, counted", &finder, &[], true);
}

#[test]
fn a_wide_operand_and_a_rare_one_cost_the_rare_one() {
    // A third of the repository, and a single name — in both orders.
    let wide = eq("kind", "note");
    let rare = eq("name", "file000123.txt");
    bounded("wide and rare", &and(vec![wide.clone(), rare.clone()]), &[], true);
    bounded("rare and wide", &and(vec![rare, wide]), &[], true);
    // A range over every record, and a rare value.
    let range = int(gt, "rating", 1);
    bounded("range and rare", &and(vec![range, eq("kind", "rare")]), &[], true);
}

#[test]
fn a_subtree_a_range_and_a_text_combine() {
    let q = and(vec![subtree(), int(gt, "rating", 4), matches("name", "file0001")]);
    bounded("subtree, range and text", &q, &[], true);
    let q = and(vec![subtree(), value_matches("loc", "file0001")]);
    bounded("subtree and name", &q, &[], true);
}

/// A page without its count, where the text leaf on a forest's names can wait
/// for the page walk: the walk's candidates are half the repository, but the
/// name keeps one in a hundred of them. Walking the uuid order for them reads
/// the repository; evaluating the leaf over the candidates reads its matches.
#[test]
fn a_page_with_a_wide_operand_and_a_rare_name_reads_the_name() {
    let q = and(vec![int(gt, "rating", 4), value_matches("loc", "file0001")]);
    bounded("wide range and a rare name, a page", &q, &[], false);
    let q = and(vec![int(gt, "rating", 4), osm("loc", &["file0001"], OsmMode::Direct)]);
    bounded("wide range and a rare osm name, a page", &q, &[], false);
}

#[test]
fn a_negation_combines_with_a_narrow_operand() {
    let q = and(vec![Query::Not { operand: Box::new(eq("kind", "note")) }, subtree()]);
    bounded("not wide, in a subtree", &q, &[], true);
    let q = and(vec![matches("name", "file00012"), Query::Not { operand: Box::new(subtree()) }]);
    bounded("text, not in a subtree", &q, &[], true);
}

#[test]
fn a_combined_page_sorted_on_a_value_reads_the_page() {
    // Most of the repository matches; one sorted page of it is asked for.
    let q = and(vec![
        present("name"),
        int(gt, "rating", 2),
        Query::Not { operand: Box::new(eq("kind", "photo")) },
    ]);
    bounded("combined page by name", &q, &[("name", true)], false);
    // Sorted on a value with ten distinct values: the page's rating is held
    // by a tenth of the repository, and ties go by uuid — the page costs
    // about the page over the density of its matches, whatever the size.
    // Past ~12 000 records the uuid order is the cheaper way through a run;
    // both sizes are.
    let sizes = (16_000, 64_000);
    bounded_sizes("combined page by rating", &q, &[("rating", false)], false, 100, sizes);
    bounded_sizes("combined page by rating, up", &q, &[("rating", true)], false, 100, sizes);
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

/// `n` nodes under `/top` in the forest `field`, ten per folder: a subtree
/// whose folders grow with the repository.
fn nested(field: &str, n: usize) -> (KvStore, TempDir) {
    let dir = TempDir::new("kv-cost-nested");
    let mut kv = KvStore::open(dir.path()).unwrap();
    let mut w = Writer::begin(&mut kv, None).unwrap();
    let tree = |parent: Option<Uuid>, name: &str| {
        Field::new(field, Value::TreeRef { parent, name: name.into() })
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

/// A subtree is one descendant bitmap on every forest — the file tree's and
/// a tag tree's alike (`tag ->* (path =>* "music")` reads the second).
#[test]
fn a_subtree_count_reads_no_more_for_more_folders() {
    for field in ["mfr_path", "path"] {
        a_subtree_count_reads_no_more_for_more_folders_in(field);
    }
}

fn a_subtree_count_reads_no_more_for_more_folders_in(field: &str) {
    let below = Query::FollowsTransitive {
        field: field.into(),
        target: FollowTarget::Path("/top".into()),
        inclusive: false,
    };
    let (small, _s) = nested(field, 1_000);
    let (large, _l) = nested(field, 8_000);
    let (found_small, cost_small) = reads(&small, &below, &[], true);
    let (found_large, cost_large) = reads(&large, &below, &[], true);
    assert_eq!((found_small, found_large), (1_100, 8_800), "files and folders below /top");
    assert!(
        cost_large <= cost_small + cost_small / 2 + 20,
        "subtree count in {field}: {cost_small} keys for 100 folders, {cost_large} for 800"
    );
}

/// A candidate is checked on the field searched — its name, its positions —
/// not on every field its record holds: records carrying twenty more fields
/// cost the same keys.
#[test]
fn a_candidate_check_reads_the_searched_field_only() {
    let (lean, _l) = repository_with(2_000, 100, 0);
    let (wide, _w) = repository_with(2_000, 100, 20);
    let terms = ["d100", "0001"];
    for (what, q) in [
        ("text search", matches("name", "file0001")),
        ("osm path", osm("loc", &terms, OsmMode::Path)),
    ] {
        let (found_lean, cost_lean) = reads(&lean, &q, &[], true);
        let (found_wide, cost_wide) = reads(&wide, &q, &[], true);
        assert!(found_lean > 0, "{what}: something to check");
        assert_eq!(found_lean, found_wide, "{what}");
        assert!(
            cost_wide <= cost_lean + cost_lean / 10 + 5,
            "{what}: {cost_lean} keys with four fields a record, {cost_wide} with twenty-four"
        );
    }
}

/// An `osm` path reads each candidate anchor's rows once, for its name and
/// its positions together: it costs about the name scan of its last term,
/// plus a descendant bitmap per anchor for the subtrees — not that scan and
/// then a second read of every candidate (6 965 keys here, against 3 593).
#[test]
fn an_osm_path_reads_each_candidates_rows_once() {
    let (kv, _d) = repository_in(4_000, 100);
    let path = osm("loc", &["d", "001"], OsmMode::Path);
    let scan = value_matches("loc", "(?i)001");
    let (_, cost_path) = reads(&kv, &path, &[], true);
    let (candidates, cost_scan) = reads(&kv, &scan, &[], true);
    assert!(candidates > 100, "{candidates}: enough candidates to tell");
    // Every candidate is an anchor here (every path holds a `d`).
    let candidates = candidates as u64;
    assert!(
        cost_path < cost_scan + candidates + candidates / 4,
        "{cost_path} keys for the path, {cost_scan} for the name scan of {candidates} candidates"
    );
}

fn path_leaf(op: fn(String, Value) -> Query, value: &str) -> Query {
    op("loc".into(), Value::String(value.into()))
}

fn path_eq(field: String, value: Value) -> Query {
    Query::Eq { field, value, aspect: Aspect::Path }
}

fn path_neq(field: String, value: Value) -> Query {
    Query::Neq { field, value, aspect: Aspect::Path }
}

fn path_matches(pattern: &str) -> Query {
    Query::Matches { field: "loc".into(), pattern: pattern.into(), aspect: Aspect::Path }
}

/// A `:path` comparison seeks its path down the stored forest instead of
/// walking all of it (spec-storage "pruned `:path` walk"): an exact path
/// costs its depth, a difference the same plus the holders of the field.
#[test]
fn a_path_comparison_seeks_its_path_not_the_forest() {
    let exact = path_leaf(path_eq, "/d0/file000003.txt");
    bounded("exact path", &exact, &[], false);
    bounded("exact path, counted", &exact, &[], true);
    bounded("missing path", &path_leaf(path_eq, "/d0/nope"), &[], true);
    bounded("differing path", &path_leaf(path_neq, "/d0/file000003.txt"), &[], false);
}

/// A pattern anchored on a literal prefix walks the subtrees that prefix
/// reaches, not the forest: `d0` holds the same hundred files on either size.
#[test]
fn an_anchored_path_pattern_walks_its_prefix_only() {
    bounded("anchored folder", &path_matches("^/d0/"), &[], true);
    bounded("anchored name prefix", &path_matches("^/d0/file00000"), &[], true);
    bounded("anchored, then any", &path_matches("^/d0/file.*3\\.txt$"), &[], true);
}
