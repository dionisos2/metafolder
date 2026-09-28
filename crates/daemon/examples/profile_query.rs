//! Profiles one query against an existing key-value store, phase by phase:
//! the time and the keys read of each step the route takes, and of each
//! operand of a top-level `and`/`or` evaluated alone.
//!
//!   cargo run --release -p metafolder-daemon --example profile_query -- \
//!       <store dir> '<query JSON>' [runs]
//!
//! The store is opened (and locked) by this process: no daemon may hold it.

use std::time::Instant;

use metafolder_core::query::Query;
use metafolder_daemon::forest_query;
use metafolder_daemon::index::{collect_path_targets, Eval, PageStrategy, QueryRoots};
use metafolder_daemon::kvstore::KvStore;
use metafolder_daemon::tree_cache::{SortKeys, TreeCache};

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

/// Runs `f` `runs` times; prints the median time and the keys read by one run.
fn phase<T>(kv: &KvStore, what: &str, runs: usize, mut f: impl FnMut() -> T) -> T {
    let before = kv.reads();
    let mut out = f();
    let reads = kv.reads() - before;
    let mut times = Vec::with_capacity(runs);
    for _ in 0..runs {
        let t = Instant::now();
        out = f();
        times.push(t.elapsed().as_secs_f64() * 1e3);
    }
    println!("{what:<40} {:>9.3} ms {reads:>9} keys", median(times));
    out
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = std::path::PathBuf::from(&args[1]);
    let q: Query = serde_json::from_str(&args[2]).expect("query JSON");
    let runs: usize = args.get(3).map(|s| s.parse().unwrap()).unwrap_or(20);
    let kv = KvStore::open(&dir).expect("open the store");

    let mut targets = Vec::new();
    collect_path_targets(&q, &mut targets);
    let resolved = phase(&kv, "resolve path targets", runs, || {
        let mut cache = TreeCache::new(false);
        targets
            .iter()
            .filter_map(|(f, p)| {
                cache.resolve_path(&kv, f, p).unwrap().map(|u| ((f.clone(), p.clone()), u))
            })
            .collect::<Vec<_>>()
    });
    let src = phase(&kv, "open the source", runs, || kv.source().unwrap());
    let e = Eval { src: &src, strategy: PageStrategy::Auto };
    let cache = TreeCache::new(false);
    let q = phase(&kv, "rewrite the forest's leaves", runs, || {
        forest_query::resolve_path_leaves(&cache, &kv, Some(&e), &q).unwrap()
    });
    let keys = SortKeys::new(&kv);
    let mut roots = QueryRoots::new();
    roots.path.extend(resolved);
    roots.keys = Some(&keys);

    if let Query::And { operands } | Query::Or { operands } = &q {
        for (i, o) in operands.iter().enumerate() {
            let n = phase(&kv, &format!("  operand {i} alone"), runs, || {
                e.count_with_roots(o, &roots).unwrap()
            });
            println!("{:<40} {n:>9} matches", "");
        }
    }
    let n = phase(&kv, "count", runs, || e.count_with_roots(&q, &roots).unwrap());
    println!("{:<40} {n:>9} matches", "");
    phase(&kv, "page of 100", runs, || {
        e.evaluate_page_with_roots(&q, &[], Some(100), None, &roots).unwrap()
    });
    phase(&kv, "page of 100 + count", runs, || {
        e.page_and_count(&q, &[], Some(100), None, &roots).unwrap()
    });
}
