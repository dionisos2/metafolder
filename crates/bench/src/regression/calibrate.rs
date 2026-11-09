//! The calibration of the query planner's scattered-read weight (doc "Query
//! planner", the cost model): `plan::SCATTERED`, what a probed key costs
//! against a walked one, measured cold on the large generated repository.
//!
//! Two series, every point cold (the store unloaded, evicted, loaded again):
//!
//! * *the walk*: `mfr_size > t` for three thresholds — the milliseconds
//!   against the keys read, whose slope is the cost of a walked key;
//! * *the probe*: `uuid_in(c) and mfr_size > -1`, which the planner answers by
//!   probing the `c` candidates (the range holds every file), less the
//!   `uuid_in(c)` alone — the milliseconds against `c`, whose slope is the
//!   cost of a candidate. Once for candidates spread over the repository,
//!   once for the files of one folder, which a forest-order reindex keeps
//!   close.
//!
//! What a candidate costs over what a walked key costs, shared among the keys
//! a candidate reads, is the weight. It is printed, not recorded: the
//! machine must be neutral for it to mean anything.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use super::{
    client, ensure_repo, evict, load_and_wait, load_ready, post, repo_root, subtree, synth, PORT,
};
use crate::{daemon_start, daemon_wait_ready, ms};
use metafolder_daemon::index::{PROBE_KEYS, SCATTERED};

/// Cold runs per point.
const RUNS: usize = 3;
/// The walk's thresholds: the generated sizes are spread evenly below a
/// million, so about 2 000, 8 000 and 32 000 files hold more.
const THRESHOLDS: [i64; 3] = [980_000, 920_000, 680_000];
/// The probe's candidate counts: all of them probed — `c × PROBE_KEYS ×
/// SCATTERED` stays below the 100 000 holders of `mfr_size` up to a weight of 13.
const CANDIDATES: [usize; 3] = [300, 900, 1_800];
/// The folder whose files are the close candidates (18 983 files on `L`).
const FOLDER: &str = "/dir0/dir1";

/// The least-squares slope of `y` on `x`; `None` with fewer than two distinct
/// `x`.
pub(crate) fn slope(points: &[(f64, f64)]) -> Option<f64> {
    let n = points.len() as f64;
    let mean_x = points.iter().map(|p| p.0).sum::<f64>() / n;
    let mean_y = points.iter().map(|p| p.1).sum::<f64>() / n;
    let sxx: f64 = points.iter().map(|p| (p.0 - mean_x).powi(2)).sum();
    let sxy: f64 = points.iter().map(|p| (p.0 - mean_x) * (p.1 - mean_y)).sum();
    (sxx > 0.0).then(|| sxy / sxx)
}

/// The weight of a scattered key against a contiguous one: what a probed
/// candidate costs (`per_candidate`, ms) over what a walked key costs
/// (`per_walked_key`, ms), shared among the `probe_keys` a candidate reads.
pub(crate) fn weight(per_candidate: f64, per_walked_key: f64, probe_keys: u64) -> f64 {
    per_candidate / per_walked_key / probe_keys as f64
}

/// One cold run of `body` against `/query/profile`: the milliseconds the
/// client waited, and the keys the daemon read.
async fn cold(url: &str, dir: &Path, store: &Path, body: &Value) -> Result<(f64, f64)> {
    let repo = load_ready(url, dir).await?;
    post(&format!("{url}/repos/{repo}/unload"), &json!({})).await?;
    evict(store)?;
    let repo = load_ready(url, dir).await?;
    let t = Instant::now();
    let v: Value = client()
        .post(format!("{url}/repos/{repo}/query/profile"))
        .json(body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let elapsed = ms(t.elapsed());
    let reads = v["entry"]["reads"].as_f64().context("a profile without reads")?;
    Ok((elapsed, reads))
}

/// The uuids `query` answers, in the order asked.
async fn uuids(url: &str, dir: &Path, body: Value) -> Result<Vec<String>> {
    let repo = load_ready(url, dir).await?;
    let v: Value = client()
        .post(format!("{url}/repos/{repo}/query"))
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    v["results"]
        .as_array()
        .context("a query without results")?
        .iter()
        .map(|u| u.as_str().map(str::to_string).context("a result that is not a uuid"))
        .collect()
}

fn counted(query: Value) -> Value {
    json!({"query": query, "limit": 1, "count": true})
}

fn range(threshold: i64) -> Value {
    json!({"type": "gt", "field": "mfr_size", "value": {"type": "int", "value": threshold}})
}

pub async fn run() -> Result<i32> {
    let root = repo_root();
    let dir: PathBuf = root.join("target/bench-data").join(synth::BIG.label);
    println!("=== metafolder-bench: the planner's scattered-read weight ===");
    ensure_repo(&dir, &synth::BIG)?;
    let store = dir
        .join(".metafolder")
        .join(metafolder_daemon::repo::INTERNAL_DIR)
        .join(metafolder_daemon::repo::KV_DIR)
        .join("data.mdb");
    let daemon = daemon_start(PORT)?;
    daemon_wait_ready(&daemon.url).await?;
    let url = daemon.url.clone();
    load_and_wait(&url, &dir).await?;

    println!("\nthe walk: mfr_size > t, cold, {RUNS} runs each");
    let mut walk = Vec::new();
    for t in THRESHOLDS {
        for _ in 0..RUNS {
            let (elapsed, keys) = cold(&url, &dir, &store, &counted(range(t))).await?;
            println!("  t = {t:>7}: {keys:>6} keys, {elapsed:>7.1} ms");
            walk.push((keys, elapsed));
        }
    }
    let per_key = slope(&walk).context("the walk read the same keys at every threshold")?;
    println!("  → {:.1} µs a walked key", per_key * 1000.0);

    let all =
        uuids(&url, &dir, json!({"query": super::present("mfr_size"), "limit": 200_000})).await?;
    let near = uuids(
        &url,
        &dir,
        json!({"query": subtree(FOLDER), "sort": [{"field": "mfr_path", "order": "asc"}], "limit": 5_000}),
    )
    .await?;
    let most = *CANDIDATES.iter().max().expect("candidate counts");
    anyhow::ensure!(near.len() >= most, "{FOLDER} holds {} files, not {most}", near.len());
    let mut code = 0;
    for (label, pool) in [("spread over the repository", &all), ("in one folder", &near)] {
        println!("\nthe probe, candidates {label}: uuid_in(c) and mfr_size > -1, less uuid_in(c)");
        let mut probe = Vec::new();
        for c in CANDIDATES {
            // Evenly spaced in the pool: the uuid order is unrelated to the
            // dense ids, so the spread candidates are scattered; the folder's,
            // in path order, are its files.
            let step = if label.starts_with("spread") { pool.len() / c } else { 1 };
            let ids: Vec<&String> = pool.iter().step_by(step.max(1)).take(c).collect();
            let set = json!({"type": "uuid_in", "uuids": ids});
            let both = json!({"type": "and", "operands": [set.clone(), range(-1)]});
            for _ in 0..RUNS {
                let (alone_ms, alone_keys) =
                    cold(&url, &dir, &store, &counted(set.clone())).await?;
                let (both_ms, both_keys) = cold(&url, &dir, &store, &counted(both.clone())).await?;
                let extra = both_keys - alone_keys;
                println!("  c = {c:>5}: {extra:>6} keys more, {:>7.1} ms more", both_ms - alone_ms);
                if extra > (c as f64) * 2.0 * PROBE_KEYS as f64 {
                    println!("    (more keys than a probe reads: the plan walked)");
                    code = 1;
                }
                probe.push((c as f64, both_ms - alone_ms));
            }
        }
        let per_candidate = slope(&probe).context("one candidate count")?;
        let w = weight(per_candidate, per_key, PROBE_KEYS);
        println!(
            "  → {:.1} µs a candidate, {:.1} walked keys: a weight of {w:.1} (SCATTERED is {SCATTERED})",
            per_candidate * 1000.0,
            per_candidate / per_key
        );
    }
    drop(daemon);
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_slope_of_a_line_is_its_slope() {
        let points = [(1.0, 12.0), (2.0, 14.0), (4.0, 18.0)];
        assert!((slope(&points).unwrap() - 2.0).abs() < 1e-9);
    }

    #[test]
    fn the_slope_fits_noisy_points() {
        let points = [(0.0, 1.0), (1.0, 2.9), (2.0, 5.1), (3.0, 7.0)];
        assert!((slope(&points).unwrap() - 2.0).abs() < 0.1);
    }

    #[test]
    fn one_abscissa_has_no_slope() {
        assert_eq!(slope(&[(3.0, 1.0), (3.0, 5.0)]), None);
        assert_eq!(slope(&[]), None);
    }

    #[test]
    fn the_weight_shares_a_candidate_among_its_keys() {
        // A candidate costs 52 walked keys' time, over 4 keys: 13 each.
        assert!((weight(0.52, 0.01, 4) - 13.0).abs() < 1e-9);
    }
}
