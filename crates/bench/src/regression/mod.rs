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
pub mod synth;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::json;
use uuid::Uuid;

use crate::{daemon_client, daemon_start, daemon_wait_ready, ms, Daemon};
use history::{Record, Verdict};

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
    // One metarecord, and one write (which pays for the index settle).
    "metarecord.get",
    "metarecord.write",
];

/// Everything a scenario needs to run.
struct Ctx {
    url: String,
    repo: Uuid,
    /// A metarecord that exists, for the point scenarios.
    sample: String,
    head: i64,
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
            get(&format!("{url}/repos/{repo}/log?mode=active&revisions=20")).await?;
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
/// `(median, min)` in milliseconds.
async fn measure(id: &str, ctx: &Ctx) -> Result<(f64, f64)> {
    run_scenario(id, ctx).await.with_context(|| format!("scenario {id}"))?;
    let mut values = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let t = Instant::now();
        run_scenario(id, ctx).await?;
        values.push(ms(t.elapsed()));
    }
    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    Ok((history::median(&values), min))
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
    for shape in &shapes {
        let dir = root.join("target/bench-data").join(shape.label);
        ensure_repo(&dir, shape)?;
        repos.push((shape.label.to_string(), dir));
    }
    if opts.real {
        for (label, dir) in
            [("real-S", "benchmarks/bench_data"), ("real-M", "benchmarks/bench_data_big")]
        {
            let path = root.join(dir);
            if path.join(".metafolder").exists() {
                repos.push((label.to_string(), path));
            } else {
                println!(
                    "skipping {label}: {} is not a repository (init + reconcile it first)",
                    path.display()
                );
            }
        }
    }

    let daemon = daemon_start(PORT)?;
    daemon_wait_ready(&daemon.url).await?;

    let mut records = Vec::new();
    for (size, dir) in &repos {
        let repo = load_repo(&daemon, dir).await?;
        let ctx = context_for(&daemon, repo).await?;
        println!("── size {size} ({})", dir.display());
        for id in SCENARIOS {
            if let Some(filter) = &opts.filter {
                if !id.starts_with(filter.as_str()) {
                    continue;
                }
            }
            let (median, min) = measure(id, &ctx).await?;
            records.push(Record {
                at: now.clone(),
                commit: commit.clone(),
                dirty,
                machine: machine.clone(),
                cpu: cpu.clone(),
                cores,
                profile: profile.to_string(),
                rustc: rustc.clone(),
                scenario: (*id).to_string(),
                size: size.clone(),
                unit: "ms".to_string(),
                median,
                min,
                runs: RUNS,
            });
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
        "\n{:<24} {:<7} {:>10} {:>10} {:>9}  verdict",
        "scenario", "size", "median", "baseline", "delta"
    );
    let mut regressions = 0;
    for record in records {
        let baseline = baselines.get(&record.key()).copied();
        let (verdict, delta) = history::compare(record.median, baseline, tolerance);
        if verdict == Verdict::Regressed {
            regressions += 1;
        }
        println!(
            "{:<24} {:<7} {:>9.2}ms {:>9} {:>9}  {}",
            record.scenario,
            record.size,
            record.median,
            baseline.map(|b| format!("{b:.2}ms")).unwrap_or_else(|| "—".into()),
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
    let mut keys: Vec<_> = history.iter().map(|r| (r.scenario.clone(), r.size.clone())).collect();
    keys.sort();
    keys.dedup();
    for (scenario, size) in keys {
        let series: Vec<&Record> =
            history.iter().filter(|r| r.scenario == scenario && r.size == size).collect();
        let values: Vec<String> = series
            .iter()
            .rev()
            .take(10)
            .rev()
            .map(|r| format!("{:.1}{}", r.median, if r.dirty { "*" } else { "" }))
            .collect();
        println!("{scenario:<24} {size:<7} {}", values.join("  "));
    }
    println!("\n(* measured on a dirty tree — never used as a baseline)");
    Ok(0)
}

// ─── Repositories ─────────────────────────────────────────────────────────────

/// Builds the generated repository if it is missing or of a different shape.
fn ensure_repo(dir: &Path, shape: &synth::Shape) -> Result<()> {
    let stamp_path = dir.join(".bench-shape");
    let stamp = format!(
        "{}:{}:{}:{}:api{}",
        shape.label,
        shape.dirs,
        shape.files,
        shape.revisions,
        metafolder_core::API_VERSION
    );
    if std::fs::read_to_string(&stamp_path).is_ok_and(|s| s.trim() == stamp) {
        println!("reusing {} ({stamp})", dir.display());
        return Ok(());
    }
    if dir.exists() {
        std::fs::remove_dir_all(dir)?;
    }
    print!("generating {} ({stamp}) ... ", dir.display());
    use std::io::Write as _;
    std::io::stdout().flush().ok();
    let t = Instant::now();
    synth::build(dir, shape)?;
    std::fs::write(&stamp_path, &stamp)?;
    println!("{:.1}s", t.elapsed().as_secs_f64());
    Ok(())
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
    let v: serde_json::Value = client()
        .post(format!("{}/repos/load", daemon.url))
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
            .post(format!("{}/repos/{repo}/query", daemon.url))
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
async fn context_for(daemon: &Daemon, repo: Uuid) -> Result<Ctx> {
    let url = daemon.url.clone();
    let page: serde_json::Value = client()
        .post(format!("{url}/repos/{repo}/query"))
        .json(&json!({"query": present("mfr_path"), "limit": 1}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let sample = page["results"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|v| v.as_str().map(str::to_string))
        .context("the repository has no metarecord to read")?;
    let log: serde_json::Value = client()
        .get(format!("{url}/repos/{repo}/log?mode=active&limit=1"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let head = log["head"].as_i64().unwrap_or(0);
    Ok(Ctx { url, repo, sample, head })
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
