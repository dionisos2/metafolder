//! Watch activity (doc "Watch activity"): how many filesystem
//! events the watcher delivered under each path since the repository was
//! loaded (or the counter last reset), and how many operations its flushes
//! wrote for them. In memory only — a diagnosis of where the load comes from
//! right now, not a history.
//!
//! The two numbers answer different questions. An *event* is what the kernel
//! sends: a large copy is thousands of them and costs next to nothing, since
//! they compact into one modification. An *operation* is what reached the log:
//! it is durable, spends the retention budget and buries the user's own
//! changes — a file touched every few seconds is few events and a revision
//! each time.
//!
//! Counts are *recursive*: an event at `/a/b/c` counts once on `/a/b/c`, `/a/b`,
//! `/a` and the root, so the root's count is the total and a client can walk
//! down from it to where most events come from. A rename counts once on every
//! path along either side (the common ancestors once).

use std::collections::{HashMap, HashSet};

use crate::executor::FsEvent;
use crate::relpath::RelPath;

/// The most paths the counter keeps. A build tree full of hash-named
/// directories would otherwise grow the map for ever; past the cap the least
/// active paths are dropped (see [`WatchActivity::evict`]).
pub const DEFAULT_CAP: usize = 100_000;

/// What one path counted, itself and everything below it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Counts {
    events: u64,
    operations: u64,
}

/// Which of the two counts ranks a listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    /// The events the watcher delivered.
    Events,
    /// The operations the flushes wrote.
    Operations,
}

/// A direct child of a listed path, with both its counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Child {
    pub path: RelPath,
    pub events: u64,
    pub operations: u64,
}

/// The recursive per-path counter of one repository.
#[derive(Debug)]
pub struct WatchActivity {
    counts: HashMap<RelPath, Counts>,
    since_ms: i64,
    cap: usize,
}

impl WatchActivity {
    /// An empty counter started at `now_ms`, keeping at most `cap` paths.
    pub fn new(now_ms: i64, cap: usize) -> Self {
        Self { counts: HashMap::new(), since_ms: now_ms, cap: cap.max(1) }
    }

    /// Counts a batch of events as the watcher delivered them.
    pub fn record(&mut self, events: &[(FsEvent, Option<i64>)]) {
        let mut touched: HashSet<RelPath> = HashSet::new();
        for (event, _) in events {
            touched.clear();
            for path in event_paths(event) {
                add_chain(&mut touched, path);
            }
            for path in touched.drain() {
                self.counts.entry(path).or_default().events += 1;
            }
        }
        self.bound();
    }

    /// Counts the operations a flush wrote: each entry is the path(s) of one
    /// applied event (see [`event_paths`]) and how many operations applying it
    /// recorded. Called once the revision is committed — what was rolled back
    /// cost nothing.
    pub fn record_operations(&mut self, written: &[(Vec<RelPath>, u64)]) {
        let mut touched: HashSet<RelPath> = HashSet::new();
        for (paths, operations) in written {
            touched.clear();
            for path in paths {
                add_chain(&mut touched, path);
            }
            for path in touched.drain() {
                self.counts.entry(path).or_default().operations += operations;
            }
        }
        self.bound();
    }

    fn bound(&mut self) {
        if self.counts.len() > self.cap {
            self.evict();
        }
    }

    /// The events counted under `path` (itself included).
    pub fn count(&self, path: &RelPath) -> u64 {
        self.counts.get(path).map_or(0, |c| c.events)
    }

    /// The operations written for the events under `path` (itself included).
    pub fn operations(&self, path: &RelPath) -> u64 {
        self.counts.get(path).map_or(0, |c| c.operations)
    }

    /// Every event counted: the root's count.
    pub fn total(&self) -> u64 {
        self.count(&RelPath::root())
    }

    /// Every operation counted: the root's.
    pub fn total_operations(&self) -> u64 {
        self.operations(&RelPath::root())
    }

    /// The direct children of `path` that counted anything, the largest
    /// `by` first (ties by the other count, then by path), at most `limit`.
    pub fn children(&self, path: &RelPath, limit: usize, by: Metric) -> Vec<Child> {
        let depth = path.depth() + 1;
        let mut out: Vec<Child> = self
            .counts
            .iter()
            .filter(|(p, _)| p.depth() == depth && p.parent() == *path)
            .map(|(p, c)| Child { path: p.clone(), events: c.events, operations: c.operations })
            .collect();
        let key = |c: &Child| match by {
            Metric::Events => (c.events, c.operations),
            Metric::Operations => (c.operations, c.events),
        };
        out.sort_by(|a, b| key(b).cmp(&key(a)).then_with(|| a.path.cmp(&b.path)));
        out.truncate(limit);
        out
    }

    /// When counting started (Unix ms): the load, or the last [`Self::reset`].
    pub fn since_ms(&self) -> i64 {
        self.since_ms
    }

    /// Forgets every count and restarts at `now_ms`.
    pub fn reset(&mut self, now_ms: i64) {
        self.counts.clear();
        self.since_ms = now_ms;
    }

    /// How many paths are held.
    pub fn len(&self) -> usize {
        self.counts.len()
    }

    /// Whether nothing has been counted.
    pub fn is_empty(&self) -> bool {
        self.counts.is_empty()
    }

    /// Drops the least active paths down to three quarters of the cap, so the
    /// sort is paid once per quarter-cap of new paths rather than per event.
    ///
    /// Ordered by weight — events plus operations, so a path that costs the
    /// log is kept over a merely noisy one — then deepest first: an ancestor
    /// has counted everything its descendants did, in both counts, so it always
    /// sorts after all of them, and what is dropped is closed under
    /// descendants — no surviving path is ever left without its parent. A
    /// dropped path that becomes active again restarts from zero: small counts
    /// are approximate, large ones exact.
    fn evict(&mut self) {
        let keep = self.cap * 3 / 4;
        let mut order: Vec<(u64, usize, RelPath)> = self
            .counts
            .iter()
            .map(|(p, c)| (c.events.saturating_add(c.operations), p.depth(), p.clone()))
            .collect();
        order.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)));
        let drop = order.len().saturating_sub(keep);
        for (_, _, path) in order.into_iter().take(drop) {
            if !path.is_root() {
                self.counts.remove(&path);
            }
        }
    }
}

/// The path(s) an event is about: both sides of a whole rename, one otherwise.
pub fn event_paths(event: &FsEvent) -> Vec<&RelPath> {
    match event {
        FsEvent::Create(p)
        | FsEvent::Remove(p)
        | FsEvent::RenameFrom(p)
        | FsEvent::RenameTo(p)
        | FsEvent::ModifyData(p)
        | FsEvent::ModifyMeta(p) => vec![p],
        FsEvent::Rename(a, b) => vec![a, b],
    }
}

/// Adds `path` and every ancestor up to the root.
fn add_chain(into: &mut HashSet<RelPath>, path: &RelPath) {
    let mut cur = path.clone();
    loop {
        let root = cur.is_root();
        let parent = cur.parent();
        if !into.insert(cur) {
            // Its ancestors are already in from the other side of a rename.
            return;
        }
        if root {
            return;
        }
        cur = parent;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RelPath {
        RelPath::from_display(s)
    }

    fn ev(e: FsEvent) -> (FsEvent, Option<i64>) {
        (e, None)
    }

    #[test]
    fn an_event_counts_on_its_path_and_every_ancestor() {
        let mut a = WatchActivity::new(0, DEFAULT_CAP);
        a.record(&[ev(FsEvent::ModifyData(p("/a/b/c.txt")))]);
        assert_eq!(a.count(&p("/a/b/c.txt")), 1);
        assert_eq!(a.count(&p("/a/b")), 1);
        assert_eq!(a.count(&p("/a")), 1);
        assert_eq!(a.total(), 1);
        assert_eq!(a.count(&p("/other")), 0);
    }

    #[test]
    fn counts_accumulate_and_the_root_is_the_total() {
        let mut a = WatchActivity::new(0, DEFAULT_CAP);
        a.record(&[
            ev(FsEvent::Create(p("/a/x"))),
            ev(FsEvent::ModifyData(p("/a/x"))),
            ev(FsEvent::Remove(p("/b/y"))),
        ]);
        assert_eq!(a.count(&p("/a")), 2);
        assert_eq!(a.count(&p("/b")), 1);
        assert_eq!(a.total(), 3);
    }

    #[test]
    fn a_rename_counts_once_on_the_common_ancestors() {
        let mut a = WatchActivity::new(0, DEFAULT_CAP);
        a.record(&[ev(FsEvent::Rename(p("/a/b/x"), p("/a/c/y")))]);
        assert_eq!(a.total(), 1);
        assert_eq!(a.count(&p("/a")), 1);
        assert_eq!(a.count(&p("/a/b")), 1);
        assert_eq!(a.count(&p("/a/c/y")), 1);
    }

    #[test]
    fn children_are_the_direct_ones_most_active_first() {
        let mut a = WatchActivity::new(0, DEFAULT_CAP);
        a.record(&[
            ev(FsEvent::ModifyData(p("/a/deep/x"))),
            ev(FsEvent::ModifyData(p("/b"))),
            ev(FsEvent::ModifyData(p("/b"))),
            ev(FsEvent::ModifyData(p("/a/deep/y"))),
            ev(FsEvent::ModifyData(p("/a/deep/z"))),
            ev(FsEvent::ModifyData(p("/c"))),
        ]);
        let events = |path: &RelPath, limit| -> Vec<(RelPath, u64)> {
            a.children(path, limit, Metric::Events)
                .into_iter()
                .map(|c| (c.path, c.events))
                .collect()
        };
        assert_eq!(events(&RelPath::root(), 10), vec![(p("/a"), 3), (p("/b"), 2), (p("/c"), 1)]);
        assert_eq!(events(&RelPath::root(), 2).len(), 2);
        assert_eq!(events(&p("/a"), 10), vec![(p("/a/deep"), 3)]);
    }

    #[test]
    fn operations_count_apart_from_events_on_the_path_and_every_ancestor() {
        let mut a = WatchActivity::new(0, DEFAULT_CAP);
        a.record(&[ev(FsEvent::ModifyData(p("/a/b/c.txt"))), ev(FsEvent::ModifyData(p("/d")))]);
        a.record_operations(&[(vec![p("/a/b/c.txt")], 3)]);
        assert_eq!(a.operations(&p("/a/b/c.txt")), 3);
        assert_eq!(a.operations(&p("/a")), 3);
        assert_eq!(a.total_operations(), 3);
        assert_eq!(a.operations(&p("/d")), 0);
        // The events are untouched.
        assert_eq!(a.count(&p("/a")), 1);
        assert_eq!(a.total(), 2);
    }

    #[test]
    fn a_rename_s_operations_count_once_on_the_common_ancestors() {
        let mut a = WatchActivity::new(0, DEFAULT_CAP);
        a.record_operations(&[(vec![p("/a/b/x"), p("/a/c/y")], 2)]);
        assert_eq!(a.total_operations(), 2);
        assert_eq!(a.operations(&p("/a")), 2);
        assert_eq!(a.operations(&p("/a/b")), 2);
        assert_eq!(a.operations(&p("/a/c/y")), 2);
    }

    #[test]
    fn children_can_be_ranked_by_operations() {
        let mut a = WatchActivity::new(0, DEFAULT_CAP);
        for _ in 0..5 {
            a.record(&[ev(FsEvent::ModifyData(p("/noisy/x")))]);
        }
        a.record(&[ev(FsEvent::ModifyData(p("/costly/y")))]);
        a.record_operations(&[(vec![p("/noisy/x")], 1), (vec![p("/costly/y")], 9)]);
        let by_events = a.children(&RelPath::root(), 10, Metric::Events);
        assert_eq!(
            by_events.iter().map(|c| c.path.clone()).collect::<Vec<_>>(),
            [p("/noisy"), p("/costly")]
        );
        let by_ops = a.children(&RelPath::root(), 10, Metric::Operations);
        assert_eq!(
            by_ops,
            vec![
                Child { path: p("/costly"), events: 1, operations: 9 },
                Child { path: p("/noisy"), events: 5, operations: 1 },
            ]
        );
    }

    #[test]
    fn reset_forgets_everything_and_restarts_the_clock() {
        let mut a = WatchActivity::new(5, DEFAULT_CAP);
        a.record(&[ev(FsEvent::Create(p("/a")))]);
        a.record_operations(&[(vec![p("/a")], 4)]);
        a.reset(42);
        assert_eq!(a.total(), 0);
        assert_eq!(a.total_operations(), 0);
        assert!(a.is_empty());
        assert_eq!(a.since_ms(), 42);
    }

    #[test]
    fn past_the_cap_the_least_active_paths_go_and_ancestors_stay() {
        let mut a = WatchActivity::new(0, 10);
        for _ in 0..5 {
            a.record(&[ev(FsEvent::ModifyData(p("/hot/f")))]);
        }
        for i in 0..20 {
            a.record(&[ev(FsEvent::Create(p(&format!("/cold/h{i}/x"))))]);
        }
        assert!(a.len() <= 10, "{} paths held", a.len());
        // The root and the busy subtree survive, with exact counts.
        assert_eq!(a.total(), 25);
        assert_eq!(a.count(&p("/hot")), 5);
        assert_eq!(a.count(&p("/hot/f")), 5);
        assert_eq!(a.count(&p("/cold")), 20);
        // No surviving path is ever above an ancestor that was dropped.
        for (path, n) in &a.counts {
            if !path.is_root() {
                assert!(a.count(&path.parent()) >= n.events, "{} orphaned", path.display());
            }
        }
    }

    #[test]
    fn a_path_that_costs_operations_outlives_the_merely_noisy_ones() {
        let mut a = WatchActivity::new(0, 10);
        a.record(&[ev(FsEvent::ModifyData(p("/costly/f")))]);
        a.record_operations(&[(vec![p("/costly/f")], 50)]);
        for i in 0..20 {
            a.record(&[ev(FsEvent::Create(p(&format!("/cold/h{i}/x"))))]);
        }
        assert!(a.len() <= 10, "{} paths held", a.len());
        assert_eq!(a.operations(&p("/costly/f")), 50);
        assert_eq!(a.count(&p("/costly/f")), 1);
    }
}
