//! A repository as a user gets one, driven over HTTP against a live watcher —
//! and the oracle that holds the watcher to a reconcile (doc "Testing the
//! watcher"). Shared by the suites that walk such a repository through a
//! sequence of filesystem changes.

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use metafolder_daemon::routes;
use serde_json::{json, Value};
use tower::util::ServiceExt;

use super::{Regime, TempDir};

pub async fn request(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let builder = Request::builder().method(method).uri(uri);
    let request = match body {
        Some(v) => builder
            .header("content-type", "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

/// The shipped `default` ignore preset, expanded — what `mf repo init` and the
/// GUI's create-repo flow write to a new root (the daemon writes none itself).
pub fn default_ignore_patterns() -> Vec<String> {
    let toml = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../core/default-config/ignore-presets.toml"
    ))
    .expect("the shipped ignore presets");
    let presets = metafolder_core::ignore_presets::Presets::parse(&toml).expect("valid presets");
    presets.expand(&["default"]).expect("the default preset")
}

/// A repository as a user gets one: created on an existing folder, with the
/// default ignore set applied client-side, and tracking turned on.
pub async fn journey_repo(prefix: &str, regime: Regime) -> (Router, String, TempDir) {
    let app = routes::build(std::sync::Arc::new(super::watching_state_on(regime)));
    let root = TempDir::new(&format!("journey_{prefix}"));

    let (status, body) =
        request(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    let repo = body["repo_uuid"].as_str().unwrap().to_string();
    let root_uuid = root_metarecord(&app, &repo).await;

    // The ignore set, then tracking — the order the create-repo flow uses.
    let values: Vec<Value> =
        default_ignore_patterns().iter().map(|p| json!({"type": "string", "value": p})).collect();
    let (status, body) = request(
        &app,
        "PUT",
        &format!("/repos/{repo}/metarecords/{root_uuid}/fields/mf_ignore"),
        Some(json!({"values": values})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "applying the default ignores failed: {body}");

    let (status, body) = request(
        &app,
        "PUT",
        &format!("/repos/{repo}/metarecords/{root_uuid}/fields/mf_watch"),
        Some(json!({"value": {"type": "bool", "value": true}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "enabling mf_watch failed: {body}");

    // The regime asked for is the one running: otherwise both variants would
    // test the same source under two names.
    let (_, watch) = request(&app, "GET", &format!("/repos/{repo}/watch"), None).await;
    assert_eq!(watch["backend"], regime.backend(), "watch source: {watch}");

    (app, repo, root)
}

pub async fn root_metarecord(app: &Router, repo: &str) -> String {
    let (_, roots) = request(
        app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": {"type": "is_present", "field": "mf_watch"}})),
    )
    .await;
    roots[0].as_str().expect("the filesystem root metarecord").to_string()
}

/// Every tracked path, repo-root-relative without the leading slash (the root
/// itself is `""`), sorted.
pub async fn tracked_paths(app: &Router, repo: &str) -> Vec<String> {
    let (status, body) = request(
        app,
        "POST",
        &format!("/repos/{repo}/query/fields/resolve-tree"),
        Some(json!({
            "query": {"type": "is_present", "field": "mfr_path"},
            "field": "mfr_path",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "resolve-tree failed: {body}");
    let mut out: Vec<String> = body
        .as_object()
        .expect("a uuid → paths map")
        .values()
        .filter_map(|paths| paths.as_array())
        .flatten()
        .filter_map(|p| p.as_str())
        .map(|p| p.trim_start_matches('/').to_string())
        .collect();
    out.sort();
    out
}

/// Waits for the watcher to settle on `expected` (its quiet period — 500 ms here,
/// see `common::watching_state` — plus
/// the flush), then reports what it settled on.
pub async fn settle_on(app: &Router, repo: &str, expected: &[&str]) -> Vec<String> {
    let mut want: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
    want.sort();
    let mut last = Vec::new();
    for _ in 0..100 {
        last = tracked_paths(app, repo).await;
        if last == want {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the watcher never settled on {want:?}; last seen {last:?}");
}

/// Runs a full reconcile to its end and returns its result (`created`,
/// `moved`, …).
pub async fn reconcile(app: &Router, repo: &str, label: &str) -> Value {
    let (status, body) = request(app, "POST", &format!("/repos/{repo}/reconcile"), None).await;
    assert_eq!(status, StatusCode::ACCEPTED, "[{label}] reconcile start failed: {body}");
    let task_id = body["task_id"].as_str().unwrap().to_string();
    let mut task = Value::Null;
    for _ in 0..400 {
        let (_, body) = request(app, "GET", &format!("/repos/{repo}/tasks/{task_id}"), None).await;
        if body["status"] == "done" || body["status"] == "failed" {
            task = body;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(task["status"], "done", "[{label}] reconcile did not finish: {task}");
    task["result"].clone()
}

/// The oracle: a full reconcile after a settled watcher must be a no-op.
/// `label` names the step, so a failure says which operation the two disagree
/// about.
pub async fn reconcile_agrees(app: &Router, repo: &str, label: &str) {
    let before = tracked_paths(app, repo).await;
    let result = reconcile(app, repo, label).await;
    assert_eq!(
        result["created"], 0,
        "[{label}] the watcher missed {} file(s) a reconcile had to create — tracked: {before:?}",
        result["created"],
    );
    assert_eq!(
        result["moved"], 0,
        "[{label}] the watcher left {} file(s) at a stale path — tracked: {before:?}",
        result["moved"],
    );

    let after = tracked_paths(app, repo).await;
    assert_eq!(after, before, "[{label}] the reconcile changed the tracked paths");
}

/// Metarecords whose `mfr_path` is `Nothing` — orphans. Deleting a file leaves
/// one behind by design; nothing else may.
pub async fn orphan_count(app: &Router, repo: &str) -> usize {
    let (status, body) = request(
        app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": {"type": "is_absent", "field": "mfr_path"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "orphan query failed: {body}");
    body.as_array().map(|a| a.len()).unwrap_or(0)
}
