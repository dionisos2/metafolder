//! Automatic log retention (spec-event-log "Automatic retention"): the log
//! keeps a bounded number of revisions behind HEAD, the oldest falling off as
//! new ones arrive.

use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::db;
use metafolder_daemon::log::{self, Retention, Writer};
use rusqlite::Connection;

fn test_conn() -> Connection {
    let conn = db::open_in_memory().unwrap();
    db::init_schema(&conn).unwrap();
    conn
}

/// One revision holding one operation.
fn write_revision(conn: &mut Connection, retention: Retention, label: Option<&str>) {
    let mut w = Writer::begin_with_retention(conn, label.map(str::to_string), retention).unwrap();
    w.create_metarecord(vec![Field::new("n", Value::Int(1))]).unwrap();
    w.commit().unwrap();
}

fn revisions(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM revision", [], |r| r.get(0)).unwrap()
}

fn operations(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM operation", [], |r| r.get(0)).unwrap()
}

/// Operations whose parent no longer exists — the log must never contain one.
fn orphan_ops(conn: &Connection) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM operation o WHERE o.parent_id IS NOT NULL \
         AND NOT EXISTS (SELECT 1 FROM operation p WHERE p.id = o.parent_id)",
        [],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn test_unlimited_retention_keeps_every_revision() {
    // The default: nothing is ever dropped (spec-event-log "No history is lost").
    let mut conn = test_conn();
    for _ in 0..50 {
        write_revision(&mut conn, Retention::UNLIMITED, None);
    }
    assert_eq!(revisions(&conn), 50);
}

#[test]
fn test_retention_trims_the_oldest_revisions() {
    // With a limit of 10, 60 revisions written leave the log bounded: the limit
    // itself plus at most the slack the trim tolerates before running again.
    let mut conn = test_conn();
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
    let mut conn = test_conn();
    let retention = Retention { revisions: 5, keep_labels: false };
    for _ in 0..40 {
        write_revision(&mut conn, retention, None);
    }
    let head = log::get_head(&conn).unwrap().expect("a HEAD");
    let chain = log::ancestry(&conn, head).unwrap();
    assert_eq!(chain.len() as i64, operations(&conn), "every surviving op is an ancestor of HEAD");
    let root = *chain.last().unwrap();
    let parent: Option<i64> = conn
        .query_row("SELECT parent_id FROM operation WHERE id = ?1", [root], |r| r.get(0))
        .unwrap();
    assert_eq!(parent, None, "the oldest surviving operation is the new root");
}

#[test]
fn test_trim_drops_the_snapshots_of_pruned_operations() {
    // op_snapshot is the bulk of the log on disk; a trim that left them behind
    // would bound the revision count and nothing else.
    let mut conn = test_conn();
    let retention = Retention { revisions: 5, keep_labels: false };
    for _ in 0..40 {
        write_revision(&mut conn, retention, None);
    }
    let dangling: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM op_snapshot s \
             WHERE NOT EXISTS (SELECT 1 FROM operation o WHERE o.id = s.op_id)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(dangling, 0);
}

#[test]
fn test_trim_takes_the_branches_hanging_below_the_cutoff() {
    // A branch rooted before the cutoff cannot survive it: its ancestry is
    // gone. It must go with it, not become an orphan.
    let mut conn = test_conn();
    for _ in 0..10 {
        write_revision(&mut conn, Retention::UNLIMITED, None);
    }
    // A second operation parented on the third one: a divergent branch.
    let third: i64 = conn
        .query_row("SELECT id FROM operation ORDER BY id LIMIT 1 OFFSET 2", [], |r| r.get(0))
        .unwrap();
    let head = log::get_head(&conn).unwrap().unwrap();
    conn.execute("UPDATE log_head SET op_id = ?1 WHERE singleton = 1", [third]).unwrap();
    write_revision(&mut conn, Retention::UNLIMITED, None);
    let branch = log::get_head(&conn).unwrap().unwrap();
    conn.execute("UPDATE log_head SET op_id = ?1 WHERE singleton = 1", [head]).unwrap();

    let retention = Retention { revisions: 3, keep_labels: false };
    for _ in 0..10 {
        write_revision(&mut conn, retention, None);
    }
    let alive: i64 = conn
        .query_row("SELECT COUNT(*) FROM operation WHERE id = ?1", [branch], |r| r.get(0))
        .unwrap();
    assert_eq!(alive, 0, "the branch below the cutoff is gone");
    assert_eq!(orphan_ops(&conn), 0);
}

#[test]
fn test_trim_removes_labelled_revisions_by_default() {
    let mut conn = test_conn();
    let retention = Retention { revisions: 5, keep_labels: false };
    write_revision(&mut conn, retention, Some("checkpoint"));
    for _ in 0..40 {
        write_revision(&mut conn, retention, None);
    }
    let labelled: i64 = conn
        .query_row("SELECT COUNT(*) FROM revision WHERE label IS NOT NULL", [], |r| r.get(0))
        .unwrap();
    assert_eq!(labelled, 0, "keep_labels = false lets a checkpoint fall off");
}

#[test]
fn test_trim_stops_at_the_oldest_label_when_asked() {
    // keep_labels: a named checkpoint is a floor — nothing older than it is
    // dropped, so the log grows past the limit rather than losing it.
    let mut conn = test_conn();
    let retention = Retention { revisions: 5, keep_labels: true };
    write_revision(&mut conn, retention, Some("checkpoint"));
    for _ in 0..40 {
        write_revision(&mut conn, retention, None);
    }
    let labelled: i64 = conn
        .query_row("SELECT COUNT(*) FROM revision WHERE label IS NOT NULL", [], |r| r.get(0))
        .unwrap();
    assert_eq!(labelled, 1, "the checkpoint survives");
    assert_eq!(revisions(&conn), 41, "and so does everything after it");
}

#[test]
fn test_trim_is_skipped_when_the_cutoff_is_not_an_ancestor_of_head() {
    // After a rollback, the newest revisions by id sit on the abandoned branch.
    // Cutting there would delete HEAD's own ancestry: the trim must decline.
    let mut conn = test_conn();
    for _ in 0..30 {
        write_revision(&mut conn, Retention::UNLIMITED, None);
    }
    let fifth: i64 = conn
        .query_row("SELECT id FROM operation ORDER BY id LIMIT 1 OFFSET 4", [], |r| r.get(0))
        .unwrap();
    conn.execute("UPDATE log_head SET op_id = ?1 WHERE singleton = 1", [fifth]).unwrap();

    let before = operations(&conn);
    write_revision(&mut conn, Retention { revisions: 10, keep_labels: false }, None);
    assert_eq!(operations(&conn), before + 1, "nothing was pruned");
    assert_eq!(log::ancestry(&conn, log::get_head(&conn).unwrap().unwrap()).unwrap().len(), 6);
}

// ── Configuration ─────────────────────────────────────────────────────────────

mod common;
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
    let settings = DaemonSettings { log_retention_revisions: 900, ..DaemonSettings::default() };
    let state = RepoState::from_opened_with(opened, &settings);
    assert_eq!(
        state.log_retention(),
        Retention { revisions: 4, keep_labels: false },
        "the repository's own override wins over the daemon default"
    );

    for _ in 0..30 {
        let mut conn = state.conn.lock().unwrap();
        let mut w = state.writer(&mut conn, None).unwrap();
        w.create_metarecord(vec![Field::new("n", Value::Int(1))]).unwrap();
        w.commit().unwrap();
    }
    let conn = state.conn.lock().unwrap();
    let kept: i64 = conn.query_row("SELECT COUNT(*) FROM revision", [], |r| r.get(0)).unwrap();
    assert!(kept <= 4 + Retention::slack(4) as i64, "kept {kept} revisions");
}
