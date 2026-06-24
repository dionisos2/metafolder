//! Scale measurement for the bitmap index (spec-indexing "What to measure
//! before committing"): build cost, resident memory, and bitmap-vs-SQL query
//! latency over a synthetic repo. It also doubles as an equivalence test at
//! scale — every timed query asserts the same result set as the SQL engine.
//!
//! Ignored by default (it builds tens of thousands of rows). Run with:
//!   cargo test -p metafolder-daemon --test index_bench --release -- --ignored --nocapture

use std::time::Instant;

use metafolder_core::metarecord::{Field, Value};
use metafolder_core::query::{FollowTarget, Query};
use metafolder_daemon::db;
use metafolder_daemon::index::RepoIndex;
use metafolder_daemon::log::Writer;
use metafolder_daemon::query_exec;
use metafolder_daemon::tree_cache::TreeCache;
use rusqlite::Connection;
use uuid::Uuid;

/// Deterministic, reproducible pseudo-random (no Math.random / clock).
fn prng(i: u64) -> u64 {
    let x = i.wrapping_mul(2_654_435_761).wrapping_add(12_345);
    x ^ (x >> 13)
}

fn s(v: &str) -> Value {
    Value::String(v.into())
}

/// Populates a fresh repo with `dirs` directory metarecords forming a tree and
/// `files` file metarecords, each carrying ~5 fields (loc/kind/rate/size/added),
/// mirroring a real reconciled repository. Returns (conn, db_id).
fn build_repo(dirs: usize, files: usize) -> (Connection, Uuid) {
    const FANOUT: usize = 8;
    let mut conn = db::open_in_memory().unwrap();
    db::init_schema(&conn).unwrap();
    let db_id = Uuid::new_v4();

    let mut w = Writer::begin(&mut conn, db_id, None).unwrap();
    let mut dir_uuids = Vec::with_capacity(dirs);
    for k in 0..dirs {
        let parent = if k == 0 { None } else { Some(dir_uuids[(k - 1) / FANOUT]) };
        let mut fields = vec![
            Field::new("loc", Value::TreeRef { parent, name: format!("dir{k}") }),
            Field::new("kind", s("dir")),
            Field::new("rate", Value::Int((prng(k as u64) % 100) as i64)),
        ];
        if k == 0 {
            fields.push(Field::new("tag", s("root")));
        }
        dir_uuids.push(w.create_metarecord(fields).unwrap().uuid);
    }
    let base = 1_700_000_000_000_i64;
    for i in 0..files {
        let parent = dir_uuids[i % dirs.max(1)];
        let r = prng(i as u64);
        w.create_metarecord(vec![
            Field::new("loc", Value::TreeRef { parent: Some(parent), name: format!("f{i}") }),
            Field::new("kind", s("file")),
            Field::new("rate", Value::Int((r % 100) as i64)),
            Field::new("size", Value::Int((r % 1_000_000) as i64)),
            Field::new("added", Value::DateTime(base + i as i64 * 1000)),
        ])
        .unwrap();
    }
    w.commit().unwrap();
    (conn, db_id)
}

fn follows_t(field: &str, cond: Query) -> Query {
    Query::FollowsTransitive { field: field.into(), target: FollowTarget::Condition(Box::new(cond)) }
}

fn battery() -> Vec<(&'static str, Query)> {
    let gte = |f: &str, n: i64| Query::Gte { field: f.into(), value: Value::Int(n) };
    let eq = |f: &str, v: Value| Query::Eq { field: f.into(), value: v };
    vec![
        ("present(rate)", Query::IsPresent { field: "rate".into() }),
        ("rate>=90 (selective)", gte("rate", 90)),
        ("rate>=10 (broad)", gte("rate", 10)),
        ("kind=file AND rate>=50", Query::And { operands: vec![eq("kind", s("file")), gte("rate", 50)] }),
        (
            "added<midpoint",
            Query::Lt { field: "added".into(), value: Value::DateTime(1_700_000_000_000 + 25_000_000) },
        ),
        ("descendants(root)", follows_t("loc", eq("tag", s("root")))),
        (
            "descendants(root) AND kind=file AND rate>=80",
            Query::And {
                operands: vec![
                    follows_t("loc", eq("tag", s("root"))),
                    eq("kind", s("file")),
                    gte("rate", 80),
                ],
            },
        ),
    ]
}

fn run_scale(dirs: usize, files: usize) {
    let n = dirs + files;
    let (conn, db_id) = build_repo(dirs, files);

    let t = Instant::now();
    let index = RepoIndex::build(&conn, db_id).unwrap();
    let build_ms = t.elapsed().as_secs_f64() * 1e3;
    let mem_mb = index.approx_serialized_bytes() as f64 / (1024.0 * 1024.0);

    println!("\n=== scale: {n} metarecords ({dirs} dirs + {files} files) ===");
    println!(
        "build: {build_ms:.0} ms   resident(bitmaps): {mem_mb:.2} MB   \
         universe: {}   fields: {}",
        index.universe_len(),
        index.field_count()
    );
    println!(
        "{:<42} {:>8} {:>11} {:>11} {:>9}",
        "query", "results", "sql (ms)", "index (ms)", "speedup"
    );

    let mut cache = TreeCache::new(false);
    for (name, q) in battery() {
        let t = Instant::now();
        let (mut sql, _) =
            query_exec::execute(&conn, &mut cache, db_id, &q, &[], None, None).unwrap();
        let sql_ms = t.elapsed().as_secs_f64() * 1e3;

        let t = Instant::now();
        let bm = index.evaluate(&q).unwrap();
        let idx_ms = t.elapsed().as_secs_f64() * 1e3;
        let mut got = index.to_uuids(&bm);

        sql.sort();
        got.sort();
        assert_eq!(got, sql, "divergence at scale on {name}");

        println!(
            "{name:<42} {:>8} {sql_ms:>11.2} {idx_ms:>11.3} {:>8.1}x",
            sql.len(),
            sql_ms / idx_ms.max(1e-6)
        );
    }
}

#[test]
#[ignore = "scale measurement; run with --release --ignored --nocapture"]
fn index_scale_measurement() {
    run_scale(700, 5_000);
    run_scale(7_000, 50_000);
}
