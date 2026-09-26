//! Running a query through both engines, and asserting they agree.
//!
//! The SQL engine is the oracle; the bitmap index (plus the forest, through the
//! route's own preparation) is what the daemon actually serves from
//! (spec-indexing "No operand runs in SQL"). The semantics batteries in this
//! directory pin what a query *means*, so running them on the oracle alone
//! would leave the serving path untested by everything they assert.

use metafolder_core::query::Query;
use metafolder_daemon::error::ApiError;
use metafolder_daemon::index::{
    collect_node_paths, collect_path_targets, PageStrategy, QueryRoots, RepoIndex, SortBy,
};
use metafolder_daemon::query_result::{SortKey, SortOrder};
use metafolder_daemon::store::Store;
use metafolder_daemon::tree_cache::{SortKeys, TreeCache};
use metafolder_daemon::{forest_query, query_validate};
use metafolder_query_oracle as query_exec;
use rusqlite::Connection;
use uuid::Uuid;

/// The engine-independent rejections, exactly as the route makes them.
pub fn validate(conn: &Connection, query: &Query) -> Result<(), ApiError> {
    query_validate::validate_query(query)?;
    query_validate::check_query_size(query)?;
    let index = RepoIndex::build(conn).unwrap();
    query_validate::validate_query_types(query, &|f| index.value_type(f))
}

/// Runs `query` through both engines and asserts they agree, returning the
/// answer they share. Unsorted queries are compared as sets, sorted ones in
/// order — the order is the thing under test there.
pub fn both(
    conn: &Connection,
    cache: &mut TreeCache,
    query: &Query,
    sort: &[SortKey],
) -> Vec<Uuid> {
    let (sql, _) = query_exec::execute(conn, cache, query, sort, None, None).unwrap();
    let got = indexed(conn, cache, query, sort);
    if sort.is_empty() {
        let (mut got, mut want) = (got, sql.clone());
        got.sort();
        want.sort();
        assert_eq!(got, want, "index/SQL divergence on {query:?}");
    } else {
        assert_eq!(got, sql, "sort divergence on {query:?} by {sort:?}");
    }
    sql
}

/// Asserts both engines refuse `query`, and returns the oracle's message. A
/// rejection only one of them makes is a rejection that depends on who ran the
/// query.
pub fn both_refuse(conn: &Connection, cache: &mut TreeCache, query: &Query) -> String {
    let sql = match query_exec::execute(conn, cache, query, &[], None, None) {
        Ok(_) => panic!("query should have been rejected"),
        Err(e) => format!("{e:?}"),
    };
    assert!(
        validate(conn, query).is_err(),
        "the SQL engine refused {query:?} ({sql}) but the serving path accepted it"
    );
    sql
}

/// The serving path: the route's own preparation (path seeds, exact nodes, the
/// leaves the forest resolves) and then the bitmap index.
pub fn indexed(
    conn: &Connection,
    cache: &mut TreeCache,
    query: &Query,
    sort: &[SortKey],
) -> Vec<Uuid> {
    validate(conn, query).expect("validation");
    cache.populate(conn).unwrap();

    let sort_by: Vec<SortBy> = sort
        .iter()
        .map(|k| SortBy { field: k.field.clone(), ascending: k.order == SortOrder::Asc })
        .collect();
    let (roots, indexed) = prepare(cache, conn, query);
    let keys = SortKeys::new(cache);
    let roots = QueryRoots { keys: Some(&keys), ..roots };
    let index = RepoIndex::build(conn).unwrap();
    let got = match index.evaluate_page_with_roots(&indexed, &sort_by, None, None, &roots) {
        Ok((uuids, _)) => uuids,
        Err(gap) => panic!("the serving path declined {query:?}: {gap}"),
    };
    // The same evaluator over the key-value store's derived key spaces, and
    // the forest read from that store: a KV repository keeps none resident.
    let (kv, _dir) = super::kv::kv_mirror(conn);
    let mut no_forest = TreeCache::new(false).without_forest();
    let (roots, indexed) = prepare(&mut no_forest, &kv, query);
    let keys = SortKeys::with_store(&no_forest, &kv);
    let roots = QueryRoots { keys: Some(&keys), ..roots };
    let on_kv = super::kv::with_kv(&kv, PageStrategy::Auto, |e, _| {
        e.evaluate_page_with_roots(&indexed, &sort_by, None, None, &roots)
    });
    assert!(keys.take_error().is_none(), "reading the forest failed");
    match on_kv {
        Ok((uuids, _)) => assert_eq!(uuids, got, "KV/resident divergence on {query:?}"),
        Err(gap) => panic!("the KV source declined {query:?}: {gap}"),
    }
    got
}

/// The route's preparation of a query: its path seeds and exact nodes
/// resolved, its forest leaves rewritten — through `cache`, or `store` where
/// the cache holds no forest.
pub fn prepare(
    cache: &mut TreeCache,
    store: &dyn Store,
    query: &Query,
) -> (QueryRoots<'static>, Query) {
    let mut roots = QueryRoots::new();
    let mut path_targets = Vec::new();
    collect_path_targets(query, &mut path_targets);
    for (field, path) in path_targets {
        if let Some(uuid) = cache.resolve_path(store, &field, &path).unwrap() {
            roots.path.insert((field, path), uuid);
        }
    }
    let mut node_paths = Vec::new();
    collect_node_paths(query, &mut node_paths);
    for (field, path) in node_paths {
        let node = cache.resolve_path(store, &field, &path).unwrap();
        roots.node.insert((field, path), node);
    }
    let indexed = forest_query::resolve_path_leaves(cache, store, query).unwrap();
    (roots, indexed)
}
