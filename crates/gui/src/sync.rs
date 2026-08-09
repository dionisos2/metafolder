//! Cross-repo synchronisation for the GUI (spec-sync): Tauri commands over the
//! shared [`metafolder_core::sync`] orchestration.
//!
//! Core's orchestration is **synchronous and blocking** (long sequences of HTTP
//! calls), so it cannot ride the async reqwest [`DaemonProxy`]. Each command
//! runs it under [`tokio::task::spawn_blocking`] with a dedicated blocking
//! `ureq` client, and answers the interactive seams non-interactively: conflicts
//! are left unresolved (`skip`) for `plan_resolve` editing in the panel, the run
//! confirmation is implicit (the panel already confirmed), and warnings are
//! collected into the returned JSON.
//!
//! [`DaemonProxy`]: crate::daemon_proxy::DaemonProxy

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use uuid::Uuid;

use metafolder_core::sync::plan::PlanReport;
use metafolder_core::sync::run::{RunReport, RunStatus, ShowOp, ShowReport};
use metafolder_core::sync::{self as core_sync, DaemonClient, Prompter, SyncCtx, SyncError};

use crate::commands::App;

/// Pagination size for the internal query loops (mirrors the CLI default).
const PAGE_SIZE: usize = 500;

/// A blocking daemon client for the sync orchestration. Mirrors the CLI client
/// (auth token, `{"error": …}` bodies → `SyncError::Op`, transport → `Op`).
struct BlockingClient {
    base: String,
    token: Option<String>,
    agent: ureq::Agent,
}

impl BlockingClient {
    fn new(base: String) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
            token: metafolder_core::auth::read_token("daemon").ok(),
            agent: ureq::Agent::new(),
        }
    }
}

impl DaemonClient for BlockingClient {
    fn request(
        &self,
        method: &str,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<Value, SyncError> {
        let url = format!("{}{}", self.base, path);
        let mut req = self.agent.request(method, &url);
        if let Some(token) = &self.token {
            req = req.set("Authorization", &format!("Bearer {token}"));
        }
        for (key, value) in query {
            req = req.query(key, value);
        }
        let result = match body {
            Some(json) => req.send_json(json),
            None => req.call(),
        };
        match result {
            Ok(response) => Ok(response.into_json().unwrap_or(Value::Null)),
            Err(ureq::Error::Status(code, response)) => {
                let body: Value = response.into_json().unwrap_or(Value::Null);
                let message = body["error"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("daemon returned HTTP {code}"));
                Err(SyncError::Op(message))
            }
            Err(ureq::Error::Transport(t)) => {
                Err(SyncError::Op(format!("cannot reach the daemon at {}: {t}", self.base)))
            }
        }
    }
}

/// The non-interactive prompter (spec-sync, GUI): skip conflicts (left for
/// `plan_resolve` editing), confirm implicitly, collect warnings for the panel.
#[derive(Default)]
struct GuiPrompter {
    warnings: Mutex<Vec<String>>,
}

impl Prompter for GuiPrompter {
    fn resolve_conflict(&self, _field: &str, _rec_a: Uuid, _rec_b: Uuid) -> Result<String, SyncError> {
        Ok("skip".into())
    }

    fn confirm(&self, _message: &str) -> Result<bool, SyncError> {
        Ok(true)
    }

    fn warn(&self, message: &str) {
        if let Ok(mut w) = self.warnings.lock() {
            w.push(message.to_string());
        }
    }
}

/// Runs `f` against a freshly built [`SyncCtx`] on a blocking thread, returning
/// its result together with any warnings the orchestration emitted.
async fn blocking<T, F>(base: String, f: F) -> Result<(T, Vec<String>), String>
where
    T: Send + 'static,
    F: FnOnce(&SyncCtx) -> Result<T, SyncError> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let client = BlockingClient::new(base);
        let prompter = GuiPrompter::default();
        let ctx = SyncCtx { client: &client, prompter: &prompter, page_size: PAGE_SIZE };
        let out = f(&ctx).map_err(|e| e.message().to_string())?;
        let warnings = prompter.warnings.into_inner().unwrap_or_default();
        Ok::<_, String>((out, warnings))
    })
    .await
    .map_err(|e| format!("sync task failed: {e}"))?
}

// ── Report → JSON ───────────────────────────────────────────────────────────

fn plan_json(report: PlanReport, warnings: Vec<String>) -> Value {
    json!({
        "plan_uuid": report.plan_uuid.as_simple().to_string(),
        "operations": report.operations,
        "warnings": warnings,
    })
}

fn run_json(report: RunReport, warnings: Vec<String>) -> Value {
    let status = match report.status {
        RunStatus::NothingToRun => "nothing_to_run",
        RunStatus::Aborted => "aborted",
        RunStatus::Ran => "ran",
    };
    let divergences: Vec<Value> = core_sync::run::aggregate_divergences(&report.divergences)
        .into_iter()
        .map(|(subtree, count)| json!({ "subtree": subtree, "count": count }))
        .collect();
    json!({
        "status": status,
        "done": report.done,
        "skipped": report.skipped,
        "divergences": divergences,
        "warnings": warnings,
    })
}

fn show_op_json(op: &ShowOp) -> Value {
    json!({ "green": op.green, "kind": op.kind, "context": op.context, "why": op.why })
}

fn show_json(report: ShowReport) -> Value {
    match report {
        ShowReport::NoPlan => json!({ "state": "no_plan" }),
        ShowReport::Empty => json!({ "state": "empty" }),
        ShowReport::Filtered(ops) => json!({
            "state": "filtered",
            "ops": ops.iter().map(show_op_json).collect::<Vec<_>>(),
        }),
        ShowReport::Summary { total, counts, reds } => json!({
            "state": "summary",
            "total": total,
            "counts": counts.into_iter().map(|(kind, count)| json!({ "kind": kind, "count": count })).collect::<Vec<_>>(),
            "reds": reds.iter().map(show_op_json).collect::<Vec<_>>(),
        }),
    }
}

// ── Tauri commands ──────────────────────────────────────────────────────────

type AppHandle<'a> = tauri::State<'a, Arc<App>>;

/// `metafolder.sync.status(a, b)` — the raw `/status` body (links + states).
#[tauri::command]
pub async fn sync_status(app: AppHandle<'_>, repo_a: String, repo_b: String) -> Result<Value, String> {
    let base = app.daemon.base_url();
    let (body, _) = blocking(base, move |ctx| core_sync::status(ctx, &repo_a, &repo_b)).await?;
    Ok(body)
}

/// `metafolder.sync.link(a, b, uuidA, uuidB, host?)` — returns `{ uuid }`.
#[tauri::command]
pub async fn sync_link(
    app: AppHandle<'_>,
    repo_a: String,
    repo_b: String,
    uuid_a: String,
    uuid_b: String,
    host: Option<String>,
) -> Result<Value, String> {
    let base = app.daemon.base_url();
    let (uuid, _) = blocking(base, move |ctx| {
        core_sync::link(ctx, &repo_a, &repo_b, &uuid_a, &uuid_b, host.as_deref())
    })
    .await?;
    Ok(json!({ "uuid": uuid.as_simple().to_string() }))
}

/// `metafolder.sync.unlink(a, b, link, withEndpoint?)` — returns `{ uuid }`.
#[tauri::command]
pub async fn sync_unlink(
    app: AppHandle<'_>,
    repo_a: String,
    repo_b: String,
    link: String,
    with_endpoint: Option<String>,
) -> Result<Value, String> {
    let base = app.daemon.base_url();
    let (uuid, _) = blocking(base, move |ctx| {
        core_sync::unlink(ctx, &repo_a, &repo_b, &link, with_endpoint.as_deref())
    })
    .await?;
    Ok(json!({ "uuid": uuid.as_simple().to_string() }))
}

/// `metafolder.sync.plan(a, b, intentsPath, host?, onConflict?)` — recompute the
/// plan; returns `{ plan_uuid, operations, warnings }`.
#[tauri::command]
pub async fn sync_plan(
    app: AppHandle<'_>,
    repo_a: String,
    repo_b: String,
    intents_path: String,
    host: Option<String>,
    on_conflict: Option<String>,
) -> Result<Value, String> {
    let base = app.daemon.base_url();
    let path = PathBuf::from(intents_path);
    let (report, warnings) = blocking(base, move |ctx| {
        core_sync::plan::run(ctx, &repo_a, &repo_b, &path, host.as_deref(), on_conflict.as_deref())
    })
    .await?;
    Ok(plan_json(report, warnings))
}

/// `metafolder.sync.run(a, b)` — execute the plan (always confirmed); returns
/// `{ status, done, skipped, divergences, warnings }`.
#[tauri::command]
pub async fn sync_run(app: AppHandle<'_>, repo_a: String, repo_b: String) -> Result<Value, String> {
    let base = app.daemon.base_url();
    let (report, warnings) =
        blocking(base, move |ctx| core_sync::run::run(ctx, &repo_a, &repo_b, true)).await?;
    Ok(run_json(report, warnings))
}

/// `metafolder.sync.show(a, b, conflicts, files)` — the live red/green overlay.
#[tauri::command]
pub async fn sync_show(
    app: AppHandle<'_>,
    repo_a: String,
    repo_b: String,
    conflicts: bool,
    files: bool,
) -> Result<Value, String> {
    let base = app.daemon.base_url();
    let (report, _) =
        blocking(base, move |ctx| core_sync::run::show(ctx, &repo_a, &repo_b, conflicts, files)).await?;
    Ok(show_json(report))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_json_shape() {
        let uuid = Uuid::from_bytes([0xab; 16]);
        let v = plan_json(PlanReport { plan_uuid: uuid, operations: 3 }, vec!["w".into()]);
        assert_eq!(v["plan_uuid"], uuid.as_simple().to_string());
        assert_eq!(v["operations"], 3);
        assert_eq!(v["warnings"][0], "w");
    }

    #[test]
    fn run_json_maps_status_and_aggregates_divergences() {
        let report = RunReport {
            status: RunStatus::Ran,
            done: 2,
            skipped: 1,
            divergences: vec!["/photos/a".into(), "/photos/b".into(), "/docs/c".into()],
        };
        let v = run_json(report, vec![]);
        assert_eq!(v["status"], "ran");
        assert_eq!(v["done"], 2);
        assert_eq!(v["skipped"], 1);
        // Aggregated by subtree, never one line per file.
        let div = v["divergences"].as_array().unwrap();
        assert_eq!(div.len(), 2);
        assert_eq!(div[0], json!({ "subtree": "/docs", "count": 1 }));
        assert_eq!(div[1], json!({ "subtree": "/photos", "count": 2 }));
    }

    #[test]
    fn run_json_status_variants() {
        for (status, expected) in [
            (RunStatus::NothingToRun, "nothing_to_run"),
            (RunStatus::Aborted, "aborted"),
        ] {
            let v = run_json(RunReport { status, done: 0, skipped: 0, divergences: vec![] }, vec![]);
            assert_eq!(v["status"], expected);
        }
    }

    #[test]
    fn show_json_variants() {
        assert_eq!(show_json(ShowReport::NoPlan)["state"], "no_plan");
        assert_eq!(show_json(ShowReport::Empty)["state"], "empty");

        let filtered = show_json(ShowReport::Filtered(vec![ShowOp {
            green: true,
            kind: "copy".into(),
            context: "/x".into(),
            why: None,
        }]));
        assert_eq!(filtered["state"], "filtered");
        assert_eq!(filtered["ops"][0]["kind"], "copy");
        assert_eq!(filtered["ops"][0]["green"], true);

        let summary = show_json(ShowReport::Summary {
            total: 5,
            counts: vec![("sync".into(), 4), ("copy".into(), 1)],
            reds: vec![ShowOp { green: false, kind: "sync".into(), context: "/y".into(), why: Some("changed".into()) }],
        });
        assert_eq!(summary["state"], "summary");
        assert_eq!(summary["total"], 5);
        assert_eq!(summary["counts"][0], json!({ "kind": "sync", "count": 4 }));
        assert_eq!(summary["reds"][0]["why"], "changed");
    }
}
