//! Cost assertions for the event log: what an operation *does*, not how long
//! it takes (doc "Performance testing").
//!
//! Every test here counts the keys the store reads — its queries' and its
//! writes' alike. Nothing is timed, so nothing flakes under load, and the
//! suite runs in the ordinary `cargo test` pass: an algorithmic regression is
//! caught by the same run that catches a broken assertion, on a repository
//! small enough to build in a second.
//!
//! One shape of assertion, **growth invariance**: the same bounded operation,
//! run against a log of `N` and of `16N`, must read the same number of keys.
//! A count that grows with the data is the N+1 family, or a scan.

use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::{Retention, Writer, ORIGIN_WATCHER};
use metafolder_daemon::log_view::{listing, LogQuery};
use metafolder_daemon::store::{Log, Rows};

mod common;
use common::TempDir;

/// A repository whose log holds `revisions` revisions of one operation each —
/// the shape a long-lived repository ends up with, where almost every write is
/// its own revision (a manual edit, a watcher flush).
fn repo_with_log(revisions: usize) -> (KvStore, TempDir) {
    let (mut kv, dir) = common::kv::store();
    for i in 0..revisions {
        let mut w = Writer::begin(&mut kv, None).unwrap();
        w.create_metarecord(vec![
            Field::new("kind", Value::String("file".into())),
            Field::new("rank", Value::Int(i as i64)),
        ])
        .unwrap();
        w.commit().unwrap();
    }
    (kv, dir)
}

/// The keys `f` reads from `kv`, and what it returned.
fn reads_of<R>(kv: &mut KvStore, f: impl FnOnce(&mut KvStore) -> R) -> (R, u64) {
    let before = kv.reads();
    let out = f(kv);
    (out, kv.reads() - before)
}

/// A log longer than any window below reads — the first walk of a window of
/// 20 revisions reads 160 operations — and one sixteen times longer. On a
/// log shorter than the window the whole log *is* the window, and the two
/// would read different amounts for the right reason.
const SHORT_LOG: usize = 200;
const LONG_LOG: usize = 16 * SHORT_LOG;

fn bounded() -> LogQuery {
    LogQuery { limit: Some(50), ..LogQuery::default() }
}

/// The keys one listing reads.
fn cost_of(kv: &mut KvStore, q: &LogQuery) -> u64 {
    let (body, reads) = reads_of(kv, |kv| listing(kv, q).unwrap());
    assert!(body["operations"].is_array(), "the listing must answer");
    reads
}

#[test]
fn bounded_log_read_costs_the_same_on_a_log_sixteen_times_longer() {
    let q = bounded();
    let (mut small, _s) = repo_with_log(SHORT_LOG);
    let (mut big, _b) = repo_with_log(LONG_LOG);
    let (small_cost, big_cost) = (cost_of(&mut small, &q), cost_of(&mut big, &q));
    assert_eq!(small_cost, big_cost, "the keys read grew with the log");
}

#[test]
fn a_log_read_bounded_by_revisions_costs_the_same_on_a_longer_log() {
    let q = LogQuery { labels: Some(20), ..LogQuery::default() };
    let (mut small, _s) = repo_with_log(SHORT_LOG);
    let (mut big, _b) = repo_with_log(LONG_LOG);
    let (small_cost, big_cost) = (cost_of(&mut small, &q), cost_of(&mut big, &q));
    assert_eq!(small_cost, big_cost, "a listing bounded by revisions grew with the log");
}

#[test]
fn a_revision_bound_returns_whole_revisions_newest_first() {
    let (kv, _dir) = repo_with_log(40);
    let q = LogQuery { labels: Some(5), ..LogQuery::default() };
    let body = listing(&kv, &q).unwrap();
    let revisions = body["labels"].as_array().unwrap();
    assert_eq!(revisions.len(), 5, "asked for 5 revisions, got {}", revisions.len());
    // The newest ones, and every operation shown belongs to one of them.
    let ids: Vec<i64> = revisions.iter().map(|r| r["id"].as_i64().unwrap()).collect();
    assert_eq!(ids, vec![36, 37, 38, 39, 40], "the last five revisions of forty");
    for op in body["operations"].as_array().unwrap() {
        assert!(ids.contains(&op["label_id"].as_i64().unwrap()));
    }
    assert_eq!(body["total_labels"], 40, "the totals still cover the whole log");
}

/// A log whose first revision is a reconcile of `big` operations, followed by
/// `small` revisions of one operation each.
fn repo_after_reconcile(big: usize, small: usize) -> (KvStore, TempDir) {
    let (mut kv, dir) = common::kv::store();
    let mut w = Writer::begin(&mut kv, Some("reconcile".into())).unwrap();
    for i in 0..big {
        w.create_metarecord(vec![Field::new("rank", Value::Int(i as i64))]).unwrap();
    }
    w.commit().unwrap();
    for i in 0..small {
        let mut w = Writer::begin(&mut kv, None).unwrap();
        w.create_metarecord(vec![Field::new("edit", Value::Int(i as i64))]).unwrap();
        w.commit().unwrap();
    }
    (kv, dir)
}

/// `mf log list` asks for twenty revisions *and* a number of operations: a
/// reconcile's revision holds tens of thousands, and reading it whole to show
/// its line cost 0.7 s and 400 MB on a 50 000-file repository.
fn revisions_and_ops(revisions: usize, limit: usize) -> LogQuery {
    LogQuery { labels: Some(revisions), limit: Some(limit), ..LogQuery::default() }
}

#[test]
fn a_revision_window_is_bounded_by_operations_too() {
    let (kv, _dir) = repo_after_reconcile(800, 5);
    let body = listing(&kv, &revisions_and_ops(20, 100)).unwrap();
    let ops = body["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 100, "at most the operations asked for");
    let revisions = body["labels"].as_array().unwrap();
    assert_eq!(revisions.len(), 6, "the five edits, and the reconcile in part");
    // The reconcile is the oldest label shown, and the one cut: it says so
    // (counting what it holds in all would read it whole).
    let reconcile = &revisions[0];
    assert_eq!(reconcile["partial"], true, "{reconcile}");
    assert_eq!(ops.iter().filter(|o| o["label_id"] == reconcile["id"]).count(), 95);
    // The whole ones say nothing.
    for whole in &revisions[1..] {
        assert!(whole.get("partial").is_none(), "{whole}");
    }
    // Twenty revisions of one operation each are still all there.
    let (kv, _dir) = repo_with_log(40);
    let body = listing(&kv, &revisions_and_ops(20, 100)).unwrap();
    assert_eq!(body["labels"].as_array().unwrap().len(), 20);
    assert_eq!(body["operations"].as_array().unwrap().len(), 20);
}

#[test]
fn a_revision_window_costs_the_same_behind_a_larger_reconcile() {
    let q = revisions_and_ops(20, 100);
    let (mut small, _s) = repo_after_reconcile(400, 5);
    let (mut big, _b) = repo_after_reconcile(6_400, 5);
    let (small_cost, big_cost) = (cost_of(&mut small, &q), cost_of(&mut big, &q));
    assert_eq!(small_cost, big_cost, "a listing bounded by operations grew with the reconcile");
    assert_eq!(listing(&big, &q).unwrap()["operations"].as_array().unwrap().len(), 100);
}

/// Not the log: the point read every panel, every `mf metarecord get` and
/// every lookup does. It must cost the same on a repository of any size — and
/// the moment it does not, it is the whole interface that slows down at once.
#[test]
fn reading_one_metarecord_costs_the_same_in_a_repository_sixteen_times_larger() {
    let (mut small, _s) = repo_with_log(40);
    let (mut big, _b) = repo_with_log(640);
    let (small_uuid, big_uuid) = (small.metarecords().unwrap()[0], big.metarecords().unwrap()[0]);
    let (_, small_cost) = reads_of(&mut small, |kv| kv.metarecord(small_uuid).unwrap());
    let (_, big_cost) = reads_of(&mut big, |kv| kv.metarecord(big_uuid).unwrap());
    assert_eq!(small_cost, big_cost, "reading one metarecord grew with the repository");
}

#[test]
fn a_filtered_bounded_read_looks_past_the_window_for_matches() {
    // One metarecord written at the very start, then 300 revisions touching
    // others. A bounded read filtered on that metarecord must still find its
    // operations: a window is a bound on what is *returned*, not a promise to
    // stop looking (doc "Log endpoints").
    let (mut kv, _dir) = common::kv::store();
    let mut w = Writer::begin(&mut kv, None).unwrap();
    let target =
        w.create_metarecord(vec![Field::new("kind", Value::String("old".into()))]).unwrap();
    w.commit().unwrap();
    for i in 0..300 {
        let mut w = Writer::begin(&mut kv, None).unwrap();
        w.create_metarecord(vec![Field::new("rank", Value::Int(i))]).unwrap();
        w.commit().unwrap();
    }

    let q = LogQuery { limit: Some(10), entity: Some(target.uuid), ..LogQuery::default() };
    let body = listing(&kv, &q).unwrap();
    let ops = body["operations"].as_array().unwrap();
    assert!(!ops.is_empty(), "the metarecord's own operations were missed by the window");
    for op in ops {
        assert_eq!(op["entity_uuid"].as_str().unwrap(), target.uuid.as_simple().to_string());
    }
}

/// The keys one watcher write reads when it trims 100 of the watcher's
/// operations off a log that keeps 20 of them, behind `big` operations of the
/// user's (a reconcile's, say) that the trim keeps.
fn trim_cost(big: usize) -> u64 {
    let (mut kv, _dir) = common::kv::store();
    let mut w = Writer::begin(&mut kv, None).unwrap();
    for i in 0..big {
        w.create_metarecord(vec![Field::new("rank", Value::Int(i as i64))]).unwrap();
    }
    w.commit().unwrap();
    let watcher_write = |kv: &mut KvStore, retention: Retention| {
        let mut w = Writer::begin_with_retention(kv, None, retention).unwrap();
        w.set_origin(ORIGIN_WATCHER).unwrap();
        w.create_metarecord(vec![Field::new("kind", Value::String("file".into()))]).unwrap();
        w.commit().unwrap();
    };
    for _ in 0..119 {
        watcher_write(&mut kv, Retention::UNLIMITED);
    }
    let retention = Retention { watcher_ops: 20, user_ops: 0 };
    let (_, reads) = reads_of(&mut kv, |kv| watcher_write(kv, retention));
    assert_eq!(kv.counts().unwrap().0, 20 + big as i64, "the trim kept 20 watcher operations");
    reads
}

/// The retention trim deletes the oldest operations of an origin. What it
/// reads must depend on what it deletes, never on how much the log *keeps*:
/// it runs once every `slack` writes, and a trim that reads the whole log
/// reads a reconcile's tens of thousands of operations each time.
#[test]
fn trimming_the_log_does_not_read_the_operations_it_keeps() {
    assert_eq!(trim_cost(1), trim_cost(2_000), "the trim read the operations it keeps");
}
