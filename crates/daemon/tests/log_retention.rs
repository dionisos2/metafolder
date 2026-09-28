//! Automatic log retention (spec-event-log "Automatic retention"): the log
//! keeps a bounded number of revisions behind HEAD, the oldest falling off as
//! new ones arrive.

use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::{Retention, Writer};
use metafolder_daemon::store::{Begin, Log};

mod common;

fn test_conn() -> (KvStore, common::TempDir) {
    common::kv::store()
}

/// One revision holding one operation.
fn write_revision(conn: &mut KvStore, retention: Retention, label: Option<&str>) {
    let mut w = Writer::begin_with_retention(conn, label.map(str::to_string), retention).unwrap();
    w.create_metarecord(vec![Field::new("n", Value::Int(1))]).unwrap();
    w.commit().unwrap();
}

fn revisions(conn: &KvStore) -> i64 {
    conn.counts().unwrap().1
}

fn operations(conn: &KvStore) -> i64 {
    conn.counts().unwrap().0
}

/// Operations whose parent no longer exists — the log must never contain one.
fn orphan_ops(conn: &KvStore) -> usize {
    let ops = conn.all_ops().unwrap();
    let ids: std::collections::HashSet<i64> = ops.iter().map(|o| o.id).collect();
    ops.iter().filter(|o| o.parent_id.is_some_and(|p| !ids.contains(&p))).count()
}

/// The id of the `n`-th operation still in the log, oldest first.
fn nth_op(conn: &KvStore, n: usize) -> i64 {
    let mut ids: Vec<i64> = conn.all_ops().unwrap().iter().map(|o| o.id).collect();
    ids.sort();
    ids[n]
}

/// Moves HEAD without writing anything, as a navigation leaves it.
fn set_head(conn: &mut KvStore, op: i64) {
    let txn = conn.begin_write().unwrap();
    txn.set_head(Some(op)).unwrap();
    txn.commit().unwrap();
}

/// How many revisions still in the log carry a label.
fn labelled(conn: &KvStore) -> usize {
    let revs: Vec<i64> = conn.all_ops().unwrap().iter().map(|o| o.rev_id).collect();
    conn.revisions(&revs).unwrap().values().filter(|m| m.label.is_some()).count()
}

#[test]
fn test_unlimited_retention_keeps_every_revision() {
    // The default: nothing is ever dropped (spec-event-log "No history is lost").
    let (mut conn, _dir) = test_conn();
    for _ in 0..50 {
        write_revision(&mut conn, Retention::UNLIMITED, None);
    }
    assert_eq!(revisions(&conn), 50);
}

#[test]
fn test_retention_trims_the_oldest_revisions() {
    // With a limit of 10, 60 revisions written leave the log bounded: the limit
    // itself plus at most the slack the trim tolerates before running again.
    let (mut conn, _dir) = test_conn();
    let retention = Retention { revisions: 10, keep_labels: false };
    for _ in 0..60 {
        write_revision(&mut conn, retention, None);
    }
    let kept = revisions(&conn);
    assert!(
        (10..=10 + Retention::slack(10) as i64).contains(&kept),
        "expected the log bounded around 10 revisions, kept {kept}"
    );
    assert_eq!(orphan_ops(&conn), 0);
}

#[test]
fn test_trimmed_log_keeps_head_and_a_walkable_ancestry() {
    // What survives is the *newest* history: HEAD, its whole ancestry, and a
    // root whose parent is NULL (the weak root of a prune-before).
    let (mut conn, _dir) = test_conn();
    let retention = Retention { revisions: 5, keep_labels: false };
    for _ in 0..40 {
        write_revision(&mut conn, retention, None);
    }
    let head = conn.head().unwrap().expect("a HEAD");
    let chain = conn.ancestry(head).unwrap();
    assert_eq!(chain.len() as i64, operations(&conn), "every surviving op is an ancestor of HEAD");
    let root = *chain.last().unwrap();
    let parent = conn.op(root).unwrap().expect("the root is in the log").parent_id;
    assert_eq!(parent, None, "the oldest surviving operation is the new root");
}

#[test]
fn test_trim_drops_the_snapshots_of_pruned_operations() {
    // op_snapshot is the bulk of the log on disk; a trim that left them behind
    // would bound the revision count and nothing else.
    let (mut conn, _dir) = test_conn();
    let retention = Retention { revisions: 5, keep_labels: false };
    let mut written = Vec::new();
    for _ in 0..40 {
        write_revision(&mut conn, retention, None);
        written.push(conn.head().unwrap().unwrap());
    }
    let pruned: Vec<i64> =
        written.into_iter().filter(|&op| conn.op(op).unwrap().is_none()).collect();
    assert!(!pruned.is_empty(), "the trim pruned nothing");
    for op in pruned {
        for after in [false, true] {
            assert!(conn.snapshots(op, after).unwrap().is_empty(), "op {op} left snapshots");
        }
    }
}

#[test]
fn test_trim_takes_the_branches_hanging_below_the_cutoff() {
    // A branch rooted before the cutoff cannot survive it: its ancestry is
    // gone. It must go with it, not become an orphan.
    let (mut conn, _dir) = test_conn();
    for _ in 0..10 {
        write_revision(&mut conn, Retention::UNLIMITED, None);
    }
    // A second operation parented on the third one: a divergent branch.
    let third = nth_op(&conn, 2);
    let head = conn.head().unwrap().unwrap();
    set_head(&mut conn, third);
    write_revision(&mut conn, Retention::UNLIMITED, None);
    let branch = conn.head().unwrap().unwrap();
    set_head(&mut conn, head);

    let retention = Retention { revisions: 3, keep_labels: false };
    for _ in 0..10 {
        write_revision(&mut conn, retention, None);
    }
    assert!(conn.op(branch).unwrap().is_none(), "the branch below the cutoff is gone");
    assert_eq!(orphan_ops(&conn), 0);
}

#[test]
fn test_trim_removes_labelled_revisions_by_default() {
    let (mut conn, _dir) = test_conn();
    let retention = Retention { revisions: 5, keep_labels: false };
    write_revision(&mut conn, retention, Some("checkpoint"));
    for _ in 0..40 {
        write_revision(&mut conn, retention, None);
    }
    assert_eq!(labelled(&conn), 0, "keep_labels = false lets a checkpoint fall off");
}

#[test]
fn test_trim_stops_at_the_oldest_label_when_asked() {
    // keep_labels: a named checkpoint is a floor — nothing older than it is
    // dropped, so the log grows past the limit rather than losing it.
    let (mut conn, _dir) = test_conn();
    let retention = Retention { revisions: 5, keep_labels: true };
    write_revision(&mut conn, retention, Some("checkpoint"));
    for _ in 0..40 {
        write_revision(&mut conn, retention, None);
    }
    assert_eq!(labelled(&conn), 1, "the checkpoint survives");
    assert_eq!(revisions(&conn), 41, "and so does everything after it");
}

#[test]
fn test_trim_is_skipped_when_the_cutoff_is_not_an_ancestor_of_head() {
    // After a rollback, the newest revisions by id sit on the abandoned branch.
    // Cutting there would delete HEAD's own ancestry: the trim must decline.
    let (mut conn, _dir) = test_conn();
    for _ in 0..30 {
        write_revision(&mut conn, Retention::UNLIMITED, None);
    }
    let fifth = nth_op(&conn, 4);
    set_head(&mut conn, fifth);

    let before = operations(&conn);
    write_revision(&mut conn, Retention { revisions: 10, keep_labels: false }, None);
    assert_eq!(operations(&conn), before + 1, "nothing was pruned");
    assert_eq!(conn.ancestry(conn.head().unwrap().unwrap()).unwrap().len(), 6);
}

// ── Configuration ─────────────────────────────────────────────────────────────

use common::TempDir;

/// The policy a loaded repository actually writes with: its `config.json`
/// override where set, the daemon's `[settings]` otherwise.
#[test]
fn test_a_repository_writes_with_its_configured_retention() {
    use metafolder_daemon::daemon_config::DaemonSettings;
    use metafolder_daemon::repo;
    use metafolder_daemon::state::RepoState;

    let root = TempDir::new("log_retention_repo");
    let mut opened = repo::init_repository(&root, None, None, false).unwrap();
    opened.config.log_retention_revisions = Some(4);
    let settings = DaemonSettings {
        log_retention_revisions: 900,
        log_retention_keep_labels: true,
        ..DaemonSettings::default()
    };
    let state = RepoState::from_opened_with(opened, &settings);
    assert_eq!(
        state.log_retention(),
        Retention { revisions: 4, keep_labels: true },
        "each key resolves on its own: the repository overrides the count, \
         the daemon still decides about labels"
    );

    for _ in 0..30 {
        let mut conn = state.conn.lock().unwrap();
        let mut w = state.writer(&mut conn, None).unwrap();
        w.create_metarecord(vec![Field::new("n", Value::Int(1))]).unwrap();
        w.commit().unwrap();
    }
    let conn = state.conn.lock().unwrap();
    let kept = metafolder_daemon::store::Log::counts(&*conn).unwrap().1;
    assert!(kept <= 4 + Retention::slack(4) as i64, "kept {kept} revisions");
}
