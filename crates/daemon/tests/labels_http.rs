//! Labels and contexts over HTTP (doc "Labels and contexts"): creating a
//! label, reading it with its ancestors, writing inside one through the
//! `X-Metafolder-Label` header, and a write with no context opening its own.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use metafolder_daemon::routes;
use metafolder_daemon::state::AppState;
use serde_json::{json, Value};
use tower::util::ServiceExt;

mod common;
use common::TempDir;

async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    label: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(label) = label {
        builder = builder.header("x-metafolder-label", label);
    }
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
    let value =
        if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap() };
    (status, value)
}

async fn setup(prefix: &str) -> (Router, String, TempDir) {
    let app = routes::build(Arc::new(AppState::new()));
    let root = TempDir::new(&format!("labels_{prefix}"));
    let (status, body) =
        send(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()})), None)
            .await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    let repo = body["repo_uuid"].as_str().unwrap().to_string();
    (app, repo, root)
}

async fn new_label(app: &Router, repo: &str, body: Value) -> i64 {
    let (status, label) =
        send(app, "POST", &format!("/repos/{repo}/labels"), Some(body), None).await;
    assert_eq!(status, StatusCode::OK, "label creation failed: {label}");
    label["id"].as_i64().expect("a label id")
}

async fn create(app: &Router, repo: &str, label: Option<&str>) -> (StatusCode, Value) {
    let fields = json!([{"name": "a", "value": {"type": "int", "value": 1}}]);
    send(app, "POST", &format!("/repos/{repo}/metarecords"), Some(json!({"fields": fields})), label)
        .await
}

async fn newest_op(app: &Router, repo: &str) -> Value {
    let (status, log) = send(app, "GET", &format!("/repos/{repo}/log?limit=1"), None, None).await;
    assert_eq!(status, StatusCode::OK, "{log}");
    log["operations"][0].clone()
}

#[tokio::test]
async fn a_label_is_read_back_with_its_ancestors() {
    let (app, repo, _root) = setup("ancestors").await;
    let run = new_label(&app, &repo, json!({"name": "tag run", "description": "all photos"})).await;
    let answer = new_label(&app, &repo, json!({"name": "answer 1", "parent": run})).await;

    let (status, label) =
        send(&app, "GET", &format!("/repos/{repo}/labels/{answer}"), None, None).await;
    assert_eq!(status, StatusCode::OK, "{label}");
    assert_eq!(label["name"], "answer 1");
    assert_eq!(label["parent"], run);
    assert!(label["created"].as_i64().is_some(), "{label}");
    let ancestors = label["ancestors"].as_array().unwrap();
    assert_eq!(ancestors.len(), 1, "{label}");
    assert_eq!(ancestors[0]["id"], run);
    assert_eq!(ancestors[0]["description"], "all photos");

    let (status, _) = send(&app, "GET", &format!("/repos/{repo}/labels/99999"), None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_label_under_an_unknown_parent_is_refused() {
    let (app, repo, _root) = setup("unknown_parent").await;
    let (status, body) = send(
        &app,
        "POST",
        &format!("/repos/{repo}/labels"),
        Some(json!({"name": "x", "parent": 99999})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn a_write_inside_a_context_carries_its_label() {
    let (app, repo, _root) = setup("context").await;
    let run = new_label(&app, &repo, json!({"name": "tag run"})).await;
    let answer = new_label(&app, &repo, json!({"name": "answer 1", "parent": run})).await;

    let (status, body) = create(&app, &repo, Some(&answer.to_string())).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let op = newest_op(&app, &repo).await;
    assert_eq!(op["label_id"], answer, "{op}");

    // The listing names the label and every ancestor of it.
    let (_, log) = send(&app, "GET", &format!("/repos/{repo}/log?limit=1"), None, None).await;
    let labels = log["labels"].as_array().unwrap();
    let ids: Vec<i64> = labels.iter().map(|l| l["id"].as_i64().unwrap()).collect();
    assert!(ids.contains(&answer) && ids.contains(&run), "{log}");
}

#[tokio::test]
async fn a_write_without_a_context_opens_its_own_label() {
    let (app, repo, _root) = setup("own").await;
    create(&app, &repo, None).await;
    let first = newest_op(&app, &repo).await;
    create(&app, &repo, None).await;
    let second = newest_op(&app, &repo).await;
    assert_ne!(first["label_id"], second["label_id"], "one label per request");

    let id = second["label_id"].as_i64().unwrap();
    let (_, label) = send(&app, "GET", &format!("/repos/{repo}/labels/{id}"), None, None).await;
    assert!(!label["name"].as_str().unwrap().is_empty(), "{label}");
    assert_eq!(label["parent"], Value::Null);
}

#[tokio::test]
async fn a_write_inside_an_unknown_context_is_refused_and_writes_nothing() {
    let (app, repo, _root) = setup("unknown_context").await;
    let before = newest_op(&app, &repo).await;
    let (status, body) = create(&app, &repo, Some("99999")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = create(&app, &repo, Some("not-a-number")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(newest_op(&app, &repo).await["id"], before["id"]);
}

#[tokio::test]
async fn a_label_detail_lists_its_operations() {
    let (app, repo, _root) = setup("detail").await;
    let label = new_label(&app, &repo, json!({"name": "two writes"})).await;
    create(&app, &repo, Some(&label.to_string())).await;
    create(&app, &repo, Some(&label.to_string())).await;

    let (status, detail) =
        send(&app, "GET", &format!("/repos/{repo}/log/labels/{label}"), None, None).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["label"]["name"], "two writes");
    assert_eq!(detail["label"]["operation_count"], 2);
    assert_eq!(detail["operations"].as_array().unwrap().len(), 2);

    let (status, head) =
        send(&app, "GET", &format!("/repos/{repo}/log/labels/head"), None, None).await;
    assert_eq!(status, StatusCode::OK, "{head}");
    assert_eq!(head["label"]["id"], label);
}

/// Reverting a label reverts what was written inside it, child labels
/// included: a run is undone whole.
#[tokio::test]
async fn reverting_a_label_reverts_its_children_too() {
    let (app, repo, _root) = setup("revert").await;
    let run = new_label(&app, &repo, json!({"name": "run"})).await;
    let one = new_label(&app, &repo, json!({"name": "one", "parent": run})).await;
    let two = new_label(&app, &repo, json!({"name": "two", "parent": run})).await;
    let (_, a) = create(&app, &repo, Some(&one.to_string())).await;
    let (_, b) = create(&app, &repo, Some(&two.to_string())).await;

    let (status, body) = send(
        &app,
        "POST",
        &format!("/repos/{repo}/revert"),
        Some(json!({"target": {"label_id": run}})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["reverted_operations"].as_array().unwrap().len(), 2, "{body}");
    for record in [&a, &b] {
        let uuid = record["uuid"].as_str().unwrap();
        let (status, _) =
            send(&app, "GET", &format!("/repos/{repo}/metarecords/{uuid}"), None, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uuid} was created inside the run");
    }
}

#[tokio::test]
async fn prune_keeps_a_label_whole() {
    let (app, repo, _root) = setup("prune").await;
    create(&app, &repo, None).await;
    let kept = new_label(&app, &repo, json!({"name": "kept"})).await;
    create(&app, &repo, Some(&kept.to_string())).await;
    create(&app, &repo, Some(&kept.to_string())).await;

    let (status, body) = send(
        &app,
        "POST",
        &format!("/repos/{repo}/log/prune"),
        Some(json!({"target": {"label": kept}})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, log) = send(&app, "GET", &format!("/repos/{repo}/log"), None, None).await;
    let ops = log["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 2, "{log}");
    assert!(ops.iter().all(|op| op["label_id"] == kept), "{log}");
}
