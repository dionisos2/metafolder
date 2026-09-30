//! The page strategies of the bitmap index against the oracle
//! (doc "Sorting and postings"). A sorted page can be produced two
//! ways: *fetch* — every match's sort key, then a partial sort — or *walk* —
//! an ordered structure (the uuid order, the bit-slices, the forest) read until
//! the page is full. Both must give exactly the oracle's pages, cursor after
//! cursor, on a repository built to hit their edge cases: multi-valued keys,
//! `Nothing` rows, metarecords lacking the key, a detached subtree, ties.

use metafolder_core::metarecord::{Field, Value};
use metafolder_core::query::{Aspect, FollowTarget, OsmMode, Query};
use metafolder_daemon::index::{collect_path_targets, Eval, PageStrategy, QueryRoots, SortBy};
use metafolder_daemon::log::Writer;
use metafolder_daemon::query_result::{SortKey, SortOrder};
use metafolder_daemon::tree_cache::{SortKeys, TreeCache};
use metafolder_query_oracle as query_exec;
use uuid::Uuid;

use metafolder_daemon::kvstore::KvStore;

mod common;
use common::kv::with_kv;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// A forest of its own, apart from `mfr_path`.
const P: &str = "loc";

fn tref(parent: Option<Uuid>, name: &str) -> Field {
    Field::new(P, Value::TreeRef { parent, name: name.into() })
}

fn s(v: &str) -> Value {
    Value::String(v.into())
}

/// A repository whose every sort has something to trip on.
fn fixture() -> (KvStore, common::TempDir) {
    let (mut conn, _conn_dir) = common::kv::store();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut w = Writer::begin(&mut conn, None).unwrap();
    let root = w.create_metarecord(vec![tref(None, "")]).unwrap().uuid;
    let mut dirs = vec![root];
    for k in 0..12 {
        let parent = dirs[rng.below(dirs.len() as u64) as usize];
        let d = w
            .create_metarecord(vec![
                tref(Some(parent), &format!("d{k:02}")),
                Field::new("kind", s("dir")),
            ])
            .unwrap()
            .uuid;
        dirs.push(d);
    }
    // A directory one level deeper, with children.
    let deeper = w
        .create_metarecord(vec![tref(Some(dirs[1]), "deeper"), Field::new("kind", s("dir"))])
        .unwrap()
        .uuid;
    dirs.push(deeper);
    for i in 0..400u64 {
        let mut fields = Vec::new();
        // One in twenty is in no forest at all.
        if i % 20 != 0 {
            let parent = dirs[rng.below(dirs.len() as u64) as usize];
            fields.push(tref(Some(parent), &format!("f{:03}", rng.below(1000))));
        }
        fields.push(Field::new("kind", s(["photo", "note", "song"][rng.below(3) as usize])));
        // Sizes: ties (only 40 distinct), several values, Nothing, or none.
        match rng.below(10) {
            0 => {}
            1 => fields.push(Field::new("size", Value::Nothing)),
            2 => {
                fields.push(Field::new("size", Value::Int(rng.below(40) as i64)));
                fields.push(Field::new("size", Value::Int(rng.below(40) as i64)));
            }
            _ => fields.push(Field::new("size", Value::Int(rng.below(40) as i64 - 10))),
        }
        if rng.below(4) != 0 {
            fields.push(Field::new("mtime", Value::DateTime(rng.below(5_000) as i64 * 1000)));
        }
        if rng.below(3) == 0 {
            fields.push(Field::new("score", Value::Float(rng.below(100) as f64 / 7.0 - 3.0)));
        }
        // Titles: ties, two values, and long texts sharing their first 300
        // bytes (the key-value store keys those by a prefix and a hash).
        let title = |rng: &mut Rng| match rng.below(4) {
            0 => format!("{}{}", "t".repeat(300), rng.below(5)),
            _ => format!("t{}", rng.below(25)),
        };
        match rng.below(5) {
            0 => {}
            1 => {
                fields.push(Field::new("title", s(&title(&mut rng))));
                fields.push(Field::new("title", s(&title(&mut rng))));
            }
            _ => fields.push(Field::new("title", s(&title(&mut rng)))),
        }
        // Names are unique per parent in the forest; a clash is simply retried.
        let _ = w.create_metarecord(fields);
    }
    // A directory that will lose its own position, leaving its children
    // detached: the store keeps them apart as roots of their own, and the
    // oracle sorts them by their bare names (see index_oracle's
    // `tree_ref_sort_with_a_detached_node`). Made with a name sorting mid-way.
    let gone = w.create_metarecord(vec![tref(Some(dirs[3]), "gone")]).unwrap().uuid;
    for name in ["kept", "d05x", "zzz"] {
        w.create_metarecord(vec![tref(Some(gone), name), Field::new("kind", s("photo"))]).unwrap();
    }
    w.commit().unwrap();
    common::kv::delete_rows(&mut conn, gone, P);
    (conn, _conn_dir)
}

fn queries(conn: &KvStore, cache: &TreeCache) -> Vec<Query> {
    let present = |f: &str| Query::IsPresent { field: f.into(), aspect: Default::default() };
    let eq =
        |f: &str, v: Value| Query::Eq { field: f.into(), value: v, aspect: Default::default() };
    let matches = |f: &str, p: &str| Query::Matches {
        field: f.into(),
        pattern: p.into(),
        aspect: if f == P { Aspect::Value } else { Aspect::Raw },
    };
    let _ = (conn, cache);
    vec![
        present("kind"),
        present(P),
        eq("kind", s("photo")),
        Query::Not { operand: Box::new(eq("kind", s("note"))) },
        Query::FollowsTransitive {
            field: P.into(),
            target: FollowTarget::Path("/d00".into()),
            inclusive: false,
        },
        Query::And {
            operands: vec![present("size"), Query::Not { operand: Box::new(eq("kind", s("dir"))) }],
        },
        // Text on the forest's names: checked where a walk looks, when the page
        // comes without a count.
        matches(P, "^f[0-4]"),
        Query::Osm { field: P.into(), terms: vec!["f1".into()], mode: OsmMode::Direct },
        Query::And {
            operands: vec![
                Query::FollowsTransitive {
                    field: P.into(),
                    target: FollowTarget::Path("/d00".into()),
                    inclusive: false,
                },
                matches(P, "7"),
                matches("kind", "^(photo|song)$"),
            ],
        },
    ]
}

fn sorts() -> Vec<Vec<(&'static str, bool)>> {
    vec![
        vec![],
        vec![("size", true)],
        vec![("size", false)],
        vec![("mtime", false)],
        vec![("mtime", true)],
        vec![("score", true)],
        vec![("title", true)],
        vec![("title", false)],
        vec![(P, true)],
        vec![(P, false)],
        vec![("kind", true), ("size", false)],
    ]
}

/// The oracle's pages, cursor by cursor.
fn oracle_pages(conn: &KvStore, q: &Query, by: &[(&str, bool)], limit: usize) -> Vec<Vec<Uuid>> {
    let oracle_keys: Vec<SortKey> = by
        .iter()
        .map(|(f, asc)| SortKey {
            field: f.to_string(),
            order: if *asc { SortOrder::Asc } else { SortOrder::Desc },
        })
        .collect();
    let mut pages = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let (page, next) =
            query_exec::execute(conn, q, &oracle_keys, Some(limit), cursor.as_deref()).unwrap();
        pages.push(page);
        match next {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    pages
}

/// The pages of an evaluator (the KV source's), cursor by cursor.
fn eval_pages(
    e: &Eval,
    roots: &QueryRoots,
    q: &Query,
    by: &[(&str, bool)],
    limit: usize,
) -> Vec<Vec<Uuid>> {
    let keys: Vec<SortBy> =
        by.iter().map(|(f, asc)| SortBy { field: f.to_string(), ascending: *asc }).collect();
    let mut pages = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let (page, next) =
            e.evaluate_page_with_roots(q, &keys, Some(limit), cursor.as_deref(), roots).unwrap();
        pages.push(page);
        match next {
            Some(c) => cursor = Some(c),
            None => break,
        }
        assert!(pages.len() < 10_000, "runaway pagination");
    }
    pages
}

#[test]
fn every_page_strategy_gives_the_oracles_pages() {
    let (conn, _dir) = fixture();
    // As the route prepares a query: the forest is read from the store.
    let cache = TreeCache::new(false);
    for q in queries(&conn, &cache) {
        for by in sorts() {
            for limit in [10, 50] {
                let want = oracle_pages(&conn, &q, &by, limit);
                // What `run_query_filter` resolves through the tree cache.
                let mut targets = Vec::new();
                collect_path_targets(&q, &mut targets);
                let mut roots = QueryRoots::new();
                for (field, path) in targets {
                    if let Some(uuid) = cache.resolve_path(&conn, &field, &path).unwrap() {
                        roots.path.insert((field, path), uuid);
                    }
                }
                let keys = SortKeys::new(&conn);
                roots.keys = Some(&keys);
                for strategy in [PageStrategy::Fetch, PageStrategy::Walk, PageStrategy::Auto] {
                    let got =
                        with_kv(&conn, strategy, |e, _| eval_pages(e, &roots, &q, &by, limit));
                    assert_eq!(got, want, "{strategy:?}: {q:?} by {by:?}, pages of {limit}");
                }
            }
        }
    }
}
