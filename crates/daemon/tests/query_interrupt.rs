//! Interrupting a query inside its loops (doc "Cancelling a task"): a cancel
//! request or an expired `timeout_ms` is polled where the store counts its key
//! reads, so a query stops within a few hundred keys of the request — not at the
//! end of a phase that may read the whole repository.
//!
//! Counted, not timed: the probe trips at its n-th poll, and what is asserted is
//! how many keys the query read before it stopped.

use std::cell::Cell;
use std::rc::Rc;

use metafolder_core::metarecord::{Field, Value};
use metafolder_core::query::{Aspect, Query};
use metafolder_daemon::forest_query;
use metafolder_daemon::index::{Eval, PageStrategy, QueryRoots, SortBy};
use metafolder_daemon::interrupt::{self, Reason, POLL_EVERY};
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::Writer;
use metafolder_daemon::tree_cache::{SortKeys, TreeCache};
use uuid::Uuid;

mod common;
use common::TempDir;

/// `n` files under one folder, each with a distinct `name`.
fn repository(n: usize) -> (KvStore, TempDir) {
    let dir = TempDir::new("interrupt");
    let mut kv = KvStore::open_unsynced(dir.path()).unwrap();
    let mut w = Writer::begin(&mut kv, None).unwrap();
    let tree = |parent: Option<Uuid>, name: &str| {
        Field::new("loc", Value::TreeRef { parent, name: name.into() })
    };
    let root = w.create_metarecord(vec![tree(None, "")]).unwrap().uuid;
    let folder = w.create_metarecord(vec![tree(Some(root), "d")]).unwrap().uuid;
    for i in 0..n {
        w.create_metarecord(vec![
            tree(Some(folder), &format!("file{i:06}.txt")),
            Field::new("name", Value::String(format!("file{i:06}.txt"))),
        ])
        .unwrap();
    }
    w.commit().unwrap();
    (kv, dir)
}

/// A probe that asks to stop at its `trip_at`-th poll (never, for `None`).
fn probe_tripping_at(trip_at: Option<u64>) -> Box<dyn Fn() -> Option<Reason>> {
    let polls = Rc::new(Cell::new(0u64));
    Box::new(move || {
        polls.set(polls.get() + 1);
        trip_at.filter(|&n| polls.get() >= n).map(|_| Reason::Cancelled)
    })
}

/// Runs `q` sorted on `sort` with its count, the forest's leaves prepared as
/// the route prepares them, under a probe tripping at `trip_at`: the keys read
/// and whether it was interrupted.
fn run(kv: &KvStore, q: &Query, sort: &str, trip_at: Option<u64>) -> (u64, Option<Reason>) {
    let before = kv.reads();
    let (_, reason) = interrupt::run(probe_tripping_at(trip_at), || {
        let cache = TreeCache::new(false);
        let src = kv.source().unwrap();
        let e = Eval { src: &src, strategy: PageStrategy::Auto };
        // An interrupted preparation fails; the evaluation is not reached.
        let Ok(q) = forest_query::resolve_path_leaves(&cache, kv, Some(&e), q) else {
            return;
        };
        let keys = SortKeys::new(kv);
        let mut roots = QueryRoots::new();
        roots.keys = Some(&keys);
        let sort = [SortBy { field: sort.into(), ascending: true }];
        let _ = e.page_and_count(&q, &sort, Some(50), None, &roots);
    });
    (kv.reads() - before, reason)
}

fn assert_stops_early(what: &str, q: Query, sort: &str) {
    let (kv, _dir) = repository(20_000);
    let (full, reason) = run(&kv, &q, sort, None);
    assert_eq!(reason, None, "{what}: a probe that never trips interrupts nothing");
    assert!(full > 20 * POLL_EVERY, "{what}: the query must be heavy to test this ({full} keys)");

    let (read, reason) = run(&kv, &q, sort, Some(3));
    assert_eq!(reason, Some(Reason::Cancelled), "{what}");
    assert!(
        read <= 4 * POLL_EVERY,
        "{what}: stopped after {read} keys; the third poll comes by {} (full query: {full})",
        3 * POLL_EVERY
    );
}

#[test]
fn a_regex_scan_stops_in_its_loop() {
    // Shorter than a trigram: every distinct value is read.
    assert_stops_early(
        "matches",
        Query::Matches { field: "name".into(), pattern: "7".into(), aspect: Aspect::Raw },
        "name",
    );
}

#[test]
fn a_sort_over_every_match_stops_in_its_loop() {
    // A `tree_ref` sort rebuilds every match's path: no ordered walk stops it.
    assert_stops_early(
        "sort",
        Query::IsPresent { field: "name".into(), aspect: Aspect::Raw },
        "loc",
    );
}

#[test]
fn an_unanchored_path_pattern_stops_in_the_forest_walk() {
    assert_stops_early(
        "path",
        Query::Matches { field: "loc".into(), pattern: "7".into(), aspect: Aspect::Path },
        "name",
    );
}

#[test]
fn outside_a_scope_nothing_is_interrupted() {
    assert!(interrupt::check(1_000_000).is_ok());
}
