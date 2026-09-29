//! Running a query through both engines, and asserting they agree.
//!
//! The naive oracle (`metafolder-query-oracle`, which reads every row) is the
//! reference; the evaluator over the store's derived key spaces (plus the
//! forest, through the route's own preparation) is what the daemon serves from
//! (spec-indexing "No operand runs in SQL"). The semantics batteries in this
//! directory pin what a query *means*, so running them on the oracle alone
//! would leave the serving path untested by everything they assert.

use metafolder_core::query::Query;
use metafolder_daemon::error::ApiError;
use metafolder_daemon::index::{
    collect_node_paths, collect_path_targets, Eval, PageStrategy, QueryRoots, SortBy,
};
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::query_result::{SortKey, SortOrder};
use metafolder_daemon::store::Store;
use metafolder_daemon::tree_cache::{SortKeys, TreeCache};
use metafolder_daemon::{forest_query, query_validate};
use metafolder_query_oracle as query_exec;
use uuid::Uuid;

/// The engine-independent rejections, exactly as the route makes them.
pub fn validate(kv: &KvStore, query: &Query) -> Result<(), ApiError> {
    query_validate::validate_query(query)?;
    query_validate::check_query_size(query)?;
    super::kv::with_kv(kv, PageStrategy::Auto, |_, src| {
        query_validate::validate_query_types(query, &|f| {
            metafolder_daemon::index::Source::value_type(src, f).map(str::to_string)
        })
    })
}

/// Runs `query` through the oracle and the serving path and asserts they
/// agree, returning the answer they share. Unsorted queries are compared as
/// sets, sorted ones in order — the order is the thing under test there.
pub fn both(kv: &KvStore, query: &Query, sort: &[SortKey]) -> Vec<Uuid> {
    let (want, _) = query_exec::execute(kv, query, sort, None, None).unwrap();
    let got = indexed(kv, query, sort);
    if sort.is_empty() {
        let (mut got, mut want) = (got, want.clone());
        got.sort();
        want.sort();
        assert_eq!(got, want, "index/oracle divergence on {query:?}");
    } else {
        assert_eq!(got, want, "sort divergence on {query:?} by {sort:?}");
    }
    want
}

/// Asserts both refuse `query`, and returns the oracle's message. A rejection
/// only one of them makes is a rejection that depends on who ran the query.
pub fn both_refuse(kv: &KvStore, query: &Query) -> String {
    let message = match query_exec::execute(kv, query, &[], None, None) {
        Ok(_) => panic!("query should have been rejected"),
        Err(e) => format!("{e:?}"),
    };
    assert!(
        validate(kv, query).is_err(),
        "the oracle refused {query:?} ({message}) but the serving path accepted it"
    );
    message
}

/// The serving path: the route's own preparation (path seeds, exact nodes, the
/// leaves the forest resolves), then the evaluator over the store's derived
/// key spaces, the forest read from the store as a repository keeps none
/// resident.
pub fn indexed(kv: &KvStore, query: &Query, sort: &[SortKey]) -> Vec<Uuid> {
    validate(kv, query).expect("validation");
    let sort_by: Vec<SortBy> = sort
        .iter()
        .map(|k| SortBy { field: k.field.clone(), ascending: k.order == SortOrder::Asc })
        .collect();
    let no_forest = TreeCache::new(false);
    let (roots, indexed) =
        super::kv::with_kv(kv, PageStrategy::Auto, |e, _| prepare(&no_forest, kv, Some(e), query));
    let keys = SortKeys::new(kv);
    let roots = QueryRoots { keys: Some(&keys), ..roots };
    let got = super::kv::with_kv(kv, PageStrategy::Auto, |e, _| {
        e.evaluate_page_with_roots(&indexed, &sort_by, None, None, &roots)
    });
    assert!(keys.take_error().is_none(), "reading the forest failed");
    match got {
        Ok((uuids, _)) => uuids,
        Err(gap) => panic!("the serving path declined {query:?}: {gap}"),
    }
}

/// The route's preparation of a query: its path seeds and exact nodes
/// resolved, its forest leaves rewritten — through `cache`, or `store` where
/// the cache holds no forest.
pub fn prepare(
    cache: &TreeCache,
    store: &dyn Store,
    names: Option<&Eval>,
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
    let indexed = forest_query::resolve_path_leaves(cache, store, names, query).unwrap();
    (roots, indexed)
}
