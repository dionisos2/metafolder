//! Cost assertions for the event log: what an operation *does*, not how long
//! it takes (spec-perf "Cost assertions").
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
use metafolder_daemon::log::{self, Retention, Writer};
use metafolder_daemon::log_view::{listing, LogQuery, Mode};
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

fn bounded(mode: Mode) -> LogQuery {
    LogQuery { mode, limit: Some(50), ..LogQuery::default() }
}

/// The keys one listing reads.
fn cost_of(kv: &mut KvStore, q: &LogQuery) -> u64 {
    let (body, reads) = reads_of(kv, |kv| listing(kv, q).unwrap());
    assert!(body["operations"].is_array(), "the listing must answer");
    reads
}

#[test]
fn bounded_log_read_costs_the_same_on_a_log_sixteen_times_longer() {
    for mode in [Mode::Linear, Mode::Active] {
        let q = bounded(mode);
        let (mut small, _s) = repo_with_log(SHORT_LOG);
        let (mut big, _b) = repo_with_log(LONG_LOG);
        let (small_cost, big_cost) = (cost_of(&mut small, &q), cost_of(&mut big, &q));
        assert_eq!(small_cost, big_cost, "the keys read grew with the log ({mode:?})");
    }
}

#[test]
fn a_log_read_bounded_by_revisions_costs_the_same_on_a_longer_log() {
    let q = LogQuery { mode: Mode::Active, revisions: Some(20), ..LogQuery::default() };
    let (mut small, _s) = repo_with_log(SHORT_LOG);
    let (mut big, _b) = repo_with_log(LONG_LOG);
    let (small_cost, big_cost) = (cost_of(&mut small, &q), cost_of(&mut big, &q));
    assert_eq!(small_cost, big_cost, "a listing bounded by revisions grew with the log");
}

#[test]
fn a_revision_bound_returns_whole_revisions_newest_first() {
    let (kv, _dir) = repo_with_log(40);
    let q = LogQuery { mode: Mode::Active, revisions: Some(5), ..LogQuery::default() };
    let body = listing(&kv, &q).unwrap();
    let revisions = body["revisions"].as_array().unwrap();
    assert_eq!(revisions.len(), 5, "asked for 5 revisions, got {}", revisions.len());
    // The newest ones, and every operation shown belongs to one of them.
    let ids: Vec<i64> = revisions.iter().map(|r| r["id"].as_i64().unwrap()).collect();
    assert_eq!(ids, vec![36, 37, 38, 39, 40], "the last five revisions of forty");
    for op in body["operations"].as_array().unwrap() {
        assert!(ids.contains(&op["rev_id"].as_i64().unwrap()));
    }
    assert_eq!(body["total_revisions"], 40, "the totals still cover the whole log");
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
    LogQuery {
        mode: Mode::Active,
        revisions: Some(revisions),
        limit: Some(limit),
        ..LogQuery::default()
    }
}

#[test]
fn a_revision_window_is_bounded_by_operations_too() {
    let (kv, _dir) = repo_after_reconcile(800, 5);
    let body = listing(&kv, &revisions_and_ops(20, 100)).unwrap();
    let ops = body["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 100, "at most the operations asked for");
    let revisions = body["revisions"].as_array().unwrap();
    assert_eq!(revisions.len(), 6, "the five edits, and the reconcile in part");
    // The reconcile is the oldest revision shown, and the one cut: it says so,
    // and how many operations it holds in all.
    let reconcile = &revisions[0];
    assert_eq!(reconcile["partial"], true, "{reconcile}");
    assert_eq!(reconcile["op_count"], 800, "{reconcile}");
    assert_eq!(ops.iter().filter(|o| o["rev_id"] == reconcile["id"]).count(), 95);
    // The whole ones say nothing.
    for whole in &revisions[1..] {
        assert!(whole.get("partial").is_none(), "{whole}");
    }
    // Twenty revisions of one operation each are still all there.
    let (kv, _dir) = repo_with_log(40);
    let body = listing(&kv, &revisions_and_ops(20, 100)).unwrap();
    assert_eq!(body["revisions"].as_array().unwrap().len(), 20);
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

    let q = LogQuery {
        mode: Mode::Active,
        limit: Some(10),
        entity: Some(target.uuid),
        ..LogQuery::default()
    };
    let body = listing(&kv, &q).unwrap();
    let ops = body["operations"].as_array().unwrap();
    assert!(!ops.is_empty(), "the metarecord's own operations were missed by the window");
    for op in ops {
        assert_eq!(op["entity_uuid"].as_str().unwrap(), target.uuid.as_simple().to_string());
    }
}

/// The keys one write reads when it trims 100 revisions of one operation off
/// a log that keeps 20: 18 of one operation, then one of `big` operations
/// (a reconcile's, say), then the write's own.
fn trim_cost(big: usize) -> u64 {
    let (mut kv, _dir) = repo_with_log(118);
    let mut w = Writer::begin(&mut kv, None).unwrap();
    for i in 0..big {
        w.create_metarecord(vec![Field::new("rank", Value::Int(i as i64))]).unwrap();
    }
    w.commit().unwrap();
    let (_, reads) = reads_of(&mut kv, |kv| {
        let retention = Retention { revisions: 20, keep_labels: false };
        let mut w = Writer::begin_with_retention(kv, None, retention).unwrap();
        w.create_metarecord(vec![Field::new("kind", Value::String("new".into()))]).unwrap();
        w.commit().unwrap();
    });
    assert_eq!(kv.counts().unwrap().1, 20, "the trim kept 20 revisions");
    reads
}

/// The retention trim deletes the oldest revisions. What it reads must depend
/// on what it deletes, never on how much the revisions it *keeps* hold: it
/// runs once every `slack` writes, and a trim that reads the whole log reads
/// a reconcile's tens of thousands of operations each time.
#[test]
fn trimming_the_log_does_not_read_the_operations_it_keeps() {
    assert_eq!(trim_cost(1), trim_cost(2_000), "the trim read the operations it keeps");
}

// ── Going back (doc "Script sessions") ────────────────────────────────────
// A walk that writes as it goes takes an answer back by navigating the history
// to where it stood before it (`mf log rollback --id`), one coordinated step
// per operation written. Both assertions below are about *one* such step: what
// it costs must depend on the operation it applies, and on nothing else — not
// on the history behind it, and not on how much of the navigation is left.

/// The operation `n` steps below HEAD, as a navigation target.
fn steps_back(kv: &KvStore, n: usize) -> Option<i64> {
    let head = kv.head().unwrap().expect("a non-empty log");
    Some(kv.ancestry(head).unwrap()[n])
}

/// The keys one step of a navigation towards `n` operations back reads, on a
/// log of `log` revisions. The navigation is planned once, when it starts —
/// what a step costs is the step alone.
fn step_cost(log: usize, n: usize) -> u64 {
    let (mut kv, _dir) = repo_with_log(log);
    let target = steps_back(&kv, n);
    let mut plan = log::NavPlan::new(&kv, target).unwrap();
    reads_of(&mut kv, |kv| plan.step(kv, false).unwrap()).1
}

#[test]
fn one_navigation_step_costs_the_same_on_a_log_sixteen_times_longer() {
    assert_eq!(step_cost(40, 1), step_cost(640, 1), "undoing one operation read the whole log");
}

#[test]
fn a_navigation_step_costs_the_same_however_far_the_target_is() {
    assert_eq!(
        step_cost(400, 5),
        step_cost(400, 80),
        "one step cost more when the target was further away — the path behind \
         it is being read at every step, which makes a navigation of N \
         operations cost N²"
    );
}
