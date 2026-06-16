//! Benchmark suite for metafolder — runs against two persistent data folders
//! so the effect of file count / DB size is directly comparable.
//!
//! Usage:
//!   cargo build                        # build debug binaries first
//!   cargo run -p metafolder-bench               # daemon-side suite on both folders
//!   cargo run -p metafolder-bench -- gui        # + GUI scenarios (a window opens)
//!   cargo run -p metafolder-bench -- attach     # drive an already-running GUI
//!   cargo run -p metafolder-bench -- --small DIR --big DIR
//!
//!   cargo build --release              # or release for more realistic numbers
//!
//! The two data folders (default `benchmarks/bench_data` and
//! `benchmarks/bench_data_big`) are **consumed, never generated**: each must
//! exist and hold files, and must NOT already contain a `.metafolder` (the run
//! aborts otherwise). For each folder the bench spawns one isolated daemon,
//! inits a repository in place, reconciles it to populate the DB from the real
//! files, runs the benchmarks, and removes the `.metafolder` again on teardown.
//! The watcher benchmark mutates the files (renames) and undoes it afterwards,
//! so the folders are left exactly as found.

mod gui;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde_json::json;
use uuid::Uuid;

// ─── Configuration ────────────────────────────────────────────────────────────

const DAEMON_PORT: u16 = 7610;
const LOOP_N: usize = 20;
/// Number of iterations averaged for the limited-query benchmark.
const LIMITED_ITERS: usize = 10;
/// The result cap for the limited-query benchmark.
const LIMITED_N: usize = 100;
/// Cap on the number of files the watcher benchmark renames (and restores).
const WATCHER_CAP: usize = 200;
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const TIMEOUT: Duration = Duration::from_secs(120);

const DEFAULT_SMALL_DIR: &str = "benchmarks/bench_data";
const DEFAULT_BIG_DIR: &str = "benchmarks/bench_data_big";

// ─── Utilities ────────────────────────────────────────────────────────────────

pub(crate) fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

pub(crate) fn find_binary(name: &str) -> Result<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let release = root.join("target/release").join(name);
    let debug = root.join("target/debug").join(name);
    if release.exists() {
        Ok(release)
    } else if debug.exists() {
        Ok(debug)
    } else {
        anyhow::bail!("Binary '{}' not found — run `cargo build` first.", name)
    }
}

// ─── Daemon management ────────────────────────────────────────────────────────

/// Spawns a daemon on `port` with an empty configuration. Pointing `--config`
/// at a path that does not exist makes the daemon read an empty config (no repo
/// auto-load) instead of the user's, keeping the benchmark isolated from any
/// repos held under the user's daemon lock.
pub(crate) fn spawn_isolated_daemon(port: u16) -> Result<Child> {
    let bin = find_binary("metafolder-daemon")?;
    let empty_config = std::env::temp_dir()
        .join(format!("metafolder-bench-no-config-{}-{port}.json", std::process::id()));
    let _ = std::fs::remove_file(&empty_config);
    Command::new(&bin)
        .args(["--port", &port.to_string()])
        .arg("--config")
        .arg(&empty_config)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("Failed to spawn {:?}", bin))
}

/// A spawned daemon, killed when dropped.
struct Daemon {
    _proc: Child,
    url: String,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self._proc.kill();
        let _ = self._proc.wait();
    }
}

fn daemon_start(port: u16) -> Result<Daemon> {
    Ok(Daemon { _proc: spawn_isolated_daemon(port)?, url: format!("http://127.0.0.1:{port}") })
}

pub(crate) async fn daemon_wait_ready(url: &str) -> Result<()> {
    for _ in 0..50 {
        if Client::new().get(format!("{url}/health")).send().await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!("Daemon not ready after 5 s")
}

// ─── Query IR helpers ──────────────────────────────────────────────────────────
//
// The query IR is internally tagged with "type" (snake_case) — see
// `crates/core/src/query.rs`.

fn is_present(field: &str) -> serde_json::Value {
    json!({ "type": "is_present", "field": field })
}

/// `mfr_path MATCHES "<pattern>"`: the regex applies to the TreeRef name.
fn name_matches(pattern: &str) -> serde_json::Value {
    json!({ "type": "matches", "field": "mfr_path", "pattern": pattern })
}

// ─── HTTP helpers ─────────────────────────────────────────────────────────────

pub(crate) async fn api_init_repo(url: &str, root: &Path) -> Result<Uuid> {
    let v: serde_json::Value = Client::new()
        .post(format!("{url}/repos/init"))
        .json(&json!({ "root": root }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    // Response shape: {"repo_uuid": "<32-char hex>"}.
    Ok(v["repo_uuid"].as_str().context("missing repo_uuid field")?.parse()?)
}

/// Runs a query and returns the matching metarecord UUIDs (hex strings).
async fn api_query(url: &str, repo: Uuid, query: serde_json::Value) -> Result<Vec<String>> {
    Ok(Client::new()
        .post(format!("{url}/repos/{repo}/query"))
        .json(&json!({ "query": query }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

/// The root metarecord's UUID. At init it is the only metarecord carrying an
/// `mf_watch` field, which makes it cheap to locate.
async fn api_root_uuid(url: &str, repo: Uuid) -> Result<String> {
    api_query(url, repo, is_present("mf_watch"))
        .await?
        .into_iter()
        .next()
        .context("no root metarecord found")
}

/// Enables tracking on the repository by setting `mf_watch = true` on the root.
/// Tracking is opt-in: until this is set, both the watcher and reconcile treat
/// every path as ineligible and create nothing.
async fn api_enable_watch(url: &str, repo: Uuid) -> Result<()> {
    let root = api_root_uuid(url, repo).await?;
    Client::new()
        .patch(format!("{url}/repos/{repo}/metarecords/{root}"))
        .json(&json!({ "name": "mf_watch", "value": { "type": "bool", "value": true } }))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

/// Full reconcile: walks the repo root (eligibility-pruned) and creates the
/// metarecords for the files on disk. Returns (created, moved). `mime` opens
/// each file to sniff its type — disabled here to keep reconcile about indexing.
async fn api_reconcile(url: &str, repo: Uuid, mime: bool) -> Result<(usize, usize)> {
    let v: serde_json::Value = Client::new()
        .post(format!("{url}/repos/{repo}/reconcile"))
        .json(&json!({ "mime": mime }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok((
        v["created"].as_u64().unwrap_or(0) as usize,
        v["moved"].as_u64().unwrap_or(0) as usize,
    ))
}

/// Poll until `query` matches at least `expected` metarecords.
async fn wait_for_count(
    url: &str,
    repo: Uuid,
    query: serde_json::Value,
    expected: usize,
    since: Instant,
) -> Option<Duration> {
    loop {
        if api_query(url, repo, query.clone()).await.map(|v| v.len()).unwrap_or(0) >= expected {
            return Some(since.elapsed());
        }
        if since.elapsed() > TIMEOUT {
            return None;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

// ─── CLI runner ───────────────────────────────────────────────────────────────

fn cli_run(bin: &Path, url: &str, repo: Option<Uuid>, args: &[&str]) -> Result<Duration> {
    let mut cmd = Command::new(bin);
    cmd.arg("--daemon-url").arg(url);
    if let Some(r) = repo {
        cmd.arg("--repo").arg(r.to_string());
    }
    cmd.args(args).stdout(Stdio::null()).stderr(Stdio::piped());
    let t = Instant::now();
    let output = cmd.spawn()?.wait_with_output()?;
    let elapsed = t.elapsed();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("CLI command failed: {:?}\nstderr: {}", args, stderr.trim());
    }
    Ok(elapsed)
}

fn bench_loop<T>(
    label: &str,
    n: usize,
    mut f: impl FnMut() -> Result<T>,
) -> Result<(String, usize, Duration)> {
    let t = Instant::now();
    for _ in 0..n {
        f()?;
    }
    Ok((label.to_string(), n, t.elapsed()))
}

// ─── Output ───────────────────────────────────────────────────────────────────

fn print_section(title: &str, rows: &[(String, usize, Duration)]) {
    let w = rows.iter().map(|(l, ..)| l.len()).max().unwrap_or(0).max(24);
    println!("\n┌── {title}");
    for (label, n, total) in rows {
        if *n > 1 {
            let per = *total / *n as u32;
            println!(
                "│  {:<w$}  {:8.2} ms/op  ×{:<5}  (total {:6.0} ms)",
                label,
                ms(per),
                n,
                ms(*total)
            );
        } else {
            println!("│  {:<w$}  {:8.2} ms", label, ms(*total));
        }
    }
    println!("└{}", "─".repeat(w + 42));
}

// ─── Data folders → repositories ───────────────────────────────────────────────

/// A repository built in place on one of the persistent data folders.
struct DataRepo {
    label: String,
    dir: PathBuf,
    repo: Uuid,
    total: usize,
    db_bytes: u64,
}

/// Removes the `.metafolder` directories the run created, after the daemon that
/// locked them is gone. The data files themselves are never touched here.
struct MetafolderCleanup(Vec<PathBuf>);

impl Drop for MetafolderCleanup {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

/// Consume-only preflight: the folder must exist, hold at least one file, and
/// not already carry a `.metafolder`.
fn check_data_dir(dir: &Path) -> Result<()> {
    if !dir.exists() {
        bail!("data folder {dir:?} does not exist — create it and put files in it");
    }
    if !dir.is_dir() {
        bail!("{dir:?} is not a directory");
    }
    if dir.join(".metafolder").exists() {
        bail!(
            "{dir:?} already contains a .metafolder — remove it first \
             (a previous run may have aborted before cleanup)"
        );
    }
    if collect_files(dir, 1).is_empty() {
        bail!("{dir:?} contains no files — nothing to benchmark");
    }
    Ok(())
}

/// Collects up to `cap` regular file paths under `dir` (recursive), skipping
/// the `.metafolder` directory and any leftover `.benchmoved` rename.
fn collect_files(dir: &Path, cap: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for entry in entries.flatten() {
            if entry.file_name() == ".metafolder" {
                continue;
            }
            let path = entry.path();
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => stack.push(path),
                Ok(ft) if ft.is_file() => {
                    if !path.to_string_lossy().ends_with(BENCH_SUFFIX) {
                        out.push(path);
                        if out.len() >= cap {
                            return out;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// Inits a repository in place on `dir`, enables tracking, and reconciles to
/// populate the DB from the real files.
async fn build_repo(url: &str, dir: &Path, label: &str) -> Result<DataRepo> {
    use std::io::Write as _;
    print!("  [{label}] init + reconcile {} ... ", dir.display());
    std::io::stdout().flush().ok();

    let repo = api_init_repo(url, dir).await?;
    api_enable_watch(url, repo).await?;
    let t = Instant::now();
    let (created, _moved) = api_reconcile(url, repo, false).await?;
    let recon = t.elapsed();

    let total = api_query(url, repo, is_present("mfr_path")).await?.len();
    let db_bytes = std::fs::metadata(dir.join(".metafolder").join("internal").join("db.sqlite"))
        .map(|m| m.len())
        .unwrap_or(0);
    println!(
        "created {created}, {total} metarecords, db {:.1} MiB, reconcile {:.0} ms",
        db_bytes as f64 / 1_048_576.0,
        ms(recon),
    );
    Ok(DataRepo { label: label.to_string(), dir: dir.to_path_buf(), repo, total, db_bytes })
}

// ─── Bench: CLI / query latency, by DB size ────────────────────────────────────

async fn bench_repo_cli(url: &str, bin: &Path, repo: &DataRepo) -> Result<()> {
    let r = repo.repo;
    let n = LOOP_N;

    // A non-root file metarecord for the point lookups.
    let files = api_query(
        url,
        r,
        json!({ "type": "eq", "field": "mfr_type", "value": { "type": "string", "value": "file" } }),
    )
    .await?;
    let sample = files.first().cloned();

    let mut rows: Vec<(String, usize, Duration)> = Vec::new();

    // Full-scan commands (×1): cost scales with the DB size.
    let t = Instant::now();
    cli_run(bin, url, Some(r), &["list"])?;
    rows.push(("list (all metarecords)".into(), 1, t.elapsed()));

    let t = Instant::now();
    cli_run(bin, url, Some(r), &["query", "mfr_path IS PRESENT"])?;
    rows.push(("query mfr_path IS PRESENT".into(), 1, t.elapsed()));

    let t = Instant::now();
    cli_run(bin, url, Some(r), &["query", "mfr_type = \"file\""])?;
    rows.push(("query mfr_type = file".into(), 1, t.elapsed()));

    let t = Instant::now();
    cli_run(bin, url, Some(r), &["reconcile", "--no-mime"])?;
    rows.push(("reconcile (re-walk)".into(), 1, t.elapsed()));

    // Point commands (×n): roughly constant, isolate per-call latency.
    if let Some(s) = sample {
        rows.push(bench_loop("get <file>", n, || cli_run(bin, url, Some(r), &["get", &s]))?);
        rows.push(bench_loop("path <file>", n, || cli_run(bin, url, Some(r), &["path", &s]))?);
    }
    rows.push(bench_loop("create (write)", n, || {
        cli_run(bin, url, Some(r), &["create", "--field", "bench:int=1"])
    })?);

    print_section(
        &format!(
            "[{}] CLI — DB {} metarecords, {:.1} MiB",
            repo.label,
            repo.total,
            repo.db_bytes as f64 / 1_048_576.0
        ),
        &rows,
    );
    Ok(())
}

// ─── Bench: limited (first-N) retrieval, by DB size ────────────────────────────
//
// Times the queries directly over HTTP (no `mf` process overhead) so the
// numbers are comparable both across the two repos and against the same query
// run inside the GUI (the panel's `mf:daemon POST /query` measure).

/// `POST /query` with a `limit`; returns the page size actually returned.
async fn timed_query_limited(
    url: &str,
    repo: Uuid,
    query: serde_json::Value,
    limit: usize,
) -> Result<usize> {
    let v: serde_json::Value = Client::new()
        .post(format!("{url}/repos/{repo}/query"))
        .json(&json!({ "query": query, "limit": limit }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(v["results"].as_array().map(|a| a.len()).unwrap_or(0))
}

/// `GET /metarecords?limit=` (the unfiltered first page).
async fn timed_list_limited(url: &str, repo: Uuid, limit: usize) -> Result<usize> {
    let v: serde_json::Value = Client::new()
        .get(format!("{url}/repos/{repo}/metarecords?limit={limit}"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(v["results"].as_array().map(|a| a.len()).unwrap_or(0))
}

async fn bench_repo_limited(url: &str, repo: &DataRepo) -> Result<()> {
    let r = repo.repo;
    let n = LIMITED_ITERS;
    let queries: [(&str, serde_json::Value); 2] = [
        ("query mfr_path IS PRESENT", is_present("mfr_path")),
        (
            "query mfr_type = file",
            json!({ "type": "eq", "field": "mfr_type", "value": { "type": "string", "value": "file" } }),
        ),
    ];

    let mut rows: Vec<(String, usize, Duration)> = Vec::new();
    for (label, q) in queries {
        let t = Instant::now();
        for _ in 0..n {
            timed_query_limited(url, r, q.clone(), LIMITED_N).await?;
        }
        rows.push((label.to_string(), n, t.elapsed()));
    }
    let t = Instant::now();
    for _ in 0..n {
        timed_list_limited(url, r, LIMITED_N).await?;
    }
    rows.push(("list".to_string(), n, t.elapsed()));

    print_section(
        &format!(
            "[{}] limited to {LIMITED_N} (direct HTTP) — DB {} metarecords",
            repo.label, repo.total
        ),
        &rows,
    );
    Ok(())
}

// ─── Bench: watcher throughput on real files (with inverse restore) ────────────

/// Suffix appended in place to rename files for the watcher benchmark.
const BENCH_SUFFIX: &str = ".benchmoved";

/// Renames up to [`WATCHER_CAP`] real files in place (appending [`BENCH_SUFFIX`]),
/// times how long the watcher takes to re-home them, then renames them back so
/// the folder is left exactly as found. Renaming in place (vs moving into a
/// subdir) is fully reversible even for deeply-nested files.
async fn bench_repo_watcher(url: &str, repo: &DataRepo) -> Result<()> {
    let files = collect_files(&repo.dir, WATCHER_CAP);
    if files.len() < 2 {
        println!("  [{}] watcher: too few files, skipped", repo.label);
        return Ok(());
    }

    // Rename out (best-effort); remember what actually moved so we can restore.
    let t = Instant::now();
    let mut renamed: Vec<(PathBuf, PathBuf)> = Vec::with_capacity(files.len());
    for from in &files {
        let mut to = from.clone();
        let name = format!("{}{BENCH_SUFFIX}", from.file_name().unwrap().to_string_lossy());
        to.set_file_name(name);
        if std::fs::rename(from, &to).is_ok() {
            renamed.push((from.clone(), to));
        }
    }
    let k = renamed.len();

    // Detected once that many metarecords carry the suffix in their TreeRef name.
    let detected = wait_for_count(url, repo.repo, name_matches(r"\.benchmoved$"), k, t).await;

    // Restore (inverse) before reporting, so an error in reporting never leaves
    // the folder mutated.
    for (from, to) in &renamed {
        let _ = std::fs::rename(to, from);
    }

    match detected {
        Some(elapsed) => print_section(
            &format!("[{}] watcher — {k} in-place renames", repo.label),
            &[
                (format!("{k} renames detected"), 1, elapsed),
                ("  → per file".into(), 1, elapsed / k.max(1) as u32),
            ],
        ),
        None => println!("  [{}] watcher: timeout — not all {k} renames detected", repo.label),
    }
    println!("  [{}] watcher: filenames restored", repo.label);
    Ok(())
}

// ─── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    // `[mode] [--small DIR] [--big DIR] [scenario...]`. mode: data (default),
    // gui, attach. Trailing names are GUI scenarios (attach mode only).
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut mode: Option<String> = None;
    let mut small = PathBuf::from(DEFAULT_SMALL_DIR);
    let mut big = PathBuf::from(DEFAULT_BIG_DIR);
    let mut rest: Vec<String> = Vec::new();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--small" => small = it.next().context("--small needs a path")?.into(),
            "--big" => big = it.next().context("--big needs a path")?.into(),
            s if mode.is_none() && !s.starts_with('-') => mode = Some(s.to_string()),
            _ => rest.push(a),
        }
    }

    match mode.as_deref().unwrap_or("data") {
        "data" => run_data_suite(&small, &big, false).await,
        "gui" => run_data_suite(&small, &big, true).await,
        "attach" => gui::run(&rest).await,
        other => bail!("unknown mode '{other}' (expected: data | gui | attach)"),
    }
}

/// Spawns one isolated daemon, builds a repository on each data folder, runs the
/// daemon-side benchmarks on both (and the GUI scenarios when `with_gui`), then
/// tears everything down and removes the `.metafolder` directories.
async fn run_data_suite(small: &Path, big: &Path, with_gui: bool) -> Result<()> {
    println!("=== metafolder-bench ===\n");

    // Preflight both folders before touching anything.
    check_data_dir(small)?;
    check_data_dir(big)?;

    let cli_bin = find_binary("mf")?;
    println!("daemon : {}", find_binary("metafolder-daemon")?.display());
    println!("cli    : {}", cli_bin.display());
    println!("small  : {}", small.display());
    println!("big    : {}\n", big.display());

    // Declared first so it is dropped *last* — after the daemon releases the DB.
    let _cleanup = MetafolderCleanup(vec![small.join(".metafolder"), big.join(".metafolder")]);

    println!("Starting isolated daemon on {DAEMON_PORT}...");
    let daemon = daemon_start(DAEMON_PORT)?;
    daemon_wait_ready(&daemon.url).await?;
    println!("Daemon ready.\n");

    println!("Building repositories from the data folders:");
    let repos = vec![
        build_repo(&daemon.url, small, "bench_data").await?,
        build_repo(&daemon.url, big, "bench_data_big").await?,
    ];

    for repo in &repos {
        bench_repo_cli(&daemon.url, &cli_bin, repo).await?;
        bench_repo_limited(&daemon.url, repo).await?;
        bench_repo_watcher(&daemon.url, repo).await?;
    }

    if with_gui {
        let pairs: Vec<(String, String)> =
            repos.iter().map(|r| (r.label.clone(), r.repo.simple().to_string())).collect();
        gui::run_on_repos(&daemon.url, &pairs, &[]).await?;
    }

    println!("\n=== done ===");
    Ok(())
    // daemon dropped here (killed), then `_cleanup` removes the .metafolder dirs.
}
