//! GUI benchmark scenarios — a Rust port of `scripts/bench-gui.sh`.
//!
//! These drive an **already-running** GUI through its `/gui/*` scripting HTTP
//! API (the same one `mf gui` uses) and read back the panel phase timings the
//! panels record via `performance.measure` (`mf:list:*`, `mf:detail:*`,
//! `mf:fm:*`) plus the auto-instrumented daemon round-trips (`mf:daemon …`).
//!
//! Alongside each scenario we also time the equivalent raw daemon call so the
//! GUI overhead can be read off directly: e.g. "list 11000 metarecords over
//! plain HTTP" vs "the metarecord-list panel showing the same repo".
//!
//! Prerequisites (same as the shell harness): the GUI is running with a
//! repository loaded in the focused workspace, and the installed panels carry
//! the bench instrumentation (re-run `metafolder-sync-config` after editing the
//! shipped panel sources).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::ms;

// Pacing, mirroring bench-gui.sh's STEP / SETTLE / STEPS. Panel fetch/render
// happen asynchronously after a command returns, so we settle before reading.
const SETTLE: Duration = Duration::from_millis(500);
const STEP: Duration = Duration::from_millis(250);
const STEPS: usize = 10;

const ALL_SCENARIOS: &[&str] =
    &["open-list", "open-detail", "open-fm", "list-detail-nav", "fm-nav", "paging"];

#[derive(Deserialize)]
struct BenchRecord {
    name: String,
    duration_ms: f64,
}

/// Entry point for the GUI suite. `scenarios` selects which to run (empty = all).
pub async fn run(scenarios: &[String]) -> Result<()> {
    println!("=== metafolder-bench (GUI suite) ===\n");

    let (gui_url, daemon_url) = discover_urls();
    let gui = match Gui::connect(gui_url.clone(), daemon_url.clone()).await? {
        Some(gui) => gui,
        None => {
            println!("GUI not reachable at {gui_url} — start the GUI first, then re-run.");
            println!("(override with METAFOLDER_GUI_URL / METAFOLDER_DAEMON_URL)");
            return Ok(());
        }
    };

    let Some(repo) = gui.focused_repo().await? else {
        println!("No repository in the focused workspace — load one in the GUI first.");
        return Ok(());
    };
    println!("GUI    : {gui_url}");
    println!("daemon : {daemon_url}");
    println!("repo   : {repo}\n");

    let selected: Vec<&str> = if scenarios.is_empty() {
        ALL_SCENARIOS.to_vec()
    } else {
        for s in scenarios {
            if !ALL_SCENARIOS.contains(&s.as_str()) {
                anyhow::bail!("unknown GUI scenario '{s}' (one of: {})", ALL_SCENARIOS.join(", "));
            }
        }
        scenarios.iter().map(String::as_str).collect()
    };

    // Save the layout so we leave the GUI roughly as we found it.
    let saved_layout = gui.layout_get().await.ok();

    for name in selected {
        let result = match name {
            "open-list" => gui.scenario_open_list(&repo).await,
            "open-detail" => gui.scenario_open_detail(&repo).await,
            "open-fm" => gui.scenario_open_fm(&repo).await,
            "list-detail-nav" => gui.scenario_list_detail_nav(&repo).await,
            "fm-nav" => gui.scenario_fm_nav(&repo).await,
            "paging" => gui.scenario_paging(&repo).await,
            _ => unreachable!("validated above"),
        };
        if let Err(e) = result {
            println!("  scenario '{name}' failed: {e:#}\n");
        }
    }

    if let Some(layout) = saved_layout {
        let _ = gui.layout_put(&layout).await;
    }
    println!("=== done ===");
    Ok(())
}

// ─── URL discovery ─────────────────────────────────────────────────────────────

/// Resolves (gui_url, daemon_url) the same way `mf gui` and the GUI itself do:
/// env override wins, then the GUI `config.toml` (`gui-port` / `daemon-url`),
/// then the defaults (7524 / 7523).
fn discover_urls() -> (String, String) {
    let mut gui_port: Option<u16> = None;
    let mut daemon_url: Option<String> = None;
    if let Some(path) = gui_config_path() {
        if let Ok(content) = std::fs::read_to_string(path) {
            for line in content.lines() {
                let line = line.trim();
                if let Some(rest) = line.strip_prefix("gui-port") {
                    gui_port = rest.trim_start_matches([' ', '=']).trim().parse().ok();
                } else if let Some(rest) = line.strip_prefix("daemon-url") {
                    daemon_url = Some(rest.trim_start_matches([' ', '=']).trim().trim_matches('"').to_string());
                }
            }
        }
    }
    let gui_url = std::env::var("METAFOLDER_GUI_URL")
        .unwrap_or_else(|_| format!("http://127.0.0.1:{}", gui_port.unwrap_or(7524)));
    let daemon_url = std::env::var("METAFOLDER_DAEMON_URL")
        .ok()
        .or(daemon_url)
        .unwrap_or_else(|| "http://127.0.0.1:7523".to_string());
    (gui_url, daemon_url)
}

/// `~/.config/metafolder/gui/config.toml` from `$XDG_CONFIG_HOME` or `$HOME`.
fn gui_config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("metafolder").join("gui").join("config.toml"))
}

// ─── GUI client ────────────────────────────────────────────────────────────────

struct Gui {
    http: Client,
    gui_url: String,
    daemon_url: String,
}

impl Gui {
    /// Connects and confirms the GUI answers `GET /gui/status`. Returns
    /// `Ok(None)` when the GUI is simply not running (connection refused).
    async fn connect(gui_url: String, daemon_url: String) -> Result<Option<Gui>> {
        let gui = Gui { http: Client::new(), gui_url, daemon_url };
        match gui.http.get(format!("{}/gui/status", gui.gui_url)).send().await {
            Ok(resp) => {
                resp.error_for_status().context("GUI status request failed")?;
                Ok(Some(gui))
            }
            Err(e) if e.is_connect() => Ok(None),
            Err(e) => Err(e).context("contacting the GUI"),
        }
    }

    /// The active repository uuid of the focused workspace (mirrors `mf gui repo`).
    async fn focused_repo(&self) -> Result<Option<String>> {
        let status: Value = self
            .http
            .get(format!("{}/gui/status", self.gui_url))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let Some(layout) = status["layout"].as_object() else { return Ok(None) };
        let ws_id = layout
            .values()
            .find(|slot| slot["focused"] == true)
            .and_then(|slot| slot["workspace_id"].as_str());
        let Some(ws_id) = ws_id else { return Ok(None) };
        let repo = status["workspaces"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|ws| ws["id"] == ws_id)
            .and_then(|ws| ws["active_repo"].as_str())
            .map(str::to_string);
        Ok(repo)
    }

    async fn workspace_new(&self, repo: &str) -> Result<String> {
        let resp: Value = self
            .http
            .post(format!("{}/gui/workspaces", self.gui_url))
            .json(&json!({ "active_repo": repo }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp["id"].as_str().context("workspace create: missing id")?.to_string())
    }

    async fn workspace_rm(&self, id: &str) {
        let _ = self
            .http
            .delete(format!("{}/gui/workspaces/{id}", self.gui_url))
            .send()
            .await;
    }

    async fn layout_get(&self) -> Result<Value> {
        Ok(self
            .http
            .get(format!("{}/gui/layout", self.gui_url))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    async fn layout_put(&self, body: &Value) -> Result<()> {
        self.http
            .put(format!("{}/gui/layout", self.gui_url))
            .json(body)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    async fn layout_set(&self, slot: &str, ws: &str) -> Result<()> {
        self.layout_put(&json!({ slot: ws })).await
    }

    async fn view_set(&self, slot: &str, panel_type: &str) -> Result<()> {
        self.http
            .put(format!("{}/gui/panels/{slot}/view", self.gui_url))
            .json(&json!({ "type": panel_type }))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// Runs a panel/global command. Best-effort: a missing command or error
    /// event is ignored (the shell harness does the same with `|| true`).
    async fn command(&self, invocation: &str) {
        let _ = self
            .http
            .post(format!("{}/gui/command", self.gui_url))
            .json(&json!({ "invocation": invocation, "timeout_ms": 5000 }))
            .send()
            .await;
    }

    async fn bench_clear(&self) -> Result<()> {
        self.http
            .post(format!("{}/gui/bench/clear", self.gui_url))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    async fn bench_read(&self) -> Result<Vec<BenchRecord>> {
        #[derive(Deserialize)]
        struct BenchResponse {
            records: Vec<BenchRecord>,
        }
        let resp: BenchResponse = self
            .http
            .get(format!("{}/gui/bench", self.gui_url))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp.records)
    }

    /// Times the raw daemon calls a list panel ultimately makes: the full
    /// metarecord listing (every uuid) and a single page. Printed next to the
    /// panel timings so GUI overhead is the difference.
    async fn http_baseline(&self, repo: &str) -> Result<()> {
        let base = format!("{}/repos/{repo}/metarecords", self.daemon_url);

        let t = Instant::now();
        let all: Vec<String> =
            self.http.get(&base).send().await?.error_for_status()?.json().await?;
        let full = t.elapsed();

        let t = Instant::now();
        let _page: Value = self
            .http
            .get(format!("{base}?limit=500"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let page = t.elapsed();

        println!(
            "  HTTP baseline (same repo, {} metarecords): full list {:.1} ms · first page(500) {:.1} ms",
            all.len(),
            ms(full),
            ms(page),
        );
        Ok(())
    }

    // ── Scenarios ──────────────────────────────────────────────────────────────
    //
    // Each runs in its own fresh workspace so the panel's first load is
    // uncached (PanelHost keeps one live panel per workspace×type), then drops
    // it so later panel commands are unambiguous.

    async fn scenario_open_list(&self, repo: &str) -> Result<()> {
        let ws = self.workspace_new(repo).await?;
        self.bench_clear().await?;
        self.layout_set("left", &ws).await?;
        self.view_set("left", "metarecord-list").await?;
        settle().await;
        report("open metarecord-list", &self.bench_read().await?);
        self.http_baseline(repo).await?;
        println!();
        self.workspace_rm(&ws).await;
        Ok(())
    }

    async fn scenario_open_detail(&self, repo: &str) -> Result<()> {
        // Detail loads `selected_metarecord`: select a row in a list first,
        // then measure only the detail panel's first load.
        let ws = self.workspace_new(repo).await?;
        self.layout_set("left", &ws).await?;
        self.view_set("left", "metarecord-list").await?;
        settle().await;
        self.command("metarecord-list:first").await;
        settle().await;
        self.bench_clear().await?;
        self.layout_set("right", &ws).await?;
        self.view_set("right", "metarecord-detail").await?;
        settle().await;
        report("open metarecord-detail (selected row)", &self.bench_read().await?);
        println!();
        self.workspace_rm(&ws).await;
        Ok(())
    }

    async fn scenario_open_fm(&self, repo: &str) -> Result<()> {
        let ws = self.workspace_new(repo).await?;
        self.bench_clear().await?;
        self.layout_set("left", &ws).await?;
        self.view_set("left", "file-manager").await?;
        settle().await;
        report("open file-manager", &self.bench_read().await?);
        println!();
        self.workspace_rm(&ws).await;
        Ok(())
    }

    async fn scenario_list_detail_nav(&self, repo: &str) -> Result<()> {
        let ws = self.workspace_new(repo).await?;
        self.layout_set("left", &ws).await?;
        self.layout_set("right", &ws).await?;
        self.view_set("left", "metarecord-list").await?;
        self.view_set("right", "metarecord-detail").await?;
        settle().await;
        self.command("metarecord-list:first").await;
        settle().await;
        self.bench_clear().await?;
        for _ in 0..STEPS {
            self.command("metarecord-list:next").await;
            tokio::time::sleep(STEP).await; // let the detail panel settle
        }
        settle().await;
        report(&format!("list+detail: {STEPS}× selection down"), &self.bench_read().await?);
        println!();
        self.workspace_rm(&ws).await;
        Ok(())
    }

    async fn scenario_fm_nav(&self, repo: &str) -> Result<()> {
        // The file-manager drives the `file` viewer (not metarecord-detail);
        // this measures the file-manager's own re-render and directory loads.
        let ws = self.workspace_new(repo).await?;
        self.layout_set("left", &ws).await?;
        self.layout_set("right", &ws).await?;
        self.view_set("left", "file-manager").await?;
        self.view_set("right", "file").await?;
        settle().await;
        self.bench_clear().await?;
        for _ in 0..STEPS {
            self.command("file-manager:next").await;
            tokio::time::sleep(STEP).await;
        }
        settle().await;
        report(&format!("file-manager: {STEPS}× selection down"), &self.bench_read().await?);
        println!();
        self.workspace_rm(&ws).await;
        Ok(())
    }

    async fn scenario_paging(&self, repo: &str) -> Result<()> {
        let ws = self.workspace_new(repo).await?;
        self.layout_set("left", &ws).await?;
        self.view_set("left", "metarecord-list").await?;
        settle().await;
        self.bench_clear().await?;
        for _ in 0..STEPS {
            self.command("metarecord-list:page-next").await;
            tokio::time::sleep(STEP).await;
        }
        settle().await;
        report(&format!("paging: {STEPS}× load next page"), &self.bench_read().await?);
        self.http_baseline(repo).await?;
        println!();
        self.workspace_rm(&ws).await;
        Ok(())
    }
}

async fn settle() {
    tokio::time::sleep(SETTLE).await;
}

/// Aggregates the recorded measures by name (count / total / mean / max),
/// sorted by total time, like bench-gui.sh's `report`.
fn report(title: &str, records: &[BenchRecord]) {
    println!("── {title} ──");
    if records.is_empty() {
        println!("  (no measures recorded)");
        return;
    }
    // (count, total, max) per measure name.
    let mut agg: BTreeMap<&str, (usize, f64, f64)> = BTreeMap::new();
    for r in records {
        let e = agg.entry(r.name.as_str()).or_insert((0, 0.0, 0.0));
        e.0 += 1;
        e.1 += r.duration_ms;
        e.2 = e.2.max(r.duration_ms);
    }
    let mut rows: Vec<(&str, usize, f64, f64)> =
        agg.into_iter().map(|(n, (c, total, max))| (n, c, total, max)).collect();
    rows.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    for (name, n, total, max) in rows {
        println!(
            "  {name:<34} n={n:<3} total={total:8.2} ms  mean={:7.2} ms  max={max:7.2} ms",
            total / n as f64,
        );
    }
}
