//! A query's progress (doc "Task status and progress"): the evaluator names
//! its phases, and a scan checking candidates one by one counts them against
//! their number — reported where the work is done (`crate::progress`), not
//! guessed beforehand.

use std::cell::RefCell;
use std::rc::Rc;

use metafolder_core::metarecord::{Field, Value};
use metafolder_core::query::{Aspect, Query};
use metafolder_daemon::forest::SortKeys;
use metafolder_daemon::index::{Eval, PageStrategy, QueryRoots, SortBy};
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::log::Writer;
use metafolder_daemon::progress::{self, REPORT_EVERY};
use uuid::Uuid;

mod common;
use common::TempDir;

type Report = (String, Option<u64>, Option<u64>);

/// `n` files, each with a distinct `name` and a `size`; their uuids.
fn repository(n: usize) -> (KvStore, Vec<Uuid>, TempDir) {
    let dir = TempDir::new("progress");
    let mut kv = KvStore::open_unsynced(dir.path()).unwrap();
    let mut w = Writer::begin(&mut kv, None).unwrap();
    let mut uuids = Vec::new();
    for i in 0..n {
        let r = w
            .create_metarecord(vec![
                Field::new("name", Value::String(format!("file{i:06}.txt"))),
                Field::new("size", Value::Int(i as i64)),
            ])
            .unwrap();
        uuids.push(r.uuid);
    }
    w.commit().unwrap();
    (kv, uuids, dir)
}

/// Runs `q` with its count, sorted by name, and what it reported.
fn reports(kv: &KvStore, q: &Query) -> Vec<Report> {
    let seen: Rc<RefCell<Vec<Report>>> = Rc::default();
    let s = seen.clone();
    let sink: progress::Sink = Box::new(move |p, d, t| s.borrow_mut().push((p.into(), d, t)));
    progress::run(sink, || {
        let src = kv.source().unwrap();
        let e = Eval { src: &src, strategy: PageStrategy::Auto };
        let keys = SortKeys::new(kv);
        let mut roots = QueryRoots::new();
        roots.keys = Some(&keys);
        let sort = [SortBy { field: "name".into(), ascending: true }];
        e.page_and_count(q, &sort, Some(50), None, &roots).unwrap();
    });
    seen.take()
}

/// The reports of `phase` carrying counts.
fn counted(reports: &[Report], phase: &str) -> Vec<(u64, u64)> {
    reports
        .iter()
        .filter(|(p, ..)| p == phase)
        .filter_map(|(_, d, t)| Some(((*d)?, (*t)?)))
        .collect::<Vec<_>>()
}

fn matches(pattern: &str) -> Query {
    Query::Matches { field: "name".into(), pattern: pattern.into(), aspect: Aspect::Raw }
}

#[test]
fn a_text_scan_counts_the_candidates_it_checks() {
    let (kv, _, _dir) = repository(3000);
    // A literal: the trigram index narrows the candidates, each then checked.
    let r = reports(&kv, &matches("file001"));
    let counts = counted(&r, "scanning name");
    let total = counts.first().expect("the scan reports its total at once").1;
    assert!(total >= 1000, "every name holding the literal is a candidate ({total})");
    assert!(counts.iter().all(|&(d, t)| t == total && d <= t), "{counts:?}");
    assert!(
        counts.iter().any(|&(d, _)| d >= REPORT_EVERY),
        "the count advances while it runs: {counts:?}"
    );
}

#[test]
fn a_scan_probing_the_running_intersection_counts_it() {
    let (kv, uuids, _dir) = repository(3000);
    // 400 candidates against 3000 holders: checked on their own rows.
    let some = Query::UuidIn { uuids: uuids[..400].to_vec() };
    for (phase, scan) in [
        ("scanning name", matches("7")),
        (
            "comparing size",
            Query::Gt { field: "size".into(), value: Value::Int(10), aspect: Aspect::Raw },
        ),
    ] {
        let r = reports(&kv, &Query::And { operands: vec![some.clone(), scan] });
        let counts = counted(&r, phase);
        assert_eq!(counts.first(), Some(&(0, 400)), "{phase}: {r:?}");
        assert!(counts.contains(&(REPORT_EVERY, 400)), "{phase}: {counts:?}");
    }
}

#[test]
fn the_phases_come_in_order_and_the_last_stays() {
    let (kv, _, _dir) = repository(300);
    let r = reports(&kv, &Query::And { operands: vec![matches("7"), matches("file")] });
    let mut phases: Vec<&str> = r.iter().map(|(p, ..)| p.as_str()).collect();
    phases.dedup();
    let first_scan = phases.iter().position(|p| *p == "scanning name").expect("a scan");
    let paging = phases.iter().position(|p| *p == "paging").expect("a paging phase");
    assert_eq!(phases[0], "filtering", "{phases:?}");
    assert!(first_scan < paging, "{phases:?}");
    assert_eq!(phases.last(), Some(&"paging"), "the phase a finished query shows: {phases:?}");
}
