//! Benchmark suite for metafolder — CLI and watcher performance.
//!
//! Usage:
//!   cargo build                        # build debug binaries first
//!   cargo run -p metafolder-bench
//!
//!   cargo build --release              # or release for more realistic numbers
//!   cargo run -p metafolder-bench --release

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use reqwest::Client;
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

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn find_binary(name: &str) -> Result<PathBuf> {
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

fn daemon_start() -> Result<Daemon> {
    let bin = find_binary("metafolder-daemon")?;
    let child = Command::new(&bin)
        .args(["--port", &PORT.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("Failed to spawn {:?}", bin))?;
    Ok(Daemon {
        _proc: child,
        url: format!("http://127.0.0.1:{PORT}"),
    })
}

async fn daemon_wait_ready(url: &str) -> Result<()> {
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

async fn api_init_repo(url: &str, root: &Path) -> Result<Uuid> {
    Ok(Client::new()
        .post(format!("{url}/repos/init"))
        .json(&serde_json::json!({ "root": root }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

async fn api_create_entry(url: &str, repo: Uuid, rating: i64) -> Result<Uuid> {
    let v: serde_json::Value = Client::new()
        .post(format!("{url}/repos/{repo}/entries"))
        .json(&serde_json::json!({
            "fields": [{"name":"rating","value":{"type":"int","value":rating}}]
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(v["uuid"].as_str().context("missing uuid field")?.parse()?)
}

/// Create `n` entries concurrently via HTTP. Returns their UUIDs.
async fn api_create_n(url: &str, repo: Uuid, n: usize) -> Result<Vec<Uuid>> {
    let sem = Arc::new(Semaphore::new(HTTP_CONCURRENCY));
    let mut handles = Vec::with_capacity(n);
    for i in 0..n {
        let (url, sem) = (url.to_string(), sem.clone());
        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            api_create_entry(&url, repo, i as i64).await
        }));
    }
    let mut uuids = Vec::with_capacity(n);
    for h in handles {
        uuids.push(h.await??);
    }
    Ok(uuids)
}

/// Returns the total number of entries in the repo.
async fn api_entry_count(url: &str, repo: Uuid) -> Result<usize> {
    let v: Vec<Uuid> = Client::new()
        .get(format!("{url}/repos/{repo}/entries"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(v.len())
}

/// Returns true if any entry has path equal to `path`.
async fn api_path_exists(url: &str, repo: Uuid, path: &str) -> Result<bool> {
    let v: Vec<Uuid> = Client::new()
        .post(format!("{url}/repos/{repo}/query"))
        .json(&serde_json::json!({
            "op": "Eq",
            "field": "path",
            "value": { "type": "string", "value": path }
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(!v.is_empty())
}

/// Poll until the repo has at least `expected` entries (or TIMEOUT).
async fn wait_for_entries(url: &str, repo: Uuid, expected: usize) -> Result<()> {
    let start = Instant::now();
    loop {
        if api_entry_count(url, repo).await? >= expected {
            return Ok(());
        }
        if start.elapsed() > TIMEOUT {
            anyhow::bail!("Timeout waiting for {expected} entries in repo");
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Poll until the given path appears in the DB. Returns elapsed time, or None on timeout.
async fn wait_for_path(url: &str, repo: Uuid, path: &str, since: Instant) -> Option<Duration> {
    loop {
        if api_path_exists(url, repo, path).await.unwrap_or(false) {
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

// ─── Bench 1: CLI latency (single-entry loop) ─────────────────────────────────

async fn bench_cli_loop(url: &str, bin: &Path, repo: Uuid) -> Result<()> {
    let n = LOOP_N;

    // Pre-create one persistent entry (for get/set) and n entries (for delete)
    let entry = api_create_entry(url, repo, 99).await?;
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
        &format!("CLI — latency per command  (×{n}, DB ~{n} entries)"),
        &rows,
    );
    Ok(())
}

// ─── Bench 2: CLI bulk (large number of entries) ──────────────────────────────

async fn bench_cli_bulk(url: &str, bin: &Path, repo: Uuid) -> Result<()> {
    let mut rows: Vec<(String, usize, Duration)> = Vec::new();
    let mut total_entries = 0usize;

    for &n in BULK_SIZES {
        print!("  pre-populating +{n} entries... ");
        let t = Instant::now();
        api_create_n(url, repo, n).await?;
        total_entries += n;
        println!("done in {:.0} ms  (DB = {} entries)", ms(t.elapsed()), total_entries);

        let db = total_entries;

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

    print_section("CLI — bulk (commands on large number of entries)", &rows);
    Ok(())
}

// ─── Bench 3: Watcher — individual file moves ─────────────────────────────────

async fn bench_watcher_moves(url: &str, n: usize) -> Result<()> {
    let tmp = tempfile::TempDir::new()?;
    let root = tmp.path();
    let src = root.join("src");
    let dst = root.join("dst");

    // Create subdirectories BEFORE init so the watcher starts already watching them.
    // If we created them after init, inotify might not have registered the new dirs
    // before we start creating files inside them, causing events to be missed.
    std::fs::create_dir_all(&src)?;
    std::fs::create_dir_all(&dst)?;

    let repo = api_init_repo(url, root).await?;
    println!("  repo: {repo}");

    // Create N files; the last one serves as the sentinel
    for i in 0..n {
        std::fs::write(src.join(format!("file_{i:05}.txt")), b"")?;
    }
    let sentinel_old = src.join(format!("file_{:05}.txt", n - 1));
    let sentinel_new = dst.join(format!("file_{:05}.txt", n - 1));
    let sentinel_str = sentinel_new.to_string_lossy().into_owned();

    // Wait for the watcher to register all files before timing the moves
    print!("  waiting for watcher to register {n} files... ");
    wait_for_entries(url, repo, n).await?;
    println!("ok");

    // ── Timer start ──
    let t = Instant::now();

    for i in 0..n {
        let old = src.join(format!("file_{i:05}.txt"));
        let new = dst.join(format!("file_{i:05}.txt"));
        std::fs::rename(&old, &new)?;
    }

    let elapsed = match wait_for_path(url, repo, &sentinel_str, t).await {
        Some(d) => d,
        None => {
            println!("  ⚠  Timeout ({TIMEOUT:?}) — sentinel not detected after {n} renames");
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

    // Verify: check that the sentinel entry was actually updated
    let updated = api_path_exists(url, repo, &sentinel_str).await?;
    let still_old = api_path_exists(url, repo, &sentinel_old.to_string_lossy()).await?;
    println!(
        "  Sentinel check: new path found={updated}  old path still present={still_old}"
    );

    Ok(())
}

// ─── Bench 4: Watcher — folder rename ─────────────────────────────────────────

async fn bench_watcher_folder(url: &str, n: usize) -> Result<()> {
    let tmp = tempfile::TempDir::new()?;
    let root = tmp.path();
    let subdir = root.join("subdir");
    let subdir_moved = root.join("subdir_moved");

    // Create subdirectory BEFORE init so the watcher starts already watching it.
    std::fs::create_dir_all(&subdir)?;

    let repo = api_init_repo(url, root).await?;
    println!("  repo: {repo}");

    for i in 0..n {
        std::fs::write(subdir.join(format!("file_{i:05}.txt")), b"")?;
    }

    // Wait for the watcher to register all N files
    print!("  waiting for watcher to register {n} files... ");
    wait_for_entries(url, repo, n).await?;
    println!("ok");

    // ── Timer start ──
    let t = Instant::now();

    // Rename the whole folder
    std::fs::rename(&subdir, &subdir_moved)?;

    // Create a sentinel FILE in the root (after the folder rename).
    // When the watcher processes this Create event, we know all prior events
    // (including the folder rename) have been handled.
    let sentinel = root.join("sentinel_folder.txt");
    std::fs::write(&sentinel, b"")?;
    let sentinel_str = sentinel.to_string_lossy().into_owned();

    let elapsed = match wait_for_path(url, repo, &sentinel_str, t).await {
        Some(d) => d,
        None => {
            println!("  ⚠  Timeout ({TIMEOUT:?})");
            TIMEOUT
        }
    };
    // ── Timer end ──

    // Check whether the folder rename was actually handled by the watcher.
    // If handled: entries should have new paths (subdir_moved/...).
    // If not:     entries still have old paths (subdir/...) or Nothing.
    let first_old = subdir.join("file_00000.txt").to_string_lossy().into_owned();
    let first_new = subdir_moved
        .join("file_00000.txt")
        .to_string_lossy()
        .into_owned();
    let old_exists = api_path_exists(url, repo, &first_old).await?;
    let new_exists = api_path_exists(url, repo, &first_new).await?;

    let update_status = match (old_exists, new_exists) {
        (false, true) => "✓ path updated",
        (true, false) => "✗ old path retained (folder rename not handled)",
        (false, false) => "✗ path cleared (Nothing)",
        (true, true) => "? inconsistent state",
    };

    let rows = vec![(format!("{n} files inside a renamed folder"), 1, elapsed)];
    print_section(
        &format!("Watcher — folder rename ({n} files)"),
        &rows,
    );
    println!("  Entry state after rename: {update_status}");

    Ok(())
}

// ─── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    println!("=== metafolder-bench ===\n");

    let cli_bin = find_binary("metafolder")?;
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
