//! Benchmark suite for metafolder — CLI, watcher and GUI performance.
//!
//! Usage:
//!   cargo build                        # build debug binaries first
//!   cargo run -p metafolder-bench              # daemon suite (CLI + watcher)
//!   cargo run -p metafolder-bench -- gui       # GUI suite (needs a running GUI)
//!   cargo run -p metafolder-bench -- gui-launch 11000   # spawn daemon+GUI+repo
//!   cargo run -p metafolder-bench -- all       # daemon + attach-GUI
//!
//!   cargo build --release              # or release for more realistic numbers
//!   cargo run -p metafolder-bench --release
//!
//! The daemon suite spawns its own daemon on [`PORT`] with an empty
//! configuration (no repository auto-load), so it never touches the user's
//! running daemon or the repositories it holds under an exclusive lock.
//!
//! The GUI suite instead drives an already-running GUI through its `/gui/*`
//! scripting API (a Rust port of `scripts/bench-gui.sh`) and reads back the
//! panel phase timings, alongside a raw-HTTP baseline against the same repo.

mod gui;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::json;
use tokio::sync::Semaphore;
use uuid::Uuid;

// ─── Configuration ────────────────────────────────────────────────────────────

const PORT: u16 = 7600;
const LOOP_N: usize = 50;
const BULK_SIZES: &[usize] = &[1_000, 10_000];
const WATCHER_SIZES: &[usize] = &[100, 1_000];
const HTTP_CONCURRENCY: usize = 32;
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const TIMEOUT: Duration = Duration::from_secs(30);

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

// ─── Query IR helpers ──────────────────────────────────────────────────────────
//
// The query IR is internally tagged with "type" (snake_case) — see
// `crates/core/src/query.rs`. These build the JSON bodies the daemon expects.

/// `field IS PRESENT`.
fn is_present(field: &str) -> serde_json::Value {
    json!({ "type": "is_present", "field": field })
}

/// `mfr_path -> "<parent>"`: metarecords whose direct parent is the metarecord
/// at the given repo-root-relative path (TreeRef `Follows` semantics).
fn children_of(parent_path: &str) -> serde_json::Value {
    json!({ "type": "follows", "field": "mfr_path", "target": parent_path })
}

// ─── Daemon management ────────────────────────────────────────────────────────

struct Daemon {
    _proc: Child,
    pub url: String,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self._proc.kill();
        let _ = self._proc.wait();
    }
}

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

fn daemon_start() -> Result<Daemon> {
    let child = spawn_isolated_daemon(PORT)?;
    Ok(Daemon {
        _proc: child,
        url: format!("http://127.0.0.1:{PORT}"),
    })
}

pub(crate) async fn daemon_wait_ready(url: &str) -> Result<()> {
    for _ in 0..50 {
        if Client::new()
            .get(format!("{url}/health"))
            .send()
            .await
            .is_ok()
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!("Daemon not ready after 5 s")
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
    Ok(v["repo_uuid"]
        .as_str()
        .context("missing repo_uuid field")?
        .parse()?)
}

async fn api_create_metarecord(url: &str, repo: Uuid, rating: i64) -> Result<Uuid> {
    // Response is the full MetaRecord object; we only need its uuid.
    let v: serde_json::Value = Client::new()
        .post(format!("{url}/repos/{repo}/metarecords"))
        .json(&json!({
            "fields": [{ "name": "rating", "value": { "type": "int", "value": rating } }]
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(v["uuid"].as_str().context("missing uuid field")?.parse()?)
}

/// Create `n` metarecords concurrently via HTTP. Returns their UUIDs.
pub(crate) async fn api_create_n(url: &str, repo: Uuid, n: usize) -> Result<Vec<Uuid>> {
    let sem = Arc::new(Semaphore::new(HTTP_CONCURRENCY));
    let mut handles = Vec::with_capacity(n);
    for i in 0..n {
        let (url, sem) = (url.to_string(), sem.clone());
        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            api_create_metarecord(&url, repo, i as i64).await
        }));
    }
    let mut uuids = Vec::with_capacity(n);
    for h in handles {
        uuids.push(h.await??);
    }
    Ok(uuids)
}

/// Runs a query and returns the matching metarecord UUIDs (as hex strings).
/// Without a `limit` the daemon answers with a bare array.
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

/// Number of metarecords whose direct parent is the metarecord at `parent_path`.
async fn api_children_count(url: &str, repo: Uuid, parent_path: &str) -> Result<usize> {
    Ok(api_query(url, repo, children_of(parent_path)).await?.len())
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
/// Tracking is opt-in: until this is set, the watcher drops every event as
/// ineligible. Eligibility is read fresh from the DB per event, so this takes
/// effect immediately for subsequently-created files (no repo reload needed).
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

/// Poll until at least `expected` metarecords are direct children of
/// `parent_path`. Returns elapsed time since `since`, or None on timeout.
async fn wait_for_children(
    url: &str,
    repo: Uuid,
    parent_path: &str,
    expected: usize,
    since: Instant,
) -> Option<Duration> {
    loop {
        if api_children_count(url, repo, parent_path).await.unwrap_or(0) >= expected {
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
        anyhow::bail!(
            "CLI command failed: {:?}\nstderr: {}",
            args,
            stderr.trim()
        );
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

// ─── Bench 1: CLI latency (single-metarecord loop) ────────────────────────────

async fn bench_cli_loop(url: &str, bin: &Path, repo: Uuid) -> Result<()> {
    let n = LOOP_N;

    // Pre-create one persistent metarecord (for get/set) and n metarecords (for delete)
    let entry = api_create_metarecord(url, repo, 99).await?;
    let to_delete = api_create_n(url, repo, n).await?;
    let entry_s = entry.to_string();

    let mut rows: Vec<(String, usize, Duration)> = Vec::new();

    rows.push(bench_loop("repos", n, || {
        cli_run(bin, url, None, &["repos"])
    })?);

    rows.push(bench_loop("list", n, || {
        cli_run(bin, url, Some(repo), &["list"])
    })?);

    rows.push(bench_loop("get", n, || {
        cli_run(bin, url, Some(repo), &["get", &entry_s])
    })?);

    rows.push(bench_loop("create", n, || {
        cli_run(bin, url, Some(repo), &["create", "--field", "rating:int=5"])
    })?);

    rows.push(bench_loop("set", n, || {
        cli_run(bin, url, Some(repo), &["set", &entry_s, "rating:int=9"])
    })?);

    // Delete: consume the pre-created UUIDs one by one
    let t = Instant::now();
    for u in &to_delete {
        cli_run(bin, url, Some(repo), &["delete", &u.to_string()])?;
    }
    rows.push(("delete".to_string(), n, t.elapsed()));

    rows.push(bench_loop("query IS PRESENT", n, || {
        cli_run(bin, url, Some(repo), &["query", "rating IS PRESENT"])
    })?);

    rows.push(bench_loop("query > 50", n, || {
        cli_run(bin, url, Some(repo), &["query", "rating > 50"])
    })?);

    rows.push(bench_loop("reconcile", n, || {
        cli_run(bin, url, Some(repo), &["reconcile"])
    })?);

    print_section(
        &format!("CLI — latency per command  (×{n}, DB ~{n} metarecords)"),
        &rows,
    );
    Ok(())
}

// ─── Bench 2: CLI bulk (large number of metarecords) ──────────────────────────

async fn bench_cli_bulk(url: &str, bin: &Path, repo: Uuid) -> Result<()> {
    let mut rows: Vec<(String, usize, Duration)> = Vec::new();
    let mut total = 0usize;

    for &n in BULK_SIZES {
        print!("  pre-populating +{n} metarecords... ");
        let t = Instant::now();
        api_create_n(url, repo, n).await?;
        total += n;
        println!("done in {:.0} ms  (DB = {} metarecords)", ms(t.elapsed()), total);

        let db = total;

        let t = Instant::now();
        cli_run(bin, url, Some(repo), &["list"])?;
        rows.push((format!("list           (DB={db})"), 1, t.elapsed()));

        let t = Instant::now();
        cli_run(bin, url, Some(repo), &["query", "rating IS PRESENT"])?;
        rows.push((format!("query IS PRES. (DB={db})"), 1, t.elapsed()));

        let t = Instant::now();
        cli_run(bin, url, Some(repo), &["query", "rating > 500"])?;
        rows.push((format!("query >500     (DB={db})"), 1, t.elapsed()));

        let t = Instant::now();
        cli_run(bin, url, Some(repo), &["reconcile"])?;
        rows.push((format!("reconcile      (DB={db})"), 1, t.elapsed()));
    }

    print_section("CLI — bulk (commands on large number of metarecords)", &rows);
    Ok(())
}

// ─── Bench 3: Watcher — individual file moves ─────────────────────────────────

async fn bench_watcher_moves(url: &str, n: usize) -> Result<()> {
    let tmp = tempfile::TempDir::new()?;
    let root = tmp.path();
    let src = root.join("src");
    let dst = root.join("dst");

    // Create subdirectories BEFORE init so the recursive notify watch already
    // covers them when files appear: creating a dir then immediately writing
    // files inside it can race the inotify registration of the new dir.
    std::fs::create_dir_all(&src)?;
    std::fs::create_dir_all(&dst)?;

    let repo = api_init_repo(url, root).await?;
    println!("  repo: {repo}");

    // Tracking is opt-in — enable it before creating the files, otherwise the
    // watcher drops their create events as ineligible.
    api_enable_watch(url, repo).await?;

    // Create N files; they become eligible because mf_watch is now set on root.
    for i in 0..n {
        std::fs::write(src.join(format!("file_{i:05}.txt")), b"")?;
    }

    // Wait for the watcher to register all files under /src before timing moves.
    print!("  waiting for watcher to register {n} files... ");
    if wait_for_children(url, repo, "/src", n, Instant::now())
        .await
        .is_none()
    {
        anyhow::bail!("Timeout waiting for {n} files to be registered under /src");
    }
    println!("ok");

    // ── Timer start ──
    let t = Instant::now();

    for i in 0..n {
        let old = src.join(format!("file_{i:05}.txt"));
        let new = dst.join(format!("file_{i:05}.txt"));
        std::fs::rename(&old, &new)?;
    }

    // All moves are processed once every file is a child of /dst.
    let elapsed = match wait_for_children(url, repo, "/dst", n, t).await {
        Some(d) => d,
        None => {
            println!("  ⚠  Timeout ({TIMEOUT:?}) — not all {n} moves detected");
            TIMEOUT
        }
    };
    // ── Timer end ──

    let per_file = elapsed / n as u32;
    let rows = vec![
        (format!("{n} files moved individually"), 1, elapsed),
        ("  → per file".to_string(), 1, per_file),
    ];
    print_section(
        &format!("Watcher — individual moves ({n} files)"),
        &rows,
    );

    // Verify: /src should be empty and /dst should hold all N.
    let in_src = api_children_count(url, repo, "/src").await?;
    let in_dst = api_children_count(url, repo, "/dst").await?;
    println!("  Move check: /src has {in_src} files, /dst has {in_dst} (expected 0 / {n})");

    Ok(())
}

// ─── Bench 4: Watcher — folder rename ─────────────────────────────────────────

async fn bench_watcher_folder(url: &str, n: usize) -> Result<()> {
    let tmp = tempfile::TempDir::new()?;
    let root = tmp.path();
    let subdir = root.join("subdir");
    let subdir_moved = root.join("subdir_moved");

    // Create subdirectory BEFORE init so the recursive watch already covers it.
    std::fs::create_dir_all(&subdir)?;

    let repo = api_init_repo(url, root).await?;
    println!("  repo: {repo}");

    // Opt in to tracking before creating the files (see bench_watcher_moves).
    api_enable_watch(url, repo).await?;

    for i in 0..n {
        std::fs::write(subdir.join(format!("file_{i:05}.txt")), b"")?;
    }

    // Wait for the watcher to register all N files under /subdir.
    print!("  waiting for watcher to register {n} files... ");
    if wait_for_children(url, repo, "/subdir", n, Instant::now())
        .await
        .is_none()
    {
        anyhow::bail!("Timeout waiting for {n} files to be registered under /subdir");
    }
    println!("ok");

    // ── Timer start ──
    let t = Instant::now();

    // Rename the whole folder. The watcher should re-home every descendant.
    std::fs::rename(&subdir, &subdir_moved)?;

    // The rename is fully processed once all N files are children of the moved
    // folder.
    let elapsed = match wait_for_children(url, repo, "/subdir_moved", n, t).await {
        Some(d) => d,
        None => {
            println!("  ⚠  Timeout ({TIMEOUT:?})");
            TIMEOUT
        }
    };
    // ── Timer end ──

    // Check whether the folder rename was actually handled by the watcher.
    let old_count = api_children_count(url, repo, "/subdir").await?;
    let new_count = api_children_count(url, repo, "/subdir_moved").await?;

    let update_status = if new_count == n && old_count == 0 {
        "✓ all paths updated".to_string()
    } else if old_count == n && new_count == 0 {
        "✗ old paths retained (folder rename not handled)".to_string()
    } else {
        format!("? partial state ({old_count} under /subdir, {new_count} under /subdir_moved)")
    };

    let rows = vec![(format!("{n} files inside a renamed folder"), 1, elapsed)];
    print_section(
        &format!("Watcher — folder rename ({n} files)"),
        &rows,
    );
    println!("  Path state after rename: {update_status}");

    Ok(())
}

// ─── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    // First positional arg selects the suite: "daemon" (default), "gui", "all".
    // Any further args are GUI scenario names (see `gui::run`).
    let args: Vec<String> = std::env::args().skip(1).collect();
    let suite = args.first().map(String::as_str).unwrap_or("daemon");
    match suite {
        "daemon" => run_daemon_suite().await,
        "gui" => gui::run(&args[1..]).await,
        "gui-launch" => {
            // `gui-launch [count] [scenario...]`: a numeric arg sets the record
            // count (default 2000); the rest are scenario names.
            let mut count = 2000usize;
            let mut scenarios = Vec::new();
            for a in &args[1..] {
                match a.parse::<usize>() {
                    Ok(n) => count = n,
                    Err(_) => scenarios.push(a.clone()),
                }
            }
            gui::run_launched(count, &scenarios).await
        }
        "all" => {
            run_daemon_suite().await?;
            println!();
            gui::run(&[]).await
        }
        other => {
            anyhow::bail!(
                "unknown suite '{other}' (expected: daemon | gui | gui-launch | all)"
            )
        }
    }
}

async fn run_daemon_suite() -> Result<()> {
    println!("=== metafolder-bench (daemon suite) ===\n");

    let cli_bin = find_binary("mf")?;
    println!("daemon : {}", find_binary("metafolder-daemon")?.display());
    println!("cli    : {}\n", cli_bin.display());

    // Start daemon
    println!("Starting daemon on port {PORT}...");
    let daemon = daemon_start()?;
    daemon_wait_ready(&daemon.url).await?;
    println!("Daemon ready.\n");

    // ── CLI benchmarks ────────────────────────────────────────────────────────
    let tmp_cli = tempfile::TempDir::new()?;
    let repo_cli = api_init_repo(&daemon.url, tmp_cli.path()).await?;
    println!("CLI repo: {repo_cli}");

    bench_cli_loop(&daemon.url, &cli_bin, repo_cli).await?;

    let tmp_bulk = tempfile::TempDir::new()?;
    let repo_bulk = api_init_repo(&daemon.url, tmp_bulk.path()).await?;
    println!("\nBulk repo: {repo_bulk}");

    bench_cli_bulk(&daemon.url, &cli_bin, repo_bulk).await?;

    // ── Watcher benchmarks ────────────────────────────────────────────────────
    println!("\n--- Watcher benchmarks ---");
    println!("(files are created then moved inside a watched directory)\n");

    for &n in WATCHER_SIZES {
        println!("Watcher moves ({n} files):");
        bench_watcher_moves(&daemon.url, n).await?;
    }

    for &n in WATCHER_SIZES {
        println!("\nWatcher folder rename ({n} files):");
        bench_watcher_folder(&daemon.url, n).await?;
    }

    println!("\n=== done ===");
    Ok(())
}
