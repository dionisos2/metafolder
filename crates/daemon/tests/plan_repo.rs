//! Phase-3 plumbing for the cross-repo sync **plan repository** (spec-sync
//! "The plan"): a daemon-internal, data-only repository whose op-metarecords
//! describe a pending sync. The daemon delta is thin — a repo can be created as
//! `system` (hidden from `GET /repos`) and holds abstract records (no
//! `mfr_path`) with ordinary `plan_*` user fields, including cross-repo
//! `ExternalRef` values. Orchestration (what to write) is the CLI's job (v2).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use metafolder_daemon::routes;
use metafolder_daemon::state::AppState;
use serde_json::{json, Value};
use tower::util::ServiceExt;
use uuid::Uuid;

mod common;
use common::TempDir;

fn temp_dir(prefix: &str) -> TempDir {
    TempDir::new(&format!("plan_{prefix}"))
}

fn app() -> Router {
    routes::build(std::sync::Arc::new(AppState::new()))
}

async fn request(
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

/// Initialises a system (plan) repository; returns its uuid.
async fn init_system_repo(app: &Router) -> String {
    let root = temp_dir("root");
    let (status, body) = request(
        app,
        "POST",
        "/repos/init",
        Some(json!({"root": root.to_str().unwrap(), "system": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    body["repo_uuid"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn test_system_repo_hidden_unless_all() {
    let app = app();
    let uuid = init_system_repo(&app).await;

    // Hidden from the default listing …
    let (status, list) = request(&app, "GET", "/repos", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        list.as_array().unwrap().iter().all(|r| r["repo_uuid"] != uuid),
        "system repo must be hidden from GET /repos: {list}"
    );

    // … visible with ?all=true, flagged system …
    let (status, all) = request(&app, "GET", "/repos?all=true", None).await;
    assert_eq!(status, StatusCode::OK);
    let found = all
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["repo_uuid"] == uuid)
        .expect("system repo listed with ?all=true");
    assert_eq!(found["system"], true);

    // … and reachable directly like any repo.
    let (status, info) = request(&app, "GET", &format!("/repos/{uuid}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(info["system"], true);
}

#[tokio::test]
async fn test_plan_metarecord_roundtrip() {
    let app = app();
    let plan = init_system_repo(&app).await;
    let repo_a = Uuid::new_v4().as_simple().to_string();
    let record_a = Uuid::new_v4().as_simple().to_string();

    // An abstract op-metarecord: no mfr_path, ordinary `plan_*` user fields,
    // one of them a cross-repo ExternalRef.
    let fields = json!([
        {"name": "plan_kind", "value": {"type": "string", "value": "field_change"}},
        {"name": "plan_version_a", "value": {"type": "int", "value": 7}},
        {"name": "plan_a", "value": {"type": "externalref",
            "value": {"repo": repo_a, "metarecord": record_a}}},
    ]);
    let (status, created) = request(
        &app,
        "POST",
        &format!("/repos/{plan}/metarecords"),
        Some(json!({"fields": fields})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create plan record failed: {created}");
    let uuid = created["uuid"].as_str().unwrap().to_string();

    // Read it back and confirm every field round-trips.
    let (status, got) =
        request(&app, "GET", &format!("/repos/{plan}/metarecords/{uuid}"), None).await;
    assert_eq!(status, StatusCode::OK);
    let field = |name: &str| -> Value {
        got["fields"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["name"] == name)
            .cloned()
            .unwrap_or(Value::Null)
    };
    assert_eq!(field("plan_kind")["value"]["value"], "field_change");
    assert_eq!(field("plan_version_a")["value"]["value"], 7);
    let ext = field("plan_a");
    assert_eq!(ext["value"]["type"], "externalref");
    assert_eq!(ext["value"]["value"]["repo"], repo_a);
    assert_eq!(ext["value"]["value"]["metarecord"], record_a);
    // No mfr_path was written — the record is abstract.
    assert!(field("mfr_path").is_null(), "plan record must have no mfr_path");
}
