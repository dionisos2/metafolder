//! Manual performance benchmark (not run by default — `#[ignore]`d).
//!
//! Confirms the two query-latency fixes on a realistically sized repository,
//! comparing the old and new code paths *in one run* (both are still reachable
//! from the library, only the callers changed):
//!
//!   #1 index build: one query per metarecord (old) vs a single table scan
//!      (new, `db::for_each_field_row`).
//!   #2 folder open: `and(follows(dir), matches(^(name1|…)$))` served by the
//!      SQL engine with a full-repo REGEXP scan (old) vs `follows(dir)` served
//!      by the bitmap index (new).
//!
//! Run against the persistent 50k-file tree:
//!   cargo test -p metafolder-daemon --test perf_bench --release -- --ignored --nocapture
//!
//! It never writes into the data folder (the `.metafolder` lives in a temp
//! dir) and only reads the files, so the tree stays consume-only.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use metafolder_core::metarecord::Value;
use metafolder_core::query::{FollowTarget, Query};
use metafolder_daemon::index::{QueryRoots, RepoIndex};
use metafolder_daemon::log::Writer;
use metafolder_daemon::state::RepoState;
use metafolder_daemon::{db, query_exec, reconcile, repo};
use uuid::Uuid;

/// `<repo>/benchmarks/bench_data_big` (50k files). `CARGO_MANIFEST_DIR` is
/// `crates/daemon`, so climb two levels.
fn bench_data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../benchmarks/bench_data_big")
}

/// Escapes the regex metacharacters in a literal filename, so an alternation of
/// real names stays a valid, literal-matching pattern (mirrors the panel's own
/// `escapeRegex`).
fn escape_regex(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if ".*+?^${}()|[]\\".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[test]
#[ignore = "manual perf benchmark; needs benchmarks/bench_data_big"]
fn bench_index_build_and_folder_query() {
    let root = bench_data_dir();
    assert!(root.is_dir(), "expected the benchmark tree at {root:?}");

    // External metafolder → nothing is written into the data folder.
    let meta = std::env::temp_dir().join(format!("mf_bench_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&meta).unwrap();
    let opened = repo::init_repository(&root, Some(&meta), Some("bench"), false).unwrap();
    let repo = Arc::new(RepoState::from_opened(opened));

    // Opt into tracking on the root, then reconcile to populate the DB.
    let root_uuid = {
        let conn = repo.conn.lock().unwrap();
        db::find_tree_child(&conn, "mfr_path", None, "").unwrap().unwrap()
    };
    {
        let mut conn = repo.conn.lock().unwrap();
        let mut w = Writer::begin(&mut conn, None).unwrap();
        w.set_field(root_uuid, "mf_watch", Value::Bool(true)).unwrap();
        w.commit().unwrap();
    }
    let t = Instant::now();
    let res = reconcile::reconcile(&repo).unwrap();
    eprintln!("\nreconcile: {} metarecords created in {:?}", res.created, t.elapsed());

    {
        let conn = repo.conn.lock().unwrap();
        repo.cache.lock().unwrap().populate(&conn).unwrap();
    }

    let conn = repo.conn.lock().unwrap();
    let n = db::list_entries(&conn).unwrap().len();
    eprintln!("repository: {n} metarecords\n");

    // ── #1: index-build field-row access pattern ────────────────────────────
    let t = Instant::now();
    let mut rows_old = 0usize;
    for uuid in db::list_entries(&conn).unwrap() {
        rows_old += db::get_field_rows(&conn, uuid).unwrap().len();
    }
    let old = t.elapsed();

    let t = Instant::now();
    let mut rows_new = 0usize;
    db::for_each_field_row(&conn, |_uuid, _row| {
        rows_new += 1;
        Ok(())
    })
    .unwrap();
    let new = t.elapsed();
    assert_eq!(rows_old, rows_new, "both paths must read the same rows");

    eprintln!("#1 read all {rows_new} field rows:");
    eprintln!("   OLD  one query per metarecord : {old:?}");
    eprintln!("   NEW  single table scan        : {new:?}");
    eprintln!("   speedup                       : {:.1}x", old.as_secs_f64() / new.as_secs_f64());

    let t = Instant::now();
    let mut forest = Vec::new();
    let index =
        RepoIndex::build_reported_collecting(&conn, &mut forest, &|_, _| {}, &|| false).unwrap();
    eprintln!("   NEW  build + collect forest    : {:?}", t.elapsed());
    // Piggyback: the tree cache built from the forest the build just collected,
    // vs the standalone DB scan (`populate`) it replaces at load.
    let t = Instant::now();
    metafolder_daemon::tree_cache::TreeCache::new(false).populate_from_forest(forest);
    eprintln!("   NEW  tree cache from that scan : {:?}  (vs a full second field scan)\n", t.elapsed());

    // ── #2: folder-open query, on the busiest directory ─────────────────────
    // The directory (tree node) with the most direct children — parent uuid
    // histogram over the tree_ref rows.
    let (parent_uuid, child_count): (Uuid, i64) = {
        let mut stmt = conn
            .prepare(
                "SELECT value_uuid, COUNT(*) c FROM field \
                 WHERE field_name = 'mfr_path' AND value_type = 'tree_ref' \
                   AND value_uuid IS NOT NULL \
                 GROUP BY value_uuid ORDER BY c DESC LIMIT 1",
            )
            .unwrap();
        stmt.query_row([], |r| {
            Ok((db::bytes_to_uuid(r.get::<_, Vec<u8>>(0).unwrap()).unwrap(), r.get::<_, i64>(1)?))
        })
        .unwrap()
    };
    let rel = repo
        .cache
        .lock()
        .unwrap()
        .path_of(&conn, "mfr_path", parent_uuid)
        .unwrap()
        .unwrap_or_default();
    eprintln!("#2 busiest directory {rel:?} has {child_count} direct children");

    // A rendered window of up to 200 of its child names (what the old panel
    // stuffed into the alternation).
    let names: Vec<String> = {
        let mut stmt = conn
            .prepare(
                "SELECT value_name FROM field \
                 WHERE field_name = 'mfr_path' AND value_uuid = ?1 LIMIT 200",
            )
            .unwrap();
        stmt.query_map([db::uuid_to_bytes(parent_uuid)], |r| r.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };

    let follows = || Query::Follows {
        field: "mfr_path".into(),
        target: FollowTarget::Path(rel.clone()),
    };
    let old_query = Query::And {
        operands: vec![
            follows(),
            Query::Matches {
                field: "mfr_path".into(),
                pattern: format!("^({})$", names.iter().map(|n| escape_regex(n)).collect::<Vec<_>>().join("|")),
            },
        ],
    };
    let new_query = follows();

    // OLD: the daemon rejected this from the index (Matches) and ran it in the
    // SQL engine, whose REGEXP UDF scans every mfr_path row.
    let mut cache = repo.cache.lock().unwrap();
    let t = Instant::now();
    let (old_hits, _) =
        query_exec::execute(&conn, &mut cache, &old_query, &[], Some(names.len()), None).unwrap();
    let old_q = t.elapsed();

    // NEW: a plain Follows the bitmap index serves, path target resolved
    // through the tree cache (as run_query_filter does).
    let mut roots = QueryRoots::new();
    if let Some(u) = cache.resolve_path(&conn, "mfr_path", &rel).unwrap() {
        roots.path.insert(("mfr_path".into(), rel.clone()), u);
    }
    let t = Instant::now();
    let (new_hits, _) =
        index.evaluate_page_with_roots(&new_query, &[], Some(200), None, &roots).unwrap();
    let new_q = t.elapsed();

    eprintln!("   OLD  and(follows, matches(^(…)$)) via SQL : {old_q:?}  ({} hits)", old_hits.len());
    eprintln!("   NEW  follows(dir) via bitmap index        : {new_q:?}  ({} hits)", new_hits.len());
    eprintln!("   speedup                                   : {:.0}x\n", old_q.as_secs_f64() / new_q.as_secs_f64());

    drop(cache);

    // ── #3: the watcher's initial directory walk (the startup "dead" phase) ──
    // Split into the filesystem read_dir cost and the per-directory eligibility
    // cost, the latter served from the DB (empty cache) vs from memory (the
    // populated cache) — to decide whether starting the watcher after the tree
    // cache is populated would meaningfully shrink the walk.
    use metafolder_daemon::tree_cache::TreeCache;
    let root_dir = repo.config.root.clone();
    let internal = repo.internal_dir();

    let mut empty = TreeCache::new(false);
    let (dirs, total_cold, elig_cold) =
        metafolder_daemon::watcher::compute_watched_dirs_timed(&conn, &mut empty, &root_dir, &internal);
    eprintln!("\n#3 watcher walk over {} eligible dirs:", dirs.len());
    eprintln!(
        "   empty cache (eligibility → DB) : total {total_cold:?}  (fs {:?} + eligibility {elig_cold:?})",
        total_cold.saturating_sub(elig_cold)
    );

    let mut warm = TreeCache::new(false);
    warm.populate(&conn).unwrap();
    let (_dirs, total_warm, elig_warm) =
        metafolder_daemon::watcher::compute_watched_dirs_timed(&conn, &mut warm, &root_dir, &internal);
    eprintln!(
        "   full cache  (eligibility → mem): total {total_warm:?}  (fs {:?} + eligibility {elig_warm:?})",
        total_warm.saturating_sub(elig_warm)
    );

    drop(conn);
    let _ = std::fs::remove_dir_all(&meta);
}
