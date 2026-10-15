//! The session scenario (doc "The session scenario"): a real GUI on a large
//! repository, driven through a day's worth of use, every step timed against
//! its own budget, and the slow log — its thresholds at their floor — telling
//! where each step's time and keys went.
//!
//! Where the regression suite (`regression/`) times a fixed list of daemon
//! calls against their history, this one looks for what nobody listed: the
//! request a panel sends that it should not, the step that reads the whole
//! repository, the polling that costs between two steps. It is a report, not a
//! gate: the budgets say what a user should wait, and every step over it is
//! shown with the operations that made it so.
//!
//! The repository is a hard-linked copy of a data folder (default
//! `benchmarks/bench_data_big`, 50 000 files) rebuilt on every run, so every
//! run starts from the same state. The session never writes into an existing
//! file — a hard link shares its bytes with the original — it only renames,
//! creates and removes.
//!
//! The GUI reads a configuration built from the shipped defaults
//! (`crates/*/default-config/`), not the user's, so the user's edits cannot
//! change a run.

pub mod load;
pub mod report;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use report::{Outcome, Step};

pub struct Options {
    pub data: PathBuf,
    pub unsandboxed: bool,
    pub calibrate: bool,
    pub record: bool,
    /// Record even when the machine is busier than its neutral load.
    pub force: bool,
    /// Keep the work directory (logs, the repository, the report).
    pub keep: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            data: PathBuf::from(crate::DEFAULT_BIG_DIR),
            unsandboxed: false,
            calibrate: false,
            record: false,
            force: false,
            keep: false,
        }
    }
}

// ─── The scenario ─────────────────────────────────────────────────────────────

/// The steps, in the order a session runs them. A budget is what a user
/// should wait for that action on a repository of this size, not what it
/// takes today: a step over it is the finding.
const STEPS: &[Step] = &[
    // Building the repository, through the daemon alone.
    step("repo.init", 3_000, None, "create the repository"),
    step("reconcile.first", 60_000, None, "first reconcile of every file"),
    step("gui.start", 20_000, None, "start the GUI until it takes commands"),
    // The list: open, page, search.
    step("list.open", 1_500, Some(20_000), "open the metarecord list on the repository"),
    step("list.page", 1_500, Some(20_000), "five pages down the list"),
    step("list.finder", 1_000, None, "search two words in the finder"),
    step("list.query", 1_000, Some(20_000), "a range query typed in the query zone"),
    step("list.clear", 1_000, Some(20_000), "clear the search, back to everything"),
    // One metarecord, then the next ones.
    step("detail.open", 1_000, Some(5_000), "open the first metarecord in the other slot"),
    step("detail.nav", 2_000, Some(20_000), "ten metarecords down, the detail following"),
    // The file manager, down to a folder of almost three hundred files.
    step("fm.open", 1_500, Some(20_000), "open the file manager at the root"),
    step("fm.descend", 2_000, Some(20_000), "down to the largest folder, one level at a time"),
    step("fm.root", 1_000, Some(20_000), "back to the root"),
    step("treeref.open", 1_500, Some(20_000), "open the tree explorer"),
    // Writing in bulk, and taking it back.
    step("bulk.add", 10_000, None, "add a field to every metarecord of the list"),
    step("log.undo", 10_000, None, "undo the bulk write"),
    step("log.redo", 10_000, None, "redo it"),
    step("log.open", 2_000, Some(20_000), "open the log panel"),
    // The filesystem changing under the watcher (2 s of quiet before a flush).
    step("fs.rename_dir", 6_000, None, "rename the largest folder on disk"),
    step("fs.create", 8_000, None, "create 1 000 files on disk"),
    step("fs.delete", 8_000, None, "delete those 1 000 files on disk"),
    // Whole-repository maintenance.
    step("duplicates.scan", 30_000, None, "scan for duplicates"),
    step("orphan.detect", 20_000, None, "detect the orphans"),
    step("reconcile.again", 30_000, None, "reconcile again, nothing changed"),
    // Doing nothing: what the panels cost while the user reads.
    step("idle", 11_000, Some(20_000), "ten seconds without input, list and detail shown"),
    // The repository again from disk.
    step("repo.reload", 20_000, None, "unload and load the repository"),
    step("list.reopen", 1_500, Some(20_000), "the list again after the load"),
];

const fn step(
    id: &'static str,
    budget_ms: u64,
    reads_budget: Option<u64>,
    what: &'static str,
) -> Step {
    Step { id, budget_ms, reads_budget, what }
}

/// Everything the steps share.
struct Session {
    daemon_url: String,
    gui_url: String,
    gui: Option<Child>,
    gui_log: PathBuf,
    config_home: PathBuf,
    unsandboxed: bool,
    data: PathBuf,
    repo: Option<Uuid>,
    workspace: Option<String>,
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(mut gui) = self.gui.take() {
            let _ = gui.kill();
            let _ = gui.wait();
        }
    }
}

/// A folder of the data copy and how many files it holds directly, chosen at
/// run time from the copy itself so the scenario follows the data.
fn biggest_folder(root: &Path) -> Result<(PathBuf, usize)> {
    let mut best = (root.to_path_buf(), 0);
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut files = 0;
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() && entry.file_name() != ".metafolder" {
                stack.push(entry.path());
            } else if kind.is_file() {
                files += 1;
            }
        }
        if files > best.1 || (files == best.1 && dir < best.0) {
            best = (dir, files);
        }
    }
    Ok(best)
}

impl Session {
    fn repo(&self) -> Result<Uuid> {
        self.repo.context("no repository yet")
    }

    async fn daemon_post(&self, path: &str, body: &Value) -> Result<Value> {
        let resp = crate::daemon_client()
            .post(format!("{}{path}", self.daemon_url))
            .json(body)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            bail!("POST {path}: {status}: {text}");
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    /// How many metarecords a query matches.
    async fn count(&self, query: Value) -> Result<u64> {
        let path = format!("/repos/{}/query", self.repo()?.simple());
        let v =
            self.daemon_post(&path, &json!({"query": query, "limit": 1, "count": true})).await?;
        v["total"].as_u64().context("no total in the answer")
    }

    /// Waits until `count(query)` satisfies `done`, polling: what the user
    /// sees once the watcher has flushed.
    async fn until_count(&self, query: Value, done: impl Fn(u64) -> bool) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let n = self.count(query.clone()).await?;
            if done(n) {
                return Ok(());
            }
            if Instant::now() > deadline {
                bail!("still {n} after two minutes");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn rel(&self, abs: &Path) -> String {
        format!("/{}", abs.strip_prefix(&self.data).unwrap_or(abs).display())
    }

    // ── The GUI ──────────────────────────────────────────────────────────────

    fn gui_client(&self) -> reqwest::Client {
        crate::authed_client(&metafolder_core::auth::read_token("gui").unwrap_or_default())
    }

    /// Runs a command as the command input would, and fails when it fails.
    async fn command(&self, invocation: &str, confirm: Option<bool>) -> Result<()> {
        self.command_within(invocation, confirm, 300_000).await
    }

    async fn command_within(
        &self,
        invocation: &str,
        confirm: Option<bool>,
        timeout_ms: u64,
    ) -> Result<()> {
        let mut body = json!({"invocation": invocation, "timeout_ms": timeout_ms});
        if let Some(answer) = confirm {
            body["confirm"] = json!(answer);
        }
        let v: Value = self
            .gui_client()
            .post(format!("{}/gui/command", self.gui_url))
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        match v["event"].as_str() {
            Some("ok") => Ok(()),
            Some("error") => bail!("{invocation}: {}", v["message"].as_str().unwrap_or("?")),
            other => bail!("{invocation}: {}", other.unwrap_or("no answer")),
        }
    }

    async fn gui_put(&self, path: &str, body: &Value) -> Result<()> {
        self.gui_client()
            .put(format!("{}{path}", self.gui_url))
            .json(body)
            .send()
            .await?
            .error_for_status()
            .with_context(|| format!("PUT {path}"))?;
        Ok(())
    }

    async fn bench_count(&self) -> Result<usize> {
        let v: Value = self
            .gui_client()
            .get(format!("{}/gui/bench", self.gui_url))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(v["records"].as_array().map_or(0, Vec::len))
    }

    async fn bench_has(&self, name: &str) -> Result<bool> {
        let v: Value = self
            .gui_client()
            .get(format!("{}/gui/bench", self.gui_url))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(v["records"].as_array().into_iter().flatten().any(|r| r["name"] == name))
    }

    async fn bench_clear(&self) -> Result<()> {
        self.gui_client()
            .post(format!("{}/gui/bench/clear", self.gui_url))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// Shows a panel type in a slot, and waits until it has drawn: until the
    /// panel's own measure `drawn` was recorded, when it has one.
    async fn show(&self, slot: &str, panel_type: &str, drawn: Option<&str>) -> Result<()> {
        self.gui_put(&format!("/gui/panels/{slot}/view"), &json!({"type": panel_type})).await?;
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let ready = self
                .gui_client()
                .get(format!("{}/gui/panels/{slot}/view", self.gui_url))
                .send()
                .await?
                .json::<Value>()
                .await?["status"]
                == "ready";
            let drew = match drawn {
                Some(name) => self.bench_has(name).await?,
                None => true,
            };
            if ready && drew {
                return Ok(());
            }
            if Instant::now() > deadline {
                bail!("the {panel_type} panel did not draw within two minutes");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Waits until the panels stop measuring (no new record for `quiet`),
    /// and returns when the last one arrived: a command that resolves before
    /// the panel has redrawn is not done for the user.
    async fn settle(&self, quiet: Duration) -> Result<Instant> {
        let mut seen = self.bench_count().await?;
        let mut last = Instant::now();
        loop {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let now = self.bench_count().await?;
            if now != seen {
                seen = now;
                last = Instant::now();
            } else if last.elapsed() >= quiet {
                return Ok(last);
            }
        }
    }

    async fn start_gui(&mut self) -> Result<()> {
        let bin = crate::find_binary("metafolder-gui")?;
        let gui_port = free_port()?;
        let daemon_port = self.daemon_url.rsplit(':').next().unwrap_or("0").to_string();
        let mut args = vec![
            "--gui-port".to_string(),
            gui_port.to_string(),
            "--daemon-port".into(),
            daemon_port,
        ];
        if self.unsandboxed {
            args.push("--allow-unsandboxed-webview".into());
        }
        let log = std::fs::File::create(&self.gui_log)?;
        let child = Command::new(&bin)
            .args(&args)
            .env("XDG_CONFIG_HOME", &self.config_home)
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .spawn()
            .with_context(|| format!("spawn {}", bin.display()))?;
        self.gui = Some(child);
        self.gui_url = format!("http://127.0.0.1:{gui_port}");
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(gui) = self.gui.as_mut() {
                if let Some(status) = gui.try_wait()? {
                    bail!("the GUI exited ({status}); see {}", self.gui_log.display());
                }
            }
            // Ready = the shell dispatches commands, not merely the server up.
            if self.command_within("status:clear", None, 1000).await.is_ok() {
                return Ok(());
            }
            if Instant::now() > deadline {
                bail!("the GUI did not take commands within a minute");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// The list's search zones run their query on `apply`.
async fn list_search(s: &Session, zone: &str, text: &str) -> Result<()> {
    s.command(&format!("metarecord-list:clear {zone}"), None).await?;
    // Quoted: an unquoted text is split into words, its last one taken for the
    // `stay` modifier.
    let quoted = format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""));
    s.command(&format!("metarecord-list:insert {zone} {quoted} stay"), None).await?;
    s.command(&format!("metarecord-list:apply {zone} stay"), None).await
}

/// When a step that redraws a panel is done for the user.
async fn drawn(s: &Session, quiet: Duration) -> Result<Option<Instant>> {
    s.settle(quiet).await.map(Some)
}

/// Runs one step. What it returns is when the user stopped waiting, when that
/// is later than the step's own return (a panel still drawing).
async fn run_step(id: &str, s: &mut Session, quiet: Duration) -> Result<Option<Instant>> {
    match id {
        "repo.init" => {
            let v =
                s.daemon_post("/repos/init", &json!({"root": s.data, "name": "session"})).await?;
            s.repo = Some(v["repo_uuid"].as_str().context("no repo_uuid")?.parse()?);
            crate::api_enable_watch(&s.daemon_url, s.repo()?).await?;
            Ok(None)
        }
        "reconcile.first" => {
            crate::api_reconcile(&s.daemon_url, s.repo()?, false).await?;
            Ok(None)
        }
        "gui.start" => {
            s.start_gui().await?;
            let repo = s.repo()?.simple().to_string();
            let v: Value = s
                .gui_client()
                .post(format!("{}/gui/workspaces", s.gui_url))
                .json(&json!({"active_repo": repo}))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            let ws = v["id"].as_str().context("no workspace id")?.to_string();
            s.gui_put("/gui/layout", &json!({"left": ws, "right": ws})).await?;
            s.workspace = Some(ws);
            Ok(None)
        }
        "list.open" => {
            s.bench_clear().await?;
            s.show("left", "metarecord-list", Some("mf:list:render")).await?;
            drawn(s, quiet).await
        }
        "list.reopen" => {
            // The list is on screen since `idle`: showing it again draws
            // nothing, reloading it does.
            s.bench_clear().await?;
            s.command("view:refresh metarecord-list", None).await?;
            s.show("left", "metarecord-list", Some("mf:list:render")).await?;
            drawn(s, quiet).await
        }
        "list.page" => {
            for _ in 0..5 {
                s.command("metarecord-list:page next", None).await?;
            }
            drawn(s, quiet).await
        }
        "list.finder" => {
            list_search(s, "finder", "album 2021").await?;
            drawn(s, quiet).await
        }
        "list.query" => {
            list_search(s, "normal", "mfr_size > 150").await?;
            drawn(s, quiet).await
        }
        "list.clear" => {
            s.command("metarecord-list:clear all", None).await?;
            drawn(s, quiet).await
        }
        "detail.open" => {
            s.command("cursor:first metarecord-list", None).await?;
            s.show("right", "metarecord-detail", Some("mf:detail:render")).await?;
            drawn(s, quiet).await
        }
        "detail.nav" => {
            for _ in 0..10 {
                s.command("cursor:next metarecord-list", None).await?;
            }
            drawn(s, quiet).await
        }
        "fm.open" => {
            s.bench_clear().await?;
            s.show("left", "file-manager", Some("mf:fm:render")).await?;
            drawn(s, quiet).await
        }
        "fm.descend" => {
            let (folder, _) = biggest_folder(&s.data)?;
            let rel = folder.strip_prefix(&s.data)?.to_path_buf();
            for part in rel.components() {
                let name = part.as_os_str().to_string_lossy();
                s.command(&format!("cursor:find file-manager select {name}"), None).await?;
                s.command("file-manager:activate other", None).await?;
            }
            drawn(s, quiet).await
        }
        "fm.root" => {
            s.command("cursor:root file-manager", None).await?;
            drawn(s, quiet).await
        }
        "treeref.open" => {
            s.show("left", "treeref", None).await?;
            s.command("view:refresh treeref", None).await?;
            drawn(s, quiet).await
        }
        "bulk.add" => {
            s.show("left", "metarecord-list", Some("mf:list:render")).await?;
            list_search(s, "normal", "mfr_size > 150").await?;
            s.command("metarecord:bulk query add session_mark bool true", Some(true)).await?;
            drawn(s, quiet).await
        }
        "log.undo" => {
            s.command("log:undo", Some(true)).await?;
            drawn(s, quiet).await
        }
        "log.redo" => {
            s.command("log:redo", Some(true)).await?;
            drawn(s, quiet).await
        }
        "log.open" => {
            s.show("right", "log", None).await?;
            s.command("view:refresh log", None).await?;
            drawn(s, quiet).await
        }
        "fs.rename_dir" => {
            let (folder, files) = biggest_folder(&s.data)?;
            let moved = folder.with_file_name(format!(
                "{}-moved",
                folder.file_name().unwrap_or_default().to_string_lossy()
            ));
            std::fs::rename(&folder, &moved)?;
            let target = s.rel(&moved);
            s.until_count(subtree(&target), |n| n >= files as u64).await?;
            Ok(None)
        }
        "fs.create" => {
            let dir = s.data.join("session-new");
            std::fs::create_dir(&dir)?;
            for i in 0..1000 {
                std::fs::write(dir.join(format!("new_{i:04}.txt")), format!("session file {i}\n"))?;
            }
            s.until_count(subtree("/session-new"), |n| n >= 1000).await?;
            Ok(None)
        }
        "fs.delete" => {
            let dir = s.data.join("session-new");
            for i in 0..1000 {
                std::fs::remove_file(dir.join(format!("new_{i:04}.txt")))?;
            }
            s.until_count(subtree("/session-new"), |n| n == 0).await?;
            Ok(None)
        }
        "duplicates.scan" => {
            s.command("mf:duplicate scan", Some(true)).await?;
            Ok(None)
        }
        "orphan.detect" => {
            s.command("orphan:detect", Some(true)).await?;
            Ok(None)
        }
        "reconcile.again" => {
            s.command("reconcile:run", Some(true)).await?;
            Ok(None)
        }
        "idle" => {
            s.show("left", "metarecord-list", None).await?;
            s.show("right", "metarecord-detail", None).await?;
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok(None)
        }
        "repo.reload" => {
            let repo = s.repo()?;
            s.daemon_post(&format!("/repos/{}/unload", repo.simple()), &json!({})).await?;
            let v = s.daemon_post("/repos/load", &json!({"root": s.data})).await?;
            let again: Uuid = v["repo_uuid"].as_str().context("no repo_uuid")?.parse()?;
            if again != repo {
                bail!("the repository came back as {again}");
            }
            // Loaded = serving, not accepted (the load warms up in the background).
            let deadline = Instant::now() + Duration::from_secs(300);
            while s.count(present("mfr_path")).await.is_err() {
                if Instant::now() > deadline {
                    bail!("the repository did not serve within five minutes");
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Ok(None)
        }
        other => bail!("unknown step {other}"),
    }
}

fn present(field: &str) -> Value {
    json!({ "type": "is_present", "field": field })
}

/// Everything below a folder, by its path from the root.
fn subtree(path: &str) -> Value {
    json!({ "type": "follows_transitive", "field": "mfr_path", "target": path })
}

fn free_port() -> Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

// ─── The environment ──────────────────────────────────────────────────────────

/// The daemon's configuration: every threshold of the slow log at its floor,
/// so every operation lands in it with its phases and keys; no automatic
/// backup in the middle of a step.
const DAEMON_CONFIG: &str = "\
[settings]
slow-operation-threshold-ms = 1
slow-read-threshold-ms = 1
slow-operation-reads-threshold = 1
auto-backup-days = 0
";

fn copy_dir(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let to = dest.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

/// The GUI's configuration home: the shipped defaults, its slow log at 1 ms.
fn build_config_home(home: &Path) -> Result<()> {
    let crates = crate::repo_root().join("crates");
    for part in ["core", "cli", "gui"] {
        let src = crates.join(part).join("default-config");
        if src.is_dir() {
            copy_dir(&src, &home.join("metafolder").join(part))?;
        }
    }
    let path = home.join("metafolder/gui/config.toml");
    let text = std::fs::read_to_string(&path).context("no shipped GUI config.toml")?;
    let patched = set_toml_key(&text, "settings", "slow-operation-threshold-ms", "1");
    std::fs::write(&path, patched)?;
    Ok(())
}

/// `text` with `key = value` in `[table]`: the line replaced where the key is
/// there (commented out or not), added at the top of the table otherwise.
fn set_toml_key(text: &str, table: &str, key: &str, value: &str) -> String {
    let header = format!("[{table}]");
    let mut out = Vec::new();
    let mut in_table = false;
    let mut done = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_table = trimmed == header;
            out.push(line.to_string());
            if in_table && !done && !text_has_key(text, &header, key) {
                out.push(format!("{key} = {value}"));
                done = true;
            }
            continue;
        }
        let bare = trimmed.trim_start_matches('#').trim_start();
        if in_table && !done && bare.split('=').next().map(str::trim) == Some(key) {
            out.push(format!("{key} = {value}"));
            done = true;
            continue;
        }
        out.push(line.to_string());
    }
    if !done {
        out.push(String::new());
        out.push(header);
        out.push(format!("{key} = {value}"));
    }
    out.join("\n") + "\n"
}

fn text_has_key(text: &str, header: &str, key: &str) -> bool {
    let mut in_table = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_table = trimmed == header;
        } else if in_table
            && trimmed.trim_start_matches('#').trim_start().split('=').next().map(str::trim)
                == Some(key)
        {
            return true;
        }
    }
    false
}

// ─── The history ──────────────────────────────────────────────────────────────

/// One recorded session.
#[derive(Debug, Serialize, Deserialize)]
struct Recorded {
    at: String,
    commit: String,
    dirty: bool,
    machine: String,
    data: String,
    load_before: load::Load,
    load_after: load::Load,
    steps: Vec<RecordedStep>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RecordedStep {
    id: String,
    wall_ms: f64,
    budget_ms: u64,
    reads: u64,
    ops: usize,
    error: Option<String>,
}

fn history_dir() -> PathBuf {
    crate::repo_root().join("benchmarks").join("session")
}

// ─── The run ──────────────────────────────────────────────────────────────────

pub async fn run(opts: &Options) -> Result<i32> {
    let machine = crate::regression::machine_name();
    let neutral_path = history_dir().join(format!("{machine}.neutral.json"));
    if opts.calibrate {
        println!("measuring the neutral load of {machine} for 30 s — touch nothing ...");
        let neutral = load::sample(Duration::from_secs(30)).await?;
        load::write_neutral(&neutral_path, &neutral)?;
        println!(
            "neutral load: {:.1} % CPU, load average {:.2} — written to {}",
            neutral.cpu_percent,
            neutral.loadavg_1m,
            neutral_path.display()
        );
        return Ok(0);
    }

    crate::gui::ensure_frontend_built()?;
    let neutral = load::read_neutral(&neutral_path)?;
    let window = Duration::from_secs(if opts.record { 10 } else { 3 });
    let load_before = load::sample(window).await?;
    let check = load::compare(neutral.as_ref(), &load_before);
    println!(
        "load before: {:.1} % CPU, load average {:.2} ({})",
        load_before.cpu_percent,
        load_before.loadavg_1m,
        describe(&check)
    );
    if opts.record {
        match check {
            load::Verdict::Uncalibrated => {
                bail!("no neutral load for {machine}: run with --calibrate first")
            }
            load::Verdict::Busier(_) if !opts.force => {
                eprintln!("the machine is busier than neutral: not recording (--force to anyway)");
                return Ok(2);
            }
            _ => {}
        }
    }

    let work = crate::repo_root().join("target/bench-data/session");
    if work.exists() {
        std::fs::remove_dir_all(&work)?;
    }
    std::fs::create_dir_all(&work)?;
    let data = work.join("data");
    print!("linking {} ... ", opts.data.display());
    let files = crate::regression::real::link_tree(&opts.data, &data)?;
    println!("{files} files");
    let config_home = work.join("config");
    build_config_home(&config_home)?;
    let daemon_config = work.join("daemon.toml");
    std::fs::write(&daemon_config, DAEMON_CONFIG)?;

    let port = free_port()?;
    let daemon_bin = crate::find_binary("metafolder-daemon")?;
    let daemon_log = std::fs::File::create(work.join("daemon.log"))?;
    let _daemon = crate::Daemon::from_child(
        Command::new(&daemon_bin)
            .args(["--port", &port.to_string()])
            .arg("--config")
            .arg(&daemon_config)
            .stdout(Stdio::from(daemon_log.try_clone()?))
            .stderr(Stdio::from(daemon_log))
            .spawn()
            .with_context(|| format!("spawn {}", daemon_bin.display()))?,
        port,
    );
    crate::daemon_wait_ready(&format!("http://127.0.0.1:{port}")).await?;

    let mut session = Session {
        daemon_url: format!("http://127.0.0.1:{port}"),
        gui_url: String::new(),
        gui: None,
        gui_log: work.join("gui.log"),
        config_home,
        unsandboxed: opts.unsandboxed,
        data: data.clone(),
        repo: None,
        workspace: None,
    };

    let quiet = Duration::from_millis(300);
    let mut outcomes = Vec::new();
    let mut failed = false;
    for step in STEPS {
        print!("{:<20} ", step.id);
        use std::io::Write as _;
        std::io::stdout().flush().ok();
        let start_ms = now_ms();
        let started = Instant::now();
        if failed {
            println!("skipped");
            outcomes.push(Outcome { id: step.id.to_string(), skipped: true, ..Outcome::default() });
            continue;
        }
        let result = run_step(step.id, &mut session, quiet).await;
        let ended = match &result {
            Ok(Some(drawn)) => *drawn,
            _ => Instant::now(),
        };
        let wall_ms = crate::ms(ended.saturating_duration_since(started));
        let error = result.err().map(|e| format!("{e:#}"));
        // A step that cannot run leaves the session in no known state: what
        // follows would measure something else.
        failed |= error.is_some();
        println!("{}", error.as_deref().map_or_else(|| report::human_ms(wall_ms), str::to_string));
        outcomes.push(Outcome {
            id: step.id.to_string(),
            start_ms,
            end_ms: now_ms(),
            wall_ms,
            error,
            skipped: false,
        });
    }
    // The settle window of the last step is the one place a late entry starts.
    tokio::time::sleep(quiet).await;

    let load_after = load::sample(window).await?;
    println!(
        "load after: {:.1} % CPU, load average {:.2} ({})",
        load_after.cpu_percent,
        load_after.loadavg_1m,
        describe(&load::compare(neutral.as_ref(), &load_after))
    );

    let slow_dir = metafolder_core::slowlog::slow_dir(&data.join(".metafolder").join("internal"));
    let (mut entries, _) = metafolder_core::slowlog::read(&slow_dir, usize::MAX, None);
    entries.reverse();
    let (per_step, between) = report::attribute(entries, &outcomes);
    let text = report::render(STEPS, &outcomes, &per_step, &between);
    println!("\n{text}");
    std::fs::write(work.join("report.txt"), &text)?;

    if opts.record {
        let (commit, dirty) = crate::regression::git_state(&crate::repo_root());
        let recorded = Recorded {
            at: crate::regression::timestamp(),
            commit,
            dirty,
            machine: machine.clone(),
            data: opts.data.display().to_string(),
            load_before,
            load_after,
            steps: STEPS
                .iter()
                .zip(&outcomes)
                .zip(&per_step)
                .map(|((step, o), entries)| RecordedStep {
                    id: step.id.to_string(),
                    wall_ms: o.wall_ms,
                    budget_ms: step.budget_ms,
                    reads: report::daemon_reads(entries),
                    ops: entries.len(),
                    error: o.error.clone(),
                })
                .collect(),
        };
        let path = history_dir().join(format!("{machine}.jsonl"));
        std::fs::create_dir_all(history_dir())?;
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
        writeln!(file, "{}", serde_json::to_string(&recorded)?)?;
        println!("recorded in {}", path.display());
    }

    drop(session);
    if opts.keep {
        println!("work directory kept: {}", work.display());
    } else {
        // The daemon still holds the repository: it goes when `_daemon` drops,
        // after which the copy can go.
        drop(_daemon);
        std::fs::remove_dir_all(&work).ok();
    }
    Ok(if failed { 1 } else { 0 })
}

fn describe(check: &load::Verdict) -> String {
    match check {
        load::Verdict::Neutral => "neutral".into(),
        load::Verdict::Busier(points) => format!("{points:.1} points busier than neutral"),
        load::Verdict::Uncalibrated => "no neutral load recorded".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_step_id_is_unique() {
        let mut ids: Vec<_> = STEPS.iter().map(|s| s.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), STEPS.len());
    }

    #[test]
    fn a_key_is_replaced_in_its_table_and_added_when_missing() {
        let text = "a = 1\n[settings]\nx = 2\nslow-operation-threshold-ms = 2000\n[other]\ny = 3\n";
        let out = set_toml_key(text, "settings", "slow-operation-threshold-ms", "1");
        assert!(
            out.contains("[settings]\nx = 2\nslow-operation-threshold-ms = 1\n[other]"),
            "{out}"
        );
        let out = set_toml_key("[settings]\nx = 2\n", "settings", "k", "1");
        assert_eq!(out, "[settings]\nk = 1\nx = 2\n");
        let out = set_toml_key("a = 1\n", "settings", "k", "1");
        assert_eq!(out, "a = 1\n\n[settings]\nk = 1\n");
        let out = set_toml_key("[other]\nk = 5\n[settings]\n# k = 9\n", "settings", "k", "1");
        assert_eq!(out, "[other]\nk = 5\n[settings]\nk = 1\n", "only the key of the table");
    }

    #[test]
    fn the_biggest_folder_is_the_one_holding_the_most_files_directly() {
        let root = std::env::temp_dir()
            .join("metafolder-tests")
            .join(format!("session-biggest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("a/deep")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        for i in 0..3 {
            std::fs::write(root.join(format!("a/deep/{i}")), "x").unwrap();
        }
        std::fs::write(root.join("b/0"), "x").unwrap();
        let (dir, n) = biggest_folder(&root).unwrap();
        std::fs::remove_dir_all(&root).ok();
        assert_eq!((dir.strip_prefix(&root).unwrap().to_path_buf(), n), ("a/deep".into(), 3));
    }
}
