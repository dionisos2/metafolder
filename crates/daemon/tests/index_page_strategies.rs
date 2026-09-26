//! The page strategies of the bitmap index against the SQL oracle
//! (spec-indexing "A page costs the page"). A sorted page can be produced two
//! ways: *fetch* — every match's sort key, then a partial sort — or *walk* —
//! an ordered structure (the uuid order, the bit-slices, the forest) read until
//! the page is full. Both must give exactly the oracle's pages, cursor after
//! cursor, on a repository built to hit their edge cases: multi-valued keys,
//! `Nothing` rows, metarecords lacking the key, a directory at two positions,
//! ties.

use metafolder_core::metarecord::{Field, Value};
use metafolder_core::query::{Aspect, FollowTarget, OsmMode, Query};
use metafolder_daemon::db;
use metafolder_daemon::index::{collect_path_targets, PageStrategy, QueryRoots, RepoIndex, SortBy};
use metafolder_daemon::log::Writer;
use metafolder_daemon::query_result::{SortKey, SortOrder};
use metafolder_daemon::tree_cache::{SortKeys, TreeCache};
use metafolder_query_oracle as query_exec;
use rusqlite::Connection;
use uuid::Uuid;

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

/// Not `mfr_path`, which allows one position per metarecord: this forest has a
/// directory at two.
const P: &str = "loc";

fn tref(parent: Option<Uuid>, name: &str) -> Field {
    Field::new(P, Value::TreeRef { parent, name: name.into() })
}

fn s(v: &str) -> Value {
    Value::String(v.into())
}

/// A repository whose every sort has something to trip on.
fn fixture() -> Connection {
    let mut conn = db::open_in_memory().unwrap();
    db::init_schema(&conn).unwrap();
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
    // A directory at two positions, with children (the cache hangs them under
    // the first position, and so must the walk).
    let twice = w
        .create_metarecord(vec![
            tref(Some(dirs[1]), "twice"),
            tref(Some(dirs[2]), "again"),
            Field::new("kind", s("dir")),
        ])
        .unwrap()
        .uuid;
    dirs.push(twice);
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
        // Names are unique per parent in the forest; a clash is simply retried.
        let _ = w.create_metarecord(fields);
    }
    // A directory that will lose its own position, leaving its children
    // detached: the cache keeps them apart as roots of their own, and the
    // oracle sorts them by their bare names (see index_oracle's
    // `tree_ref_sort_with_a_detached_node`). Made with a name sorting mid-way.
    let gone = w.create_metarecord(vec![tref(Some(dirs[3]), "gone")]).unwrap().uuid;
    for name in ["kept", "d05x", "zzz"] {
        w.create_metarecord(vec![tref(Some(gone), name), Field::new("kind", s("photo"))]).unwrap();
    }
    w.commit().unwrap();
    conn.execute(
        "DELETE FROM field WHERE metarecord_uuid = ?1 AND field_name = ?2",
        rusqlite::params![gone.as_bytes().to_vec(), P],
    )
    .unwrap();
    conn
}

fn queries(conn: &Connection, cache: &mut TreeCache) -> Vec<Query> {
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
        vec![(P, true)],
        vec![(P, false)],
        vec![("kind", true), ("size", false)],
    ]
}

/// The oracle's pages, cursor by cursor.
fn oracle_pages(
    conn: &Connection,
    cache: &mut TreeCache,
    q: &Query,
    by: &[(&str, bool)],
    limit: usize,
) -> Vec<Vec<Uuid>> {
    let sql_keys: Vec<SortKey> = by
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
            query_exec::execute(conn, cache, q, &sql_keys, Some(limit), cursor.as_deref()).unwrap();
        pages.push(page);
        match next {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    pages
}

/// The index's pages under `strategy`, cursor by cursor.
fn index_pages(
    index: &mut RepoIndex,
    roots: &QueryRoots,
    q: &Query,
    by: &[(&str, bool)],
    limit: usize,
    strategy: PageStrategy,
) -> Vec<Vec<Uuid>> {
    let keys: Vec<SortBy> =
        by.iter().map(|(f, asc)| SortBy { field: f.to_string(), ascending: *asc }).collect();
    index.set_page_strategy(strategy);
    let mut pages = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let (page, next) = index
            .evaluate_page_with_roots(q, &keys, Some(limit), cursor.as_deref(), roots)
            .unwrap();
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
    let conn = fixture();
    let mut cache = TreeCache::new(false);
    let mut index = RepoIndex::build(&conn).unwrap();
    for q in queries(&conn, &mut cache) {
        for by in sorts() {
            for limit in [10, 50] {
                let want = oracle_pages(&conn, &mut cache, &q, &by, limit);
                // What `run_query_filter` resolves through the tree cache.
                let mut targets = Vec::new();
                collect_path_targets(&q, &mut targets);
                let mut roots = QueryRoots::new();
                for (field, path) in targets {
                    if let Some(uuid) = cache.resolve_path(&conn, &field, &path).unwrap() {
                        roots.path.insert((field, path), uuid);
                    }
                }
                cache.populate(&conn).unwrap();
                let keys = SortKeys::new(&cache);
                roots.keys = Some(&keys);
                for strategy in [PageStrategy::Fetch, PageStrategy::Walk, PageStrategy::Auto] {
                    let got = index_pages(&mut index, &roots, &q, &by, limit, strategy);
                    assert_eq!(got, want, "{strategy:?}: {q:?} by {by:?}, pages of {limit}");
                }
            }
        }
    }
}

/// A metarecord at `/a/x` and `/b/x` matches "below `/b`", and sorts on its
/// smallest path, `/a/x` — before everything else below `/b`. A walk bounded
/// to `/b` meets it only at `/b/x`: it must refuse rather than drop it.
#[test]
fn a_smallest_position_outside_the_walked_subtree_is_not_dropped() {
    let mut conn = db::open_in_memory().unwrap();
    db::init_schema(&conn).unwrap();
    let mut w = Writer::begin(&mut conn, None).unwrap();
    let root = w.create_metarecord(vec![tref(None, "")]).unwrap().uuid;
    let a = w.create_metarecord(vec![tref(Some(root), "a")]).unwrap().uuid;
    let b = w.create_metarecord(vec![tref(Some(root), "b")]).unwrap().uuid;
    w.create_metarecord(vec![tref(Some(a), "x"), tref(Some(b), "x")]).unwrap();
    for name in ["m", "n", "o"] {
        w.create_metarecord(vec![tref(Some(b), name)]).unwrap();
    }
    w.commit().unwrap();

    let mut cache = TreeCache::new(false);
    let mut index = RepoIndex::build(&conn).unwrap();
    let q = Query::FollowsTransitive {
        field: P.into(),
        target: FollowTarget::Path("/b".into()),
        inclusive: false,
    };
    let by = [(P, true)];
    for limit in [1, 2, 10] {
        let want = oracle_pages(&conn, &mut cache, &q, &by, limit);
        let mut roots = QueryRoots::new();
        let node = cache.resolve_path(&conn, P, "/b").unwrap().unwrap();
        roots.path.insert((P.to_string(), "/b".to_string()), node);
        cache.populate(&conn).unwrap();
        let keys = SortKeys::new(&cache);
        roots.keys = Some(&keys);
        for strategy in [PageStrategy::Walk, PageStrategy::Auto] {
            let got = index_pages(&mut index, &roots, &q, &by, limit, strategy);
            assert_eq!(got, want, "{strategy:?}, pages of {limit}");
        }
    }
}
