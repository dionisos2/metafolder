//! The API sweep (doc "The timed regression suite"): one scenario per route of
//! the daemon's router, so a route that became slow — or that grows with the
//! repository when it should not — shows up as a number.
//!
//! It runs on a repository of its own, generated afresh on every run with its
//! files on disk ([`super::synth::write_files`]): the sweep writes, trashes,
//! rolls back, prunes and restores, and the repository the other scenarios
//! reuse must not carry that from one run to the next. Each scenario that
//! changes something puts it back, or changes what the next run changes too —
//! so its five timed repetitions measure the same work.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{json, Value};
use uuid::Uuid;

use super::{client, next_name};

/// One scenario of the sweep: what it measures, which routes it exercises, and
/// whether its cost is *expected* to grow with the repository (a whole-repo
/// check, a reconcile): the others must cost about the same on `M` as on `S`.
pub struct Scenario {
    pub id: &'static str,
    /// Read by the coverage test only.
    #[cfg_attr(not(test), allow(dead_code))]
    pub covers: &'static [(&'static str, &'static str)],
    pub grows: bool,
}

const fn s(id: &'static str, covers: &'static [(&'static str, &'static str)]) -> Scenario {
    Scenario { id, covers, grows: false }
}

/// A scenario whose cost follows the repository's size by nature.
const fn whole(id: &'static str, covers: &'static [(&'static str, &'static str)]) -> Scenario {
    Scenario { id, covers, grows: true }
}

const G: &str = "GET";
const P: &str = "POST";

/// In the order they run: reads first, then writes that put things back, the
/// background tasks, the whole-repository operations, and last what replaces
/// the store or the repository.
pub const SCENARIOS: &[Scenario] = &[
    // ── The daemon ─────────────────────────────────────────────────────────
    s("api.health", &[(G, "/health")]),
    s("api.diagnostics", &[(G, "/diagnostics")]),
    s("api.tasks", &[(G, "/tasks")]),
    s("api.repos", &[(G, "/repos")]),
    s("api.repo.get", &[(G, "/repos/:repo")]),
    // ── Reads ──────────────────────────────────────────────────────────────
    s("api.metarecord.get", &[(G, "/repos/:repo/metarecords/:uuid")]),
    s("api.metarecord.field_get", &[(G, "/repos/:repo/metarecords/:uuid/fields/:name")]),
    s(
        "api.metarecord.field_resolve",
        &[(G, "/repos/:repo/metarecords/:uuid/fields/:name/resolve-tree")],
    ),
    s("api.metarecord.mf_sync", &[(G, "/repos/:repo/metarecords/:uuid/mf-sync")]),
    s("api.field.get", &[(G, "/repos/:repo/fields/:id")]),
    s("api.fields", &[(G, "/repos/:repo/fields")]),
    s("api.tree.roots", &[(G, "/repos/:repo/tree/roots")]),
    s("api.tree.children", &[(G, "/repos/:repo/tree/children")]),
    s("api.tree.resolve_path", &[(P, "/repos/:repo/tree/resolve-path")]),
    s("api.query.page", &[(P, "/repos/:repo/query")]),
    s("api.query.profile", &[(P, "/repos/:repo/query/profile")]),
    s("api.query.resolve_tree", &[(P, "/repos/:repo/query/fields/resolve-tree")]),
    s("api.log.window", &[(G, "/repos/:repo/log")]),
    s("api.log.since", &[(G, "/repos/:repo/log/since")]),
    s("api.log.revision", &[(G, "/repos/:repo/log/revisions/:rev_id")]),
    s("api.rollback.plan", &[(G, "/repos/:repo/rollback/plan")]),
    s("api.rollback.plan_summary", &[(G, "/repos/:repo/rollback/plan/summary")]),
    s("api.revert.plan", &[(G, "/repos/:repo/revert/plan")]),
    s("api.schema.get", &[(G, "/repos/:repo/schema")]),
    s("api.schema.check", &[(P, "/repos/:repo/schema/check")]),
    s("api.repo.tasks", &[(G, "/repos/:repo/tasks")]),
    s("api.mounts", &[(G, "/repos/:repo/mounts")]),
    s("api.watch.status", &[(G, "/repos/:repo/watch")]),
    s("api.watch.exceeded_list", &[(G, "/repos/:repo/watch/exceeded")]),
    s("api.watch.check", &[(P, "/repos/:repo/watch/check")]),
    s("api.watch.activity_children", &[(G, "/repos/:repo/watch/activity")]),
    s("api.watch.activity_of", &[(P, "/repos/:repo/watch/activity")]),
    s("api.slow", &[(G, "/repos/:repo/slow")]),
    s("api.eligibility", &[(P, "/repos/:repo/eligibility")]),
    s("api.ignore.effective", &[(G, "/repos/:repo/ignore/effective")]),
    s("api.sync.links", &[(G, "/sync/:a/:b/links")]),
    s("api.sync.status", &[(G, "/sync/:a/:b/status")]),
    // ── Writes ─────────────────────────────────────────────────────────────
    s("api.metarecord.create", &[(P, "/repos/:repo/metarecords")]),
    s("api.metarecord.bulk_create", &[(P, "/repos/:repo/metarecords/bulk")]),
    s("api.metarecord.put", &[("PUT", "/repos/:repo/metarecords/:uuid")]),
    s("api.metarecord.delete", &[("DELETE", "/repos/:repo/metarecords/:uuid")]),
    s("api.metarecord.trash", &[(P, "/repos/:repo/metarecords/trash")]),
    s("api.metarecord.field_append", &[(P, "/repos/:repo/metarecords/:uuid/fields")]),
    s("api.metarecord.field_set", &[("PUT", "/repos/:repo/metarecords/:uuid/fields/:name")]),
    s("api.metarecord.field_unset", &[("DELETE", "/repos/:repo/metarecords/:uuid/fields/:name")]),
    s("api.metarecord.refresh", &[(P, "/repos/:repo/metarecords/:uuid/refresh")]),
    s("api.field.patch", &[("PATCH", "/repos/:repo/fields/:id")]),
    s("api.field.delete", &[("DELETE", "/repos/:repo/fields/:id")]),
    s("api.retype", &[(P, "/repos/:repo/retype")]),
    s("api.query.set", &[(P, "/repos/:repo/query/fields/set")]),
    s("api.query.add", &[(P, "/repos/:repo/query/fields/add")]),
    s("api.query.remove", &[(P, "/repos/:repo/query/fields/remove")]),
    s("api.query.unset", &[(P, "/repos/:repo/query/fields/unset")]),
    s("api.query.batch", &[(P, "/repos/:repo/query/fields/batch")]),
    s("api.query.delete", &[(P, "/repos/:repo/query/delete")]),
    s("api.log.label", &[("PATCH", "/repos/:repo/log/revisions/:rev_id")]),
    s("api.revert", &[(P, "/repos/:repo/revert")]),
    s("api.revert.session", &[(P, "/repos/:repo/revert/start"), (P, "/repos/:repo/revert/commit")]),
    s("api.revert.abort", &[(P, "/repos/:repo/revert/abort")]),
    s("api.rollback", &[(P, "/repos/:repo/rollback")]),
    s(
        "api.rollback.session",
        &[
            (P, "/repos/:repo/rollback/start"),
            (P, "/repos/:repo/rollback/step"),
            (P, "/repos/:repo/rollback/abort"),
        ],
    ),
    s("api.schema.reload", &[(P, "/repos/:repo/schema/reload")]),
    s(
        "api.watch.pause_resume",
        &[(P, "/repos/:repo/watch/pause"), (P, "/repos/:repo/watch/resume")],
    ),
    // A rule changed: the watch set is recomputed, a walk of the watched
    // folders (doc "Watch and ignore fields").
    whole("api.watch.exceeded_set", &[(P, "/repos/:repo/watch/exceeded")]),
    s("api.watch.activity_reset", &[(P, "/repos/:repo/watch/activity/reset")]),
    s("api.track", &[(P, "/repos/:repo/track")]),
    s("api.slow.clear", &[("DELETE", "/repos/:repo/slow")]),
    s("api.repo.rename", &[("PATCH", "/repos/:repo")]),
    s(
        "api.sync.link",
        &[
            (P, "/sync/:a/:b/links"),
            (G, "/sync/:a/:b/links/:link"),
            ("DELETE", "/sync/:a/:b/links/:link"),
        ],
    ),
    s("api.sync.commit", &[(P, "/sync/:a/:b/links/commit")]),
    // ── Background tasks (started, then waited for) ───────────────────────
    whole("api.task.reconcile", &[(P, "/repos/:repo/reconcile"), (G, "/repos/:repo/tasks/:task")]),
    s("api.task.cancel", &[(P, "/repos/:repo/tasks/:task/cancel")]),
    whole("api.task.duplicates", &[(P, "/repos/:repo/duplicates/scan")]),
    whole("api.task.relink", &[(P, "/repos/:repo/orphans/relink")]),
    // ── The whole repository ───────────────────────────────────────────────
    whole("api.orphans.scan", &[(P, "/repos/:repo/orphans/scan")]),
    whole("api.orphans.mark", &[(P, "/repos/:repo/orphans/mark")]),
    s("api.orphans.clear", &[(P, "/repos/:repo/orphans/clear")]),
    whole("api.repo.check", &[(P, "/repos/:repo/check")]),
    whole("api.repo.reindex", &[(P, "/repos/:repo/reindex")]),
    whole("api.repo.backup", &[(P, "/repos/:repo/backup")]),
    whole("api.repo.restore", &[(P, "/repos/:repo/restore")]),
    whole("api.repo.restore_by_root", &[(P, "/repos/restore")]),
    s("api.repo.reload", &[(P, "/repos/:repo/unload"), (P, "/repos/load")]),
    s("api.repo.init", &[(P, "/repos/init")]),
    // Reads the whole log to find what lies outside the target's subtree.
    whole("api.log.prune", &[(P, "/repos/:repo/log/prune")]),
];

/// Routes the sweep does not measure, and why (read by the coverage test).
#[cfg_attr(not(test), allow(dead_code))]
pub const NOT_MEASURED: &[(&str, &str, &str)] = &[];

/// Files whose metarecord the generated repository keeps but whose file it
/// does not write: the orphans the `orphans.*` scenarios find.
pub const ORPHANS: usize = 10;

/// What the scenarios address, read from the repository once it is loaded.
pub struct Ctx {
    pub url: String,
    pub repo: Uuid,
    /// The repository's root on disk.
    pub dir: PathBuf,
    /// A file's metarecord, its file on disk.
    pub sample: String,
    /// A page of file metarecords, and their paths.
    pub page: Vec<String>,
    pub paths: Vec<String>,
    /// The first directory of the generated tree.
    pub dir0: String,
    /// The sample's `rating` row.
    pub rating_row: i64,
    /// A revision, and an operation twenty back from HEAD: what the plans
    /// look back to, and what the prune keeps from (after the warm-up, there
    /// is nothing left before it to prune).
    pub rev_id: i64,
    pub older_op: i64,
    /// A second repository, loaded, for the pair the sync routes take, and a
    /// link between the two kept for the commit.
    pub peer: Uuid,
    pub link: String,
    /// The orphans' uuids.
    pub orphans: Vec<String>,
    /// Where the backup the restores read is written.
    pub backup: PathBuf,
}

async fn send(method: &str, url: &str, body: Option<&Value>) -> Result<Value> {
    let m = reqwest::Method::from_bytes(method.as_bytes())?;
    let mut req = client().request(m, url);
    if let Some(body) = body {
        req = req.json(body);
    }
    let response = req.send().await?;
    let status = response.status();
    let bytes = response.bytes().await?;
    if !status.is_success() {
        anyhow::bail!("{method} {url}: {status}: {}", String::from_utf8_lossy(&bytes));
    }
    Ok(serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn get(url: &str) -> Result<Value> {
    send("GET", url, None).await
}

async fn post(url: &str, body: Value) -> Result<Value> {
    send("POST", url, Some(&body)).await
}

fn uuid_in(uuids: &[&str]) -> Value {
    json!({"type": "uuid_in", "uuids": uuids})
}

fn int(n: i64) -> Value {
    json!({"type": "int", "value": n})
}

fn string(v: &str) -> Value {
    json!({"type": "string", "value": v})
}

/// A metarecord of its own, for a scenario that destroys or overwrites one.
async fn scratch(base: &str) -> Result<String> {
    let v = post(
        &format!("{base}/metarecords"),
        json!({"fields": [{"name": "bench_scratch", "value": int(1)}]}),
    )
    .await?;
    Ok(v["uuid"].as_str().context("a created metarecord has a uuid")?.to_string())
}

/// HEAD's operation id.
async fn head(base: &str) -> Result<i64> {
    let v = get(&format!("{base}/log?mode=active&limit=1")).await?;
    v["head"].as_i64().context("the log has a head")
}

/// Waits for a background task to end, and returns its final state.
async fn wait_task(base: &str, task: &str) -> Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(900);
    loop {
        let t = get(&format!("{base}/tasks/{task}")).await?;
        match t["status"].as_str() {
            Some("done" | "cancelled") => return Ok(t),
            Some("failed") => anyhow::bail!("task {task} failed: {t}"),
            _ if Instant::now() > deadline => anyhow::bail!("task {task} still running"),
            _ => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
}

/// Starts a background task and waits for it.
async fn task(base: &str, route: &str, body: Value) -> Result<Value> {
    let started = post(&format!("{base}/{route}"), body).await?;
    let id = started["task_id"].as_str().context("a task id")?.to_string();
    wait_task(base, &id).await
}

pub async fn run(id: &str, c: &Ctx) -> Result<()> {
    let url = &c.url;
    let base = format!("{url}/repos/{}", c.repo);
    let base = base.as_str();
    let pair = format!("{url}/sync/{}/{}", c.repo, c.peer);
    let sample = c.sample.as_str();
    match id {
        "api.health" => drop(get(&format!("{url}/health")).await?),
        "api.diagnostics" => drop(get(&format!("{url}/diagnostics")).await?),
        "api.tasks" => drop(get(&format!("{url}/tasks")).await?),
        "api.repos" => drop(get(&format!("{url}/repos")).await?),
        "api.repo.get" => drop(get(base).await?),

        "api.metarecord.get" => drop(get(&format!("{base}/metarecords/{sample}")).await?),
        "api.metarecord.field_get" => {
            get(&format!("{base}/metarecords/{sample}/fields/rating")).await?;
        }
        "api.metarecord.field_resolve" => {
            get(&format!("{base}/metarecords/{sample}/fields/mfr_path/resolve-tree")).await?;
        }
        "api.metarecord.mf_sync" => {
            get(&format!("{base}/metarecords/{sample}/mf-sync")).await?;
        }
        "api.field.get" => drop(get(&format!("{base}/fields/{}", c.rating_row)).await?),
        "api.fields" => drop(get(&format!("{base}/fields")).await?),
        "api.tree.roots" => drop(get(&format!("{base}/tree/roots")).await?),
        "api.tree.children" => {
            get(&format!("{base}/tree/children?uuid={}", c.dir0)).await?;
        }
        "api.tree.resolve_path" => {
            post(&format!("{base}/tree/resolve-path"), json!({"path": "/dir0/dir1"})).await?;
        }
        "api.query.page" => {
            let q = json!({"type": "eq", "field": "kind", "value": string("photo")});
            post(&format!("{base}/query"), json!({"query": q, "limit": 100, "count": true}))
                .await?;
        }
        "api.query.profile" => {
            let q = json!({"type": "eq", "field": "kind", "value": string("photo")});
            post(&format!("{base}/query/profile"), json!({"query": q, "limit": 100})).await?;
        }
        "api.query.resolve_tree" => {
            let page: Vec<&str> = c.page.iter().map(String::as_str).collect();
            post(
                &format!("{base}/query/fields/resolve-tree"),
                json!({"query": uuid_in(&page), "field": "mfr_path"}),
            )
            .await?;
        }
        "api.log.window" => drop(get(&format!("{base}/log?mode=active&limit=50")).await?),
        "api.log.since" => {
            let since = head(base).await? - 20;
            get(&format!("{base}/log/since?op={since}")).await?;
        }
        "api.log.revision" => {
            get(&format!("{base}/log/revisions/{}?limit=50", c.rev_id)).await?;
        }
        "api.rollback.plan" => {
            get(&format!("{base}/rollback/plan?target_id={}", c.older_op)).await?;
        }
        "api.rollback.plan_summary" => {
            get(&format!("{base}/rollback/plan/summary?target_id={}", c.older_op)).await?;
        }
        "api.revert.plan" => {
            get(&format!("{base}/revert/plan?target_op_ids={}", c.older_op)).await?;
        }
        "api.schema.get" => drop(get(&format!("{base}/schema")).await?),
        "api.schema.check" => {
            post(&format!("{base}/schema/check"), json!({"limit": 20})).await?;
        }
        "api.repo.tasks" => drop(get(&format!("{base}/tasks")).await?),
        "api.mounts" => drop(get(&format!("{base}/mounts")).await?),
        "api.watch.status" => drop(get(&format!("{base}/watch")).await?),
        "api.watch.exceeded_list" => drop(get(&format!("{base}/watch/exceeded")).await?),
        "api.watch.check" => {
            post(&format!("{base}/watch/check"), json!({"paths": c.paths})).await?;
        }
        "api.watch.activity_children" => {
            get(&format!("{base}/watch/activity?path=/dir0")).await?;
        }
        "api.watch.activity_of" => {
            post(&format!("{base}/watch/activity"), json!({"paths": c.paths})).await?;
        }
        "api.slow" => drop(get(&format!("{base}/slow")).await?),
        "api.eligibility" => {
            post(&format!("{base}/eligibility"), json!({"paths": c.paths})).await?;
        }
        "api.ignore.effective" => {
            get(&format!("{base}/ignore/effective?path=/dir0")).await?;
        }
        "api.sync.links" => drop(get(&format!("{pair}/links")).await?),
        "api.sync.status" => drop(get(&format!("{pair}/status")).await?),

        "api.metarecord.create" => drop(scratch(base).await?),
        "api.metarecord.bulk_create" => {
            let records: Vec<Value> = (0..100)
                .map(|i| json!({"fields": [{"name": "bench_bulk", "value": int(i)}]}))
                .collect();
            post(&format!("{base}/metarecords/bulk"), json!({"metarecords": records})).await?;
        }
        "api.metarecord.put" => {
            let u = scratch(base).await?;
            send(
                "PUT",
                &format!("{base}/metarecords/{u}"),
                Some(&json!({"fields": [{"name": "bench_put", "value": int(2)}], "force": true})),
            )
            .await?;
        }
        "api.metarecord.delete" => {
            let u = scratch(base).await?;
            send("DELETE", &format!("{base}/metarecords/{u}"), None).await?;
        }
        "api.metarecord.trash" => {
            let u = scratch(base).await?;
            post(&format!("{base}/metarecords/trash"), json!({"uuids": [u]})).await?;
        }
        "api.metarecord.field_append" => {
            post(
                &format!("{base}/metarecords/{sample}/fields"),
                json!({"name": "bench_append", "value": int(1)}),
            )
            .await?;
        }
        "api.metarecord.field_set" => {
            send(
                "PUT",
                &format!("{base}/metarecords/{sample}/fields/bench_set"),
                Some(&json!({"value": int(next_seq())})),
            )
            .await?;
        }
        "api.metarecord.field_unset" => {
            let field = format!("{base}/metarecords/{sample}/fields/bench_unset");
            send("PUT", &field, Some(&json!({"value": int(1)}))).await?;
            send("DELETE", &field, None).await?;
        }
        "api.metarecord.refresh" => {
            post(&format!("{base}/metarecords/{sample}/refresh"), json!({})).await?;
        }
        "api.field.patch" => {
            send(
                "PATCH",
                &format!("{base}/fields/{}", c.rating_row),
                Some(&json!({"value": int(next_seq() % 10)})),
            )
            .await?;
        }
        "api.field.delete" => {
            let u = scratch(base).await?;
            let record = get(&format!("{base}/metarecords/{u}")).await?;
            let row = record["fields"][0]["id"].as_i64().context("a field row id")?;
            send("DELETE", &format!("{base}/fields/{row}"), None).await?;
        }
        "api.retype" => {
            // One row, its type turned back and forth.
            let to = if next_seq() % 2 == 0 { "string" } else { "int" };
            post(&format!("{base}/retype"), json!({"name": "bench_retype", "to": to})).await?;
        }
        "api.query.set" | "api.query.add" | "api.query.remove" | "api.query.unset" => {
            let page: Vec<&str> = c.page.iter().map(String::as_str).collect();
            let verb = id.trim_start_matches("api.query.");
            let mut body = json!({"query": uuid_in(&page), "name": "bench_set_layer"});
            if verb != "unset" {
                body["value"] = int(1);
            }
            post(&format!("{base}/query/fields/{verb}"), body).await?;
        }
        "api.query.batch" => {
            let page: Vec<&str> = c.page.iter().map(String::as_str).collect();
            post(
                &format!("{base}/query/fields/batch"),
                json!({"ops": [
                    {"op": "set", "query": uuid_in(&page), "name": "bench_batch", "value": int(1)},
                    {"op": "create", "fields": [{"name": "bench_batch", "value": int(2)}]},
                ]}),
            )
            .await?;
        }
        "api.query.delete" => {
            let u = scratch(base).await?;
            post(&format!("{base}/query/delete"), json!({"query": uuid_in(&[&u])})).await?;
        }
        "api.log.label" => {
            send(
                "PATCH",
                &format!("{base}/log/revisions/{}", c.rev_id),
                Some(&json!({"label": next_name("bench-label-")})),
            )
            .await?;
        }
        "api.revert" => {
            set_touch(base, sample).await?;
            let op = head(base).await?;
            post(&format!("{base}/revert"), json!({"target": {"op_ids": [op]}})).await?;
        }
        "api.revert.session" => {
            set_touch(base, sample).await?;
            let op = head(base).await?;
            post(&format!("{base}/revert/start"), json!({"target": {"op_ids": [op]}})).await?;
            post(&format!("{base}/revert/commit"), json!({})).await?;
        }
        "api.revert.abort" => {
            set_touch(base, sample).await?;
            let op = head(base).await?;
            post(&format!("{base}/revert/start"), json!({"target": {"op_ids": [op]}})).await?;
            post(&format!("{base}/revert/abort"), json!({})).await?;
        }
        "api.rollback" => {
            let before = head(base).await?;
            set_touch(base, sample).await?;
            post(&format!("{base}/rollback"), json!({"target": {"id": before}})).await?;
        }
        "api.rollback.session" => {
            let before = head(base).await?;
            set_touch(base, sample).await?;
            set_touch(base, sample).await?;
            post(&format!("{base}/rollback/start"), json!({"target": {"id": before}})).await?;
            post(&format!("{base}/rollback/step"), json!({})).await?;
            post(&format!("{base}/rollback/abort"), json!({})).await?;
        }
        "api.schema.reload" => drop(post(&format!("{base}/schema/reload"), json!({})).await?),
        "api.watch.pause_resume" => {
            post(&format!("{base}/watch/pause"), json!({})).await?;
            post(&format!("{base}/watch/resume"), json!({})).await?;
        }
        "api.watch.exceeded_set" => {
            let exceeded = next_seq() % 2 == 0;
            post(
                &format!("{base}/watch/exceeded"),
                json!({"path": "/dir0/dir1", "exceeded": exceeded}),
            )
            .await?;
        }
        "api.watch.activity_reset" => {
            post(&format!("{base}/watch/activity/reset"), json!({})).await?;
        }
        "api.track" => {
            let file = c.dir.join("dir0").join(next_name("tracked-"));
            std::fs::write(&file, b"tracked")?;
            post(&format!("{base}/track"), json!({"path": file})).await?;
        }
        "api.slow.clear" => drop(send("DELETE", &format!("{base}/slow"), None).await?),
        "api.repo.rename" => {
            send("PATCH", base, Some(&json!({"name": next_name("bench-api-")}))).await?;
        }
        "api.sync.link" => {
            // A record is linked once in a pair: two new ones each run.
            let u = scratch(base).await?;
            let peer = scratch(&format!("{url}/repos/{}", c.peer)).await?;
            let v = post(&format!("{pair}/links"), link_body(c.repo, c.peer, &u, &peer)).await?;
            let link = v["uuid"].as_str().context("a link uuid")?.to_string();
            get(&format!("{pair}/links/{link}")).await?;
            send("DELETE", &format!("{pair}/links/{link}"), None).await?;
        }
        "api.sync.commit" => {
            let status = get(&format!("{pair}/status")).await?;
            let link = status["links"]
                .as_array()
                .and_then(|l| l.iter().find(|l| l["uuid"] == c.link.as_str()))
                .context("the kept link")?;
            post(
                &format!("{pair}/links/commit"),
                json!({"commits": [{
                    "link": c.link,
                    "version_a": link["e_a_version"],
                    "version_b": link["e_b_version"],
                }]}),
            )
            .await?;
        }

        "api.task.reconcile" => drop(task(base, "reconcile", json!({})).await?),
        "api.task.cancel" => {
            let started = post(&format!("{base}/reconcile"), json!({})).await?;
            let task_id = started["task_id"].as_str().context("a task id")?.to_string();
            post(&format!("{base}/tasks/{task_id}/cancel"), json!({})).await?;
            wait_task(base, &task_id).await?;
        }
        "api.task.duplicates" => drop(task(base, "duplicates/scan", json!({})).await?),
        "api.task.relink" => drop(task(base, "orphans/relink", json!({})).await?),

        "api.orphans.scan" => drop(post(&format!("{base}/orphans/scan"), json!({})).await?),
        "api.orphans.mark" => drop(post(&format!("{base}/orphans/mark"), json!({})).await?),
        "api.orphans.clear" => {
            post(&format!("{base}/orphans/clear"), json!({"uuids": c.orphans})).await?;
        }
        "api.repo.check" => drop(post(&format!("{base}/check"), json!({})).await?),
        "api.repo.reindex" => drop(post(&format!("{base}/reindex"), json!({})).await?),
        "api.repo.backup" => {
            let _ = std::fs::remove_dir_all(&c.backup);
            post(&format!("{base}/backup"), json!({"to": c.backup})).await?;
        }
        "api.repo.restore" => {
            post(&format!("{base}/restore"), json!({"from": c.backup})).await?;
            wait_serving(base).await?;
        }
        "api.repo.restore_by_root" => {
            post(&format!("{url}/repos/restore"), json!({"root": c.dir, "from": c.backup})).await?;
            wait_serving(base).await?;
        }
        "api.repo.reload" => {
            post(&format!("{base}/unload"), json!({})).await?;
            post(&format!("{url}/repos/load"), json!({"root": c.dir})).await?;
            wait_serving(base).await?;
        }
        "api.repo.init" => {
            let dir = c.dir.with_file_name(next_name("bench-init-"));
            std::fs::create_dir_all(&dir)?;
            let v = post(&format!("{url}/repos/init"), json!({"root": dir})).await?;
            let repo = v["repo_uuid"].as_str().context("a repo uuid")?;
            post(&format!("{url}/repos/{repo}/unload"), json!({})).await?;
            std::fs::remove_dir_all(&dir)?;
        }
        "api.log.prune" => {
            post(
                &format!("{base}/log/prune"),
                json!({"mode": "before", "target": {"id": c.older_op}}),
            )
            .await?;
        }
        other => anyhow::bail!("unknown scenario '{other}'"),
    }
    Ok(())
}

/// The body creating a link between `mine` (in `repo`) and `theirs` (in
/// `peer`): a pair is canonical, its `a` the smaller uuid
/// (`sync::canonical_pair`), whichever order the route names them in.
fn link_body(repo: Uuid, peer: Uuid, mine: &str, theirs: &str) -> Value {
    let (a, b) = if repo.as_bytes() < peer.as_bytes() { (mine, theirs) } else { (theirs, mine) };
    json!({"record_a": a, "record_b": b})
}

fn next_seq() -> i64 {
    static N: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
    N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// One field write on `uuid`, a different value each time: the change a
/// revert or a rollback then takes back.
async fn set_touch(base: &str, uuid: &str) -> Result<()> {
    send(
        "PUT",
        &format!("{base}/metarecords/{uuid}/fields/bench_touch"),
        Some(&json!({"value": int(next_seq())})),
    )
    .await?;
    Ok(())
}

/// Waits until the repository answers a query again (a load and a restore end
/// in the background).
async fn wait_serving(base: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(900);
    loop {
        let r = client()
            .post(format!("{base}/query"))
            .json(&json!({"query": {"type": "is_present", "field": "mfr_path"}, "limit": 1}))
            .send()
            .await?;
        if r.status() != reqwest::StatusCode::SERVICE_UNAVAILABLE
            && r.status() != reqwest::StatusCode::NOT_FOUND
        {
            r.error_for_status()?;
            return Ok(());
        }
        if Instant::now() > deadline {
            anyhow::bail!("{base} is not serving after 15 minutes");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The schema the sweep's repository carries, so `schema.check` has
/// constraints to check: a cardinality and a type on fields every file holds,
/// and a required field on a type no record declares.
const SCHEMA: &str = r#"{"version": 1, "groups": [
  {"targets": "*", "constraints": [
    {"field": "rating", "type": "int", "max": 1},
    {"field": "kind", "type": "string", "max": 1}
  ]},
  {"targets": ["film"], "constraints": [{"field": "title", "type": "string", "min": 1}]}
]}"#;

/// Loads the sweep's repository at `dir` (generated, its files written) and a
/// peer for the sync routes at `peer_dir`, and reads what the scenarios address.
pub async fn prepare(url: &str, dir: &Path, peer_dir: &Path) -> Result<Ctx> {
    std::fs::write(dir.join(".metafolder").join("schema.json"), SCHEMA)?;
    let repo = super::load_and_wait(url, dir).await?;
    let base = format!("{url}/repos/{repo}");
    let base = base.as_str();
    crate::api_enable_watch(url, repo).await?;

    let files = json!({"type": "eq", "field": "mfr_type", "value": string("file")});
    let v = post(&format!("{base}/query"), json!({"query": files, "limit": 100})).await?;
    let page: Vec<String> = v["results"]
        .as_array()
        .context("query results")?
        .iter()
        .filter_map(|u| u.as_str().map(str::to_string))
        .collect();
    let sample = page.first().context("a file metarecord")?.clone();
    let refs: Vec<&str> = page.iter().map(String::as_str).collect();
    let resolved = post(
        &format!("{base}/query/fields/resolve-tree"),
        json!({"query": uuid_in(&refs), "field": "mfr_path"}),
    )
    .await?;
    let paths: Vec<String> = page
        .iter()
        .filter_map(|u| resolved[u].as_array()?.first()?.as_str().map(str::to_string))
        .collect();
    anyhow::ensure!(paths.len() == page.len(), "every file of the page has a path");
    let dir0 = post(&format!("{base}/tree/resolve-path"), json!({"path": "/dir0"})).await?;
    let dir0 = dir0["uuid"].as_str().context("/dir0 resolves")?.to_string();
    let record = get(&format!("{base}/metarecords/{sample}")).await?;
    let rating_row = record["fields"]
        .as_array()
        .and_then(|f| f.iter().find(|f| f["name"] == "rating"))
        .and_then(|f| f["id"].as_i64())
        .context("the sample's rating row")?;
    // The retyped field, one row.
    send(
        "PUT",
        &format!("{base}/metarecords/{sample}/fields/bench_retype"),
        Some(&json!({"value": int(1)})),
    )
    .await?;
    let log = get(&format!("{base}/log?mode=active&limit=21")).await?;
    let ops = log["operations"].as_array().context("log operations")?;
    let rev_id = ops.first().and_then(|o| o["rev_id"].as_i64()).context("a revision")?;
    let older_op = ops.last().and_then(|o| o["id"].as_i64()).context("an older operation")?;

    // The orphans: the files the generation did not write.
    let v = post(&format!("{base}/orphans/scan"), json!({})).await?;
    let orphans: Vec<String> = v["orphans"]
        .as_array()
        .map(|o| {
            o.iter().filter_map(|e| e["uuid"].as_str().or(e.as_str()).map(str::to_string)).collect()
        })
        .unwrap_or_default();

    // The peer, and a link between the two kept for the commit.
    if peer_dir.exists() {
        std::fs::remove_dir_all(peer_dir)?;
    }
    std::fs::create_dir_all(peer_dir)?;
    let v = post(&format!("{url}/repos/init"), json!({"root": peer_dir})).await?;
    let peer: Uuid = v["repo_uuid"].as_str().context("a peer uuid")?.parse()?;
    let peer_base = format!("{url}/repos/{peer}");
    let peer_record = scratch(&peer_base).await?;
    let kept = scratch(base).await?;
    let pair = format!("{url}/sync/{repo}/{peer}");
    let v = post(&format!("{pair}/links"), link_body(repo, peer, &kept, &peer_record)).await?;
    let link = v["uuid"].as_str().context("a link uuid")?.to_string();

    Ok(Ctx {
        url: url.to_string(),
        repo,
        dir: dir.to_path_buf(),
        sample,
        page,
        paths,
        dir0,
        rating_row,
        rev_id,
        older_op,
        peer,
        link,
        orphans,
        backup: dir.with_file_name(format!(
            "{}-backup",
            dir.file_name().and_then(|n| n.to_str()).unwrap_or("api")
        )),
    })
}

/// Unloads what [`prepare`] loaded and removes what the sweep wrote beside it.
pub async fn teardown(c: &Ctx, peer_dir: &Path) -> Result<()> {
    for repo in [c.repo, c.peer] {
        let _ = post(&format!("{}/repos/{repo}/unload", c.url), json!({})).await;
    }
    let _ = std::fs::remove_dir_all(peer_dir);
    let _ = std::fs::remove_dir_all(&c.backup);
    let _ = std::fs::remove_dir_all(&c.dir);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The daemon's router, as its own catalog test reads it.
    fn routes() -> Vec<(String, String)> {
        let src = include_str!("../../../daemon/src/routes/mod.rs");
        let start = src.find("pub fn build(").unwrap();
        let end = src.find("pub fn build_authenticated(").unwrap();
        metafolder_core::doc_gen::routes(&src[start..end])
            .into_iter()
            .map(|r| (r.method.to_string(), r.path.to_string()))
            .collect()
    }

    #[test]
    fn every_route_is_measured_or_says_why_not() {
        let covered: Vec<(&str, &str)> = SCENARIOS
            .iter()
            .flat_map(|s| s.covers.iter().copied())
            .chain(NOT_MEASURED.iter().map(|(m, p, _)| (*m, *p)))
            .collect();
        let missing: Vec<String> = routes()
            .iter()
            .filter(|(m, p)| !covered.contains(&(m.as_str(), p.as_str())))
            .map(|(m, p)| format!("{m} {p}"))
            .collect();
        assert!(missing.is_empty(), "routes no scenario measures: {missing:#?}");
    }

    #[test]
    fn every_covered_route_exists() {
        let routes = routes();
        for (m, p) in SCENARIOS.iter().flat_map(|s| s.covers.iter()) {
            assert!(
                routes.iter().any(|(rm, rp)| rm == m && rp == p),
                "{m} {p} is not a route of the daemon"
            );
        }
    }

    #[test]
    fn scenario_ids_are_unique_and_prefixed() {
        let mut ids: Vec<&str> = SCENARIOS.iter().map(|s| s.id).collect();
        assert!(ids.iter().all(|id| id.starts_with("api.")), "{ids:?}");
        ids.sort();
        let n = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), n, "a scenario id twice");
    }
}
