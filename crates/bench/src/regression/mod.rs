//! The timed regression suite (spec-perf "The timed suite").
//!
//! A fixed list of scenarios, run against repositories the harness generates
//! itself, measured the same way every time, compared against what the same
//! machine measured before, and appended to a history that is kept.
//!
//! The cost assertions in `crates/daemon/tests/perf_cost.rs` are the other
//! half: they catch a change of *shape* without a clock and run in the ordinary
//! test suite. This half catches what only a clock can see — a constant factor
//! that doubled — and is run on purpose, before and after a change that could
//! cost something.

pub mod history;
pub mod memory;
pub mod real;
pub mod synth;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::json;
use uuid::Uuid;

use crate::{daemon_client, daemon_start, daemon_wait_ready, ms, Daemon};
use history::{Record, Verdict};
use metafolder_daemon::config::Storage;

/// Timed repetitions per scenario (plus one warm-up that is thrown away).
const RUNS: usize = 5;
/// The port the regression daemon binds — its own, so a data-suite run and a
/// regression run do not fight over one.
const PORT: u16 = 7611;

pub struct Options {
    /// Only the small shape.
    pub quick: bool,
    /// Also the large shape (minutes to generate).
    pub big: bool,
    /// Also the persistent data folders, when they exist.
    pub real: bool,
    /// Only the scenarios whose id starts with this.
    pub filter: Option<String>,
    /// The storage backends the generated repositories are built on — each
    /// its own repository and its own series in the history.
    pub storages: Vec<Storage>,
    /// Measure and compare, record nothing.
    pub no_history: bool,
    /// Print the history and exit.
    pub report: bool,
    pub tolerance: f64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            quick: false,
            big: false,
            real: false,
            filter: None,
            storages: vec![Storage::Kv],
            no_history: false,
            report: false,
            tolerance: history::DEFAULT_TOLERANCE,
        }
    }
}

// ─── Scenarios ────────────────────────────────────────────────────────────────

/// What is measured, and under which name it is remembered. The identifier is
/// the key of the whole history: renaming one starts a new series.
const SCENARIOS: &[&str] = &[
    // Reading the log back — the listing the GUI's log panel and `mf log list`
    // ask for, the window `mf log undo` reads, and the unbounded whole-log read
    // the graph needs (the one honest O(n) in the list).
    "log.window_ops",
    "log.window_revisions",
    "log.undo_window",
    "log.change_feed",
    "log.whole_tree",
    // Queries, the operation everything else is built on.
    "query.count",
    "query.page",
    "query.sorted_page",
    "query.folder",
    // Operands of different types combined — what the bitmaps are for: a
    // wide operand and a narrow one cost the narrow one, whatever the order;
    // the GUI's search box (the same terms against the path and two texts);
    // a subtree, a range and a text; a negation; a page of a combination
    // sorted on a value of ten distinct values. The last one is a known gap —
    // a range over a high-cardinality number on a KV repository reads its
    // matches one by one (spec-storage, deferred "KV bit-sliced index").
    "query.finder",
    "query.wide_and_rare",
    "query.rare_and_wide",
    "query.subtree_range_text",
    "query.negation",
    "query.combined_sorted",
    "query.size_range_in_folder",
    // One metarecord, and one write (which pays for the index settle).
    "metarecord.get",
    "metarecord.write",
    // The forest: the paths a listing resolves for the page it shows, a write
    // that moves a position (which pays for the tree-cache upkeep), and opening
    // the repository at all — the one operation that touches everything the
    // daemon keeps in memory.
    "tree.resolve_page",
    "tree.write",
    "repo.load",
];

/// Everything a scenario needs to run.
struct Ctx {
    url: String,
    repo: Uuid,
    /// A metarecord that exists, for the point scenarios.
    sample: String,
    /// A page of them, for the scenarios a listing drives.
    page: Vec<String>,
    /// Where the repository is, for the one scenario that closes it.
    dir: PathBuf,
    head: i64,
    /// The folder the combined scenarios look into, and the terms the finder
    /// searches: fixed on the generated repositories, taken from a real path
    /// on the real ones (a generated path finds nothing there).
    folder: String,
    terms: Vec<String>,
}

/// Makes each run of a scenario that writes a *different* write: setting a
/// TreeRef to the name it already holds moves no position, and would measure
/// the upkeep of nothing.
fn next_name(prefix: &str) -> String {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!("{prefix}{}", N.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

async fn run_scenario(id: &str, ctx: &Ctx) -> Result<()> {
    let client = client();
    let url = &ctx.url;
    let repo = ctx.repo;
    match id {
        "log.window_ops" => {
            get(&format!("{url}/repos/{repo}/log?mode=active&limit=50")).await?;
        }
        "log.window_revisions" => {
            // What `mf log list` asks for: twenty revisions, at most 500
            // operations (a reconcile's revision holds tens of thousands).
            get(&format!("{url}/repos/{repo}/log?mode=active&revisions=20&limit=500")).await?;
        }
        "log.undo_window" => {
            get(&format!("{url}/repos/{repo}/log?mode=linear&limit=500")).await?;
        }
        "log.change_feed" => {
            let since = (ctx.head - 20).max(0);
            get(&format!("{url}/repos/{repo}/log/since?op={since}")).await?;
        }
        "log.whole_tree" => {
            get(&format!("{url}/repos/{repo}/log?mode=tree")).await?;
        }
        "query.count" => {
            post(
                &format!("{url}/repos/{repo}/query"),
                &json!({"query": present("mfr_path"), "limit": 1, "count": true}),
            )
            .await?;
        }
        "query.page" => {
            post(
                &format!("{url}/repos/{repo}/query"),
                &json!({"query": present("mfr_path"), "limit": 100}),
            )
            .await?;
        }
        "query.sorted_page" => {
            post(
                &format!("{url}/repos/{repo}/query"),
                &json!({
                    "query": present("mfr_size"),
                    "sort": [{"field": "mfr_size", "order": "desc"}],
                    "limit": 100,
                }),
            )
            .await?;
        }
        "query.folder" => {
            post(
                &format!("{url}/repos/{repo}/query"),
                &json!({
                    "query": {
                        "type": "follows_transitive",
                        "field": "mfr_path",
                        "target": "/dir0",
                    },
                    "limit": 100,
                }),
            )
            .await?;
        }
        "query.finder" => {
            let terms = &ctx.terms;
            let osm = |field: &str, mode: &str| json!({"type": "osm", "field": field, "terms": terms, "mode": mode});
            let finder = json!({"type": "or", "operands": [
                osm("mfr_path", "path"), osm("kind", "direct"), osm("mfr_type", "direct"),
            ]});
            post(
                &format!("{url}/repos/{repo}/query"),
                &json!({"query": finder, "limit": 100, "count": true}),
            )
            .await?;
        }
        "query.wide_and_rare" | "query.rare_and_wide" => {
            let wide = eq("mfr_type", json!({"type": "string", "value": "file"}));
            let size = (synth::prng(7) % 1_000_000) as i64;
            let rare = eq("mfr_size", json!({"type": "int", "value": size}));
            let operands =
                if id == "query.wide_and_rare" { json!([wide, rare]) } else { json!([rare, wide]) };
            post(
                &format!("{url}/repos/{repo}/query"),
                &json!({"query": {"type": "and", "operands": operands}, "limit": 100, "count": true}),
            )
            .await?;
        }
        "query.subtree_range_text" => {
            let q = json!({"type": "and", "operands": [
                subtree(&ctx.folder),
                {"type": "gt", "field": "rating", "value": {"type": "int", "value": 4}},
                {"type": "matches", "field": "mfr_path", "pattern": "file1", "aspect": "value"},
            ]});
            post(
                &format!("{url}/repos/{repo}/query"),
                &json!({"query": q, "limit": 100, "count": true}),
            )
            .await?;
        }
        "query.negation" => {
            let photo = eq("kind", json!({"type": "string", "value": "photo"}));
            let q = json!({"type": "and", "operands": [
                {"type": "not", "operand": photo}, subtree(&ctx.folder),
            ]});
            post(
                &format!("{url}/repos/{repo}/query"),
                &json!({"query": q, "limit": 100, "count": true}),
            )
            .await?;
        }
        "query.combined_sorted" => {
            let photo = eq("kind", json!({"type": "string", "value": "photo"}));
            let q = json!({"type": "and", "operands": [
                present("mfr_size"),
                {"type": "gt", "field": "rating", "value": {"type": "int", "value": 2}},
                {"type": "not", "operand": photo},
            ]});
            post(
                &format!("{url}/repos/{repo}/query"),
                &json!({
                    "query": q,
                    "sort": [{"field": "rating", "order": "desc"}],
                    "limit": 100,
                    "count": true,
                }),
            )
            .await?;
        }
        "query.size_range_in_folder" => {
            let q = json!({"type": "and", "operands": [
                subtree(&ctx.folder),
                {"type": "gt", "field": "mfr_size", "value": {"type": "int", "value": 900_000}},
            ]});
            post(
                &format!("{url}/repos/{repo}/query"),
                &json!({"query": q, "limit": 100, "count": true}),
            )
            .await?;
        }
        "metarecord.get" => {
            get(&format!("{url}/repos/{repo}/metarecords/{}", ctx.sample)).await?;
        }
        "metarecord.write" => {
            client
                .post(format!("{url}/repos/{repo}/query/fields/set"))
                .json(&json!({
                    "query": {"type": "uuid_in", "uuids": [ctx.sample]},
                    "name": "bench_touch",
                    "value": {"type": "int", "value": 1},
                }))
                .send()
                .await?
                .error_for_status()?;
        }
        "tree.resolve_page" => {
            post(
                &format!("{url}/repos/{repo}/query/fields/resolve-tree"),
                &json!({
                    "query": {"type": "uuid_in", "uuids": ctx.page},
                    "field": "mfr_path",
                }),
            )
            .await?;
        }
        // A position moved in a forest of its own, so what is measured is the
        // *upkeep*, not the size of the tree it happens in. It is the scenario
        // that says whether a write is paying for a rebuild: settling one cell
        // costs the cell, rebuilding costs the repository, and on M and L those
        // are not the same number.
        "tree.write" => {
            post(
                &format!("{url}/repos/{repo}/query/fields/set"),
                &json!({
                    "query": {"type": "uuid_in", "uuids": [ctx.sample]},
                    "name": "bench_tree",
                    "value": {
                        "type": "tree_ref",
                        "value": {"parent": null, "name": next_name("bench-")},
                    },
                }),
            )
            .await?;
        }
        // Closing the repository and opening it again: the tree cache built,
        // the index built, the schema read, the migrations checked. The pages
        // stay in the operating system's cache, so this measures the work and
        // not the disk — which is the repeatable half, and the half a change
        // can regress.
        "repo.load" => {
            post(&format!("{url}/repos/{repo}/unload"), &json!({})).await?;
            load_and_wait(url, &ctx.dir).await?;
        }
        other => anyhow::bail!("unknown scenario '{other}'"),
    }
    Ok(())
}

/// One HTTP client for the whole run.
///
/// `daemon_client()` builds a fresh client — and therefore a fresh connection
/// pool — on every call, which put tens of milliseconds of connection setup
/// into every measurement and buried the differences the suite exists to see.
fn client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(daemon_client)
}

fn present(field: &str) -> serde_json::Value {
    json!({ "type": "is_present", "field": field })
}

fn eq(field: &str, value: serde_json::Value) -> serde_json::Value {
    json!({ "type": "eq", "field": field, "value": value })
}

/// Everything below a folder of the generated tree.
fn subtree(path: &str) -> serde_json::Value {
    json!({ "type": "follows_transitive", "field": "mfr_path", "target": path })
}

async fn get(url: &str) -> Result<()> {
    client().get(url).send().await?.error_for_status()?.bytes().await?;
    Ok(())
}

async fn post(url: &str, body: &serde_json::Value) -> Result<()> {
    client().post(url).json(body).send().await?.error_for_status()?.bytes().await?;
    Ok(())
}

// ─── Measurement ──────────────────────────────────────────────────────────────

/// Runs one scenario [`RUNS`] times (after one warm-up) and returns
/// `(median, min)` in milliseconds, and what the daemon (process `pid`)
/// allocated for it at its peak, in MiB — over every run, the warm-up
/// included (an allocation the allocator keeps is only visible the first
/// time). `None` for the memory where it cannot be read.
async fn measure(id: &str, ctx: &Ctx, pid: u32) -> Result<(f64, f64, Option<f64>)> {
    let sampler = memory::Sampler::start(pid);
    run_scenario(id, ctx).await.with_context(|| format!("scenario {id}"))?;
    let mut values = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let t = Instant::now();
        run_scenario(id, ctx).await?;
        values.push(ms(t.elapsed()));
    }
    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    let mem = sampler.map(|s| s.finish(pid));
    Ok((history::median(&values), min, mem))
}

// ─── The run ──────────────────────────────────────────────────────────────────

pub async fn run(opts: &Options) -> Result<i32> {
    let root = repo_root();
    let machine = machine_name();
    let history_path = history::path_for(&root, &machine);

    if opts.report {
        return report(&history_path);
    }

    let (commit, dirty) = git_state(&root);
    let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
    let now = timestamp();
    let cpu = cpu_model();
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0);
    let rustc = rustc_version();

    println!("=== metafolder-bench: regression suite ===");
    println!("machine : {machine} ({cpu}, {cores} cores)");
    println!(
        "build   : {profile}, rustc {rustc}, commit {commit}{}",
        if dirty { " (dirty)" } else { "" }
    );
    if profile == "debug" {
        println!("note    : a debug build measures the debug build — its history is its own.");
    }
    println!("history : {}", history_path.display());
    println!();

    let shapes: Vec<&synth::Shape> = if opts.quick {
        synth::SHAPES.iter().take(1).collect()
    } else if opts.big {
        synth::SHAPES.iter().chain(std::iter::once(&synth::BIG)).collect()
    } else {
        synth::SHAPES.iter().collect()
    };

    // The generated repositories live in target/, are reused between runs, and
    // are rebuilt when their shape (or the wire version, which moves with the
    // schema) changes.
    let mut repos: Vec<(String, PathBuf)> = Vec::new();
    let mut generated = false;
    for &storage in &opts.storages {
        for shape in &shapes {
            // The key-value repositories keep the plain name they always had.
            let name = match storage {
                Storage::Kv => shape.label.to_string(),
                Storage::Sqlite => format!("{}-sqlite", shape.label),
            };
            let dir = root.join("target/bench-data").join(name);
            generated |= ensure_repo(&dir, shape, storage)?;
            repos.push((shape.label.to_string(), dir));
        }
    }
    if generated {
        println!(
            "\nnote: a repository was generated just now, so its pages are still cold —\n\
             \x20     run the suite again for a number worth keeping as a baseline."
        );
    }
    let daemon = daemon_start(PORT)?;
    daemon_wait_ready(&daemon.url).await?;

    // The real folders, measured through copies of their own (see `real`).
    if opts.real {
        for (label, folder) in
            [("real-S", "benchmarks/bench_data"), ("real-M", "benchmarks/bench_data_big")]
        {
            let src = root.join(folder);
            if !src.is_dir() {
                println!("skipping {label}: {} does not exist", src.display());
                continue;
            }
            for &storage in &opts.storages {
                let name = match storage {
                    Storage::Kv => label.to_string(),
                    Storage::Sqlite => format!("{label}-sqlite"),
                };
                let dest = root.join("target/bench-data").join(&name);
                if real::ensure(&daemon.url, &src, &dest, &name, storage).await? {
                    println!(
                        "note: {name} was built just now, so its pages are still cold —\n\
                         \x20     run the suite again for a number worth keeping as a baseline."
                    );
                }
                repos.push((label.to_string(), dest));
            }
        }
    }

    let mut records = Vec::new();
    for (size, dir) in &repos {
        let repo = load_repo(&daemon, dir).await?;
        let ctx = context_for(&daemon, repo, dir, size.starts_with("real")).await?;
        let storage = storage_of(dir);
        println!("── size {size}, {storage} ({})", dir.display());
        for id in SCENARIOS {
            if let Some(filter) = &opts.filter {
                if !id.starts_with(filter.as_str()) {
                    continue;
                }
            }
            let (median, min, mem) = measure(id, &ctx, daemon.pid()).await?;
            let record = |scenario: String, unit: &str, median: f64, min: f64| Record {
                at: now.clone(),
                commit: commit.clone(),
                dirty,
                machine: machine.clone(),
                cpu: cpu.clone(),
                cores,
                profile: profile.to_string(),
                rustc: rustc.clone(),
                scenario,
                size: size.clone(),
                storage: storage.clone(),
                unit: unit.to_string(),
                median,
                min,
                runs: RUNS,
            };
            records.push(record((*id).to_string(), "ms", median, min));
            // Memory has a series of its own, under its own name.
            if let Some(mib) = mem {
                records.push(record(format!("mem.{id}"), "MiB", mib, mib));
            }
        }
    }
    drop(daemon);

    let baselines = history::baselines(&history::read(&history_path)?);
    let regressions = print_table(&records, &baselines, opts.tolerance);

    if opts.no_history {
        println!("\n(--no-history: nothing recorded)");
    } else {
        history::append(&history_path, &records)?;
        println!("\n{} measurement(s) appended to {}", records.len(), history_path.display());
    }

    if regressions > 0 {
        println!(
            "\n{regressions} scenario(s) regressed by more than {:.0}%.",
            opts.tolerance * 100.0
        );
        return Ok(1);
    }
    Ok(0)
}

/// Prints the run as a table, and returns how many scenarios regressed.
fn print_table(
    records: &[Record],
    baselines: &std::collections::HashMap<history::Key, f64>,
    tolerance: f64,
) -> usize {
    println!(
        "\n{:<30} {:<9} {:>11} {:>11} {:>9}  verdict",
        "scenario", "size", "median", "baseline", "delta"
    );
    let mut regressions = 0;
    for record in records {
        let baseline = baselines.get(&record.key()).copied();
        let (verdict, delta) = history::compare(record.median, baseline, tolerance, &record.unit);
        if verdict == Verdict::Regressed {
            regressions += 1;
        }
        let unit = &record.unit;
        println!(
            "{:<30} {:<9} {:>11} {:>11} {:>9}  {}",
            record.scenario,
            format!("{}/{}", record.size, record.storage),
            format!("{:.2}{unit}", record.median),
            baseline.map(|b| format!("{b:.2}{unit}")).unwrap_or_else(|| "—".into()),
            delta.map(|d| format!("{d:+.1}%")).unwrap_or_else(|| "—".into()),
            verdict.mark(),
        );
    }
    regressions
}

/// `--report`: what the history holds, newest last.
fn report(path: &Path) -> Result<i32> {
    let history = history::read(path)?;
    if history.is_empty() {
        println!("no history yet at {}", path.display());
        return Ok(0);
    }
    println!("{} measurement(s) in {}\n", history.len(), path.display());
    let mut keys: Vec<_> =
        history.iter().map(|r| (r.scenario.clone(), r.size.clone(), r.storage.clone())).collect();
    keys.sort();
    keys.dedup();
    for (scenario, size, storage) in keys {
        let series: Vec<&Record> = history
            .iter()
            .filter(|r| r.scenario == scenario && r.size == size && r.storage == storage)
            .collect();
        let values: Vec<String> = series
            .iter()
            .rev()
            .take(10)
            .rev()
            .map(|r| format!("{:.1}{}", r.median, if r.dirty { "*" } else { "" }))
            .collect();
        let size = format!("{size}/{storage}");
        println!("{scenario:<30} {size:<9} {}", values.join("  "));
    }
    println!("\n(* measured on a dirty tree — never used as a baseline)");
    Ok(0)
}

// ─── Repositories ─────────────────────────────────────────────────────────────

/// The storage backend of the repository at `dir`, as its `config.json`
/// names it (a repository from before the choice existed is SQLite).
fn storage_of(dir: &Path) -> String {
    std::fs::read_to_string(dir.join(".metafolder/config.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v["storage"].as_str().map(str::to_string))
        .unwrap_or_else(|| "sqlite".into())
}

/// Builds the generated repository if it is missing or of a different shape.
/// Returns whether it had to build it.
fn ensure_repo(dir: &Path, shape: &synth::Shape, storage: Storage) -> Result<bool> {
    let stamp_path = dir.join(".bench-shape");
    let stamp = format!(
        "{}:{}:{}:{}:api{}:{storage:?}",
        shape.label,
        shape.dirs,
        shape.files,
        shape.revisions,
        metafolder_core::API_VERSION
    );
    if std::fs::read_to_string(&stamp_path).is_ok_and(|s| s.trim() == stamp) {
        println!("reusing {} ({stamp})", dir.display());
        return Ok(false);
    }
    if dir.exists() {
        std::fs::remove_dir_all(dir)?;
    }
    print!("generating {} ({stamp}) ... ", dir.display());
    use std::io::Write as _;
    std::io::stdout().flush().ok();
    let t = Instant::now();
    synth::build(dir, shape, storage)?;
    std::fs::write(&stamp_path, &stamp)?;
    println!("{:.1}s", t.elapsed().as_secs_f64());
    Ok(true)
}

/// Loads a repository and waits until it can serve data.
///
/// A load returns immediately and warms the repository (the bitmap index, the
/// tree cache) in the background as an observable task; every data endpoint
/// answers `503` until that finishes (spec-main "POST /repos/load"). Measuring
/// through that window would measure the warm-up, so the suite waits it out —
/// by asking for what it is about to measure, which needs no assumption about
/// the task listing's shape.
async fn load_repo(daemon: &Daemon, dir: &Path) -> Result<Uuid> {
    load_and_wait(&daemon.url, dir).await
}

/// Loads a repository and waits until it answers a query — the load returns as
/// soon as the uuid is known and warms the repository in the background
/// (spec-main "POST /repos/load"), so "loaded" means "serving", not "accepted".
async fn load_and_wait(url: &str, dir: &Path) -> Result<Uuid> {
    let v: serde_json::Value = client()
        .post(format!("{url}/repos/load"))
        .json(&json!({ "root": dir }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let repo: Uuid = v["repo_uuid"].as_str().context("missing repo_uuid")?.parse()?;
    let deadline = Instant::now() + Duration::from_secs(900);
    loop {
        let response = client()
            .post(format!("{url}/repos/{repo}/query"))
            .json(&json!({"query": present("mfr_path"), "limit": 1}))
            .send()
            .await?;
        if response.status() != reqwest::StatusCode::SERVICE_UNAVAILABLE {
            response.error_for_status()?;
            return Ok(repo);
        }
        if Instant::now() > deadline {
            anyhow::bail!("{} is still warming up after 15 minutes", dir.display());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The per-repository facts the scenarios need: one existing metarecord, and
/// the log's HEAD.
async fn context_for(daemon: &Daemon, repo: Uuid, dir: &Path, real: bool) -> Result<Ctx> {
    let url = daemon.url.clone();
    let body: serde_json::Value = client()
        .post(format!("{url}/repos/{repo}/query"))
        .json(&json!({"query": present("mfr_path"), "limit": 100}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let page: Vec<String> = body["results"]
        .as_array()
        .context("the repository has no metarecord to read")?
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    let sample = page.first().cloned().context("the repository has no metarecord to read")?;
    let log: serde_json::Value = client()
        .get(format!("{url}/repos/{repo}/log?mode=active&limit=1"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let head = log["head"].as_i64().unwrap_or(0);
    let (mut folder, mut terms) = ("/dir1".to_string(), vec!["dir1".into(), "file12".into()]);
    if real {
        (folder, terms) = real_search(&url, repo).await?;
        println!("   real search: folder {folder}, terms {terms:?}");
    }
    Ok(Ctx { url, repo, sample, page, dir: dir.to_path_buf(), head, folder, terms })
}

/// A folder and finder terms from a real repository: its deepest path among
/// a page of files — the top folder it is in, and the first letters of its
/// parent's name and its own, what someone looking for it would type.
async fn real_search(url: &str, repo: Uuid) -> Result<(String, Vec<String>)> {
    let files = eq("mfr_type", json!({"type": "string", "value": "file"}));
    let body: serde_json::Value = client()
        .post(format!("{url}/repos/{repo}/query"))
        .json(&json!({"query": files, "limit": 500}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let uuids = body["results"].clone();
    let paths: serde_json::Value = client()
        .post(format!("{url}/repos/{repo}/query/fields/resolve-tree"))
        .json(&json!({"query": {"type": "uuid_in", "uuids": uuids}, "field": "mfr_path"}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let deepest = paths
        .as_object()
        .into_iter()
        .flat_map(|m| m.values())
        .filter_map(|v| v.as_array()?.first()?.as_str().map(str::to_string))
        .max_by_key(|p| (p.matches('/').count(), p.clone()))
        .context("no file path in the repository")?;
    let parts: Vec<&str> = deepest.split('/').filter(|c| !c.is_empty()).collect();
    let prefix = |c: &str| c.chars().take(4).collect::<String>().to_lowercase();
    let terms = match parts.as_slice() {
        [.., parent, name] => vec![prefix(parent), prefix(name)],
        [name] => vec![prefix(name)],
        [] => anyhow::bail!("an empty path"),
    };
    Ok((format!("/{}", parts[0]), terms))
}

// ─── The machine, the build ───────────────────────────────────────────────────

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap().to_path_buf()
}

fn machine_name() -> String {
    std::env::var("METAFOLDER_BENCH_MACHINE")
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn cpu_model() -> String {
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|text| {
            text.lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split(':').nth(1))
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_else(|| "unknown".to_string())
}

fn rustc_version() -> String {
    std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.split_whitespace().nth(1).unwrap_or("unknown").to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// `(short commit, dirty)`.
fn git_state(root: &Path) -> (String, bool) {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
    };
    let commit = git(&["rev-parse", "--short", "HEAD"])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    let dirty = git(&["status", "--porcelain"]).is_some_and(|s| !s.trim().is_empty());
    (commit, dirty)
}

/// ISO-8601 UTC to the second, through the project's own date helper.
fn timestamp() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    metafolder_core::date::iso8601_from_ms(ms)
}
