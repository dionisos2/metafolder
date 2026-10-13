//! Automatic log retention (doc "Automatic retention"): the log keeps a
//! bounded number of operations per origin — the watcher's, and everything
//! else — the oldest of each falling off as new ones arrive.

use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::{Retention, Writer, ORIGIN_WATCHER};
use metafolder_daemon::store::Log;

mod common;

fn test_conn() -> (KvStore, common::TempDir) {
    common::kv::store()
}

/// One write holding one operation, the watcher's when `watcher`.
fn write(conn: &mut KvStore, retention: Retention, watcher: bool) -> i64 {
    let mut w = Writer::begin_with_retention(conn, None, retention).unwrap();
    if watcher {
        w.set_origin(ORIGIN_WATCHER).unwrap();
    }
    w.create_metarecord(vec![Field::new("n", Value::Int(1))]).unwrap();
    w.commit().unwrap();
    conn.head().unwrap().unwrap()
}

fn ops(conn: &KvStore, watcher: bool) -> usize {
    conn.all_ops()
        .unwrap()
        .iter()
        .filter(|o| (o.origin.as_deref() == Some(ORIGIN_WATCHER)) == watcher)
        .count()
}

const fn keep(watcher_ops: u64, user_ops: u64) -> Retention {
    Retention { watcher_ops, user_ops }
}

#[test]
fn test_unlimited_retention_keeps_every_operation() {
    let (mut conn, _dir) = test_conn();
    for i in 0..50 {
        write(&mut conn, Retention::UNLIMITED, i % 2 == 0);
    }
    assert_eq!(conn.counts().unwrap().0, 50);
}

#[test]
fn test_retention_trims_the_oldest_operations_of_an_origin() {
    // With a limit of 10, 60 operations written leave the log bounded: the
    // limit itself plus at most the slack the trim tolerates.
    let (mut conn, _dir) = test_conn();
    let mut written = Vec::new();
    for _ in 0..60 {
        written.push(write(&mut conn, keep(0, 10), false));
    }
    let kept = ops(&conn, false);
    assert!(
        (10..=10 + Retention::slack(10) as usize).contains(&kept),
        "expected the log bounded around 10 operations, kept {kept}"
    );
    let survivors: Vec<i64> = conn.all_ops().unwrap().iter().map(|o| o.id).collect();
    assert_eq!(survivors, written[written.len() - kept..], "the newest are what is kept");
}

/// The point of a limit per origin: the watcher's churn does not push the
/// user's own history out.
#[test]
fn test_the_watchers_churn_does_not_take_the_users_history() {
    let (mut conn, _dir) = test_conn();
    let retention = keep(5, 1_000);
    let mine: Vec<i64> = (0..3).map(|_| write(&mut conn, retention, false)).collect();
    for _ in 0..100 {
        write(&mut conn, retention, true);
    }
    for op in mine {
        assert!(conn.op(op).unwrap().is_some(), "user operation {op} was trimmed");
    }
    let watcher = ops(&conn, true);
    assert!(watcher <= 5 + Retention::slack(5) as usize, "kept {watcher} watcher operations");
}

#[test]
fn test_trim_drops_the_snapshots_of_pruned_operations() {
    // Snapshots are the bulk of the log on disk; a trim that left them behind
    // would bound the operation count and nothing else.
    let (mut conn, _dir) = test_conn();
    let written: Vec<i64> = (0..40).map(|_| write(&mut conn, keep(0, 5), false)).collect();
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
fn test_a_write_left_with_no_operation_goes_with_its_last_one() {
    let (mut conn, _dir) = test_conn();
    for _ in 0..40 {
        write(&mut conn, keep(0, 5), false);
    }
    let (operations, groups) = conn.counts().unwrap();
    assert_eq!(operations, groups, "one operation per write: as many groups as operations");
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
    opened.config.log_retention_user_ops = Some(4);
    let settings = DaemonSettings {
        log_retention_watcher_ops: 900,
        log_retention_user_ops: 900,
        ..DaemonSettings::default()
    };
    let state = RepoState::from_opened_with(opened, &settings);
    assert_eq!(
        state.log_retention(),
        keep(900, 4),
        "each key resolves on its own: the repository overrides one, the daemon decides the other"
    );

    for _ in 0..30 {
        let mut conn = state.conn.lock().unwrap();
        let mut w =
            state.writer(&mut conn, metafolder_daemon::log::WriteContext::new("test")).unwrap();
        w.create_metarecord(vec![Field::new("n", Value::Int(1))]).unwrap();
        w.commit().unwrap();
    }
    let conn = state.conn.lock().unwrap();
    let kept = metafolder_daemon::store::Log::counts(&*conn).unwrap().0;
    assert!(kept <= 4 + Retention::slack(4) as i64, "kept {kept} operations");
}
