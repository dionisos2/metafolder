//! Cost assertions: what an operation *does*, not how long it takes
//! (spec-perf "Cost assertions").
//!
//! Every test here counts SQL statements and reads query plans. Nothing is
//! timed, so nothing flakes under load, and the suite runs in the ordinary
//! `cargo test` pass — which is the point: an algorithmic regression is caught
//! by the same run that catches a broken assertion, on a repository small
//! enough to build in a second.
//!
//! Two shapes of assertion:
//!
//! - **Growth invariance** — the same bounded operation, run against a log of
//!   `N` and of `16N`, must execute the same number of statements. A count that
//!   grows with the data is the N+1 family.
//! - **The full-scan whitelist** — every statement is replayed through
//!   `EXPLAIN QUERY PLAN`, and the ones that scan a whole table are compared
//!   against an explicit list. A new full scan on a bounded read fails here,
//!   and adding one takes an argued edit to the list.

use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::db;
use metafolder_daemon::log::Writer;
use metafolder_daemon::log_view::{listing, LogQuery, Mode};
use rusqlite::Connection;

mod common;
use common::sqlcost::{measure, SqlCost};

/// A repository whose log holds `revisions` revisions of one operation each —
/// the shape a long-lived repository ends up with, where almost every write is
/// its own revision (a manual edit, a watcher flush).
fn repo_with_log(revisions: usize) -> Connection {
    let mut conn = db::open_in_memory().unwrap();
    db::init_schema(&conn).unwrap();
    for i in 0..revisions {
        let mut w = Writer::begin(&mut conn, None).unwrap();
        w.create_metarecord(vec![
            Field::new("kind", Value::String("file".into())),
            Field::new("rank", Value::Int(i as i64)),
        ])
        .unwrap();
        w.commit().unwrap();
    }
    conn
}

/// The full scans a log read is allowed to do.
///
/// Both are the repository-wide totals the response carries (`total_operations`
/// / `total_revisions`), which a client showing a window needs to say how much
/// log there is behind it. A count over a primary key is a covering-index scan,
/// the cheapest form of "look at every row", and nothing else in a bounded read
/// may look at every row at all.
fn allowed_full_scans() -> Vec<(String, String)> {
    vec![
        ("SELECT COUNT(*) FROM operation".to_string(), "operation".to_string()),
        ("SELECT COUNT(*) FROM revision".to_string(), "revision".to_string()),
    ]
}

fn bounded(mode: Mode) -> LogQuery {
    LogQuery { mode, limit: Some(50), ..LogQuery::default() }
}

/// Measures one listing and returns what it cost.
fn cost_of(conn: &mut Connection, q: &LogQuery) -> SqlCost {
    let (result, cost) = measure(conn, |c| listing(c, q).unwrap());
    assert!(result["operations"].is_array(), "the listing must answer");
    cost
}

#[test]
fn bounded_log_read_scans_no_table() {
    for mode in [Mode::Linear, Mode::Active] {
        let mut conn = repo_with_log(400);
        let q = bounded(mode);
        let cost = cost_of(&mut conn, &q);
        let scans = cost.full_scans(&conn);
        assert_eq!(
            scans,
            allowed_full_scans(),
            "a window of 50 operations read every row of a table ({mode:?}).\n{}",
            cost.report(&conn)
        );
    }
}

#[test]
fn bounded_log_read_costs_the_same_on_a_log_sixteen_times_longer() {
    for mode in [Mode::Linear, Mode::Active] {
        let q = bounded(mode);
        let mut small = repo_with_log(40);
        let mut big = repo_with_log(640);
        let small_cost = cost_of(&mut small, &q);
        let big_cost = cost_of(&mut big, &q);
        assert_eq!(
            small_cost.count(),
            big_cost.count(),
            "the number of statements grew with the log ({mode:?}):\n\
             — short log —\n{}\n— long log —\n{}",
            small_cost.report(&small),
            big_cost.report(&big)
        );
    }
}

#[test]
fn a_log_read_bounded_by_revisions_costs_the_same_on_a_longer_log() {
    let q = LogQuery { mode: Mode::Active, revisions: Some(20), ..LogQuery::default() };
    let mut small = repo_with_log(40);
    let mut big = repo_with_log(640);
    let small_cost = cost_of(&mut small, &q);
    let big_cost = cost_of(&mut big, &q);
    assert_eq!(
        small_cost.count(),
        big_cost.count(),
        "a listing bounded by revisions grew with the log:\n\
         — short log —\n{}\n— long log —\n{}",
        small_cost.report(&small),
        big_cost.report(&big)
    );
    let scans = big_cost.full_scans(&big);
    assert_eq!(scans, allowed_full_scans(), "{}", big_cost.report(&big));
}

#[test]
fn a_revision_bound_returns_whole_revisions_newest_first() {
    let conn = repo_with_log(40);
    let q = LogQuery { mode: Mode::Active, revisions: Some(5), ..LogQuery::default() };
    let body = listing(&conn, &q).unwrap();
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

/// Not the log: the point read every panel, every `mf metarecord get` and every
/// cache miss does. It must cost the same on a repository of any size — and
/// the moment it does not, it is the whole interface that slows down at once.
#[test]
fn reading_one_metarecord_costs_the_same_in_a_repository_sixteen_times_larger() {
    let mut small = repo_with_log(40);
    let mut big = repo_with_log(640);
    let uuid = |conn: &Connection| -> uuid::Uuid {
        let blob: Vec<u8> =
            conn.query_row("SELECT uuid FROM metarecord LIMIT 1", [], |r| r.get(0)).unwrap();
        uuid::Uuid::from_slice(&blob).unwrap()
    };
    let (small_uuid, big_uuid) = (uuid(&small), uuid(&big));
    let (_, small_cost) = measure(&mut small, |c| db::get_metarecord(c, small_uuid).unwrap());
    let (_, big_cost) = measure(&mut big, |c| db::get_metarecord(c, big_uuid).unwrap());
    assert_eq!(
        small_cost.count(),
        big_cost.count(),
        "reading one metarecord grew with the repository:\n{}",
        big_cost.report(&big)
    );
    assert!(
        big_cost.full_scans(&big).is_empty(),
        "reading one metarecord scanned a table:\n{}",
        big_cost.report(&big)
    );
}

#[test]
fn a_filtered_bounded_read_looks_past_the_window_for_matches() {
    // One metarecord written at the very start, then 300 revisions touching
    // others. A bounded read filtered on that metarecord must still find its
    // operations: a window is a bound on what is *returned*, not a promise to
    // stop looking (spec-event-log "limit").
    let mut conn = db::open_in_memory().unwrap();
    db::init_schema(&conn).unwrap();
    let mut w = Writer::begin(&mut conn, None).unwrap();
    let target =
        w.create_metarecord(vec![Field::new("kind", Value::String("old".into()))]).unwrap();
    w.commit().unwrap();
    for i in 0..300 {
        let mut w = Writer::begin(&mut conn, None).unwrap();
        w.create_metarecord(vec![Field::new("rank", Value::Int(i))]).unwrap();
        w.commit().unwrap();
    }

    let q = LogQuery {
        mode: Mode::Active,
        limit: Some(10),
        entity: Some(target.uuid),
        ..LogQuery::default()
    };
    let body = listing(&conn, &q).unwrap();
    let ops = body["operations"].as_array().unwrap();
    assert!(!ops.is_empty(), "the metarecord's own operations were missed by the window");
    for op in ops {
        assert_eq!(op["entity_uuid"].as_str().unwrap(), target.uuid.as_simple().to_string());
    }
}
