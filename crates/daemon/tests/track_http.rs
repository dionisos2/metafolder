//! HTTP-level tests for `POST /reconcile`, `POST /track` and the
//! single-entry `POST /metadata/:uuid/reconcile`.

use std::path::PathBuf;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use metafolder_daemon::routes;
use metafolder_daemon::state::AppState;
use serde_json::{json, Value};
use tower::util::ServiceExt;
use uuid::Uuid;

async fn request(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
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

async fn setup(prefix: &str) -> (Router, String, PathBuf) {
    let app = routes::build(std::sync::Arc::new(AppState::new()));
    let root = std::env::temp_dir().join(format!("metafolder_thttp_{prefix}_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let (status, body) =
        request(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    let repo = body["repo_uuid"].as_str().unwrap().to_string();
    (app, repo, root)
}

async fn get_record(app: &Router, repo: &str, uuid: &str) -> Value {
    let (status, body) =
        request(app, "GET", &format!("/repos/{repo}/records/{uuid}"), None).await;
    assert_eq!(status, StatusCode::OK, "get failed: {body}");
    body
}

fn field<'a>(entry: &'a Value, name: &str) -> Option<&'a Value> {
    entry["fields"].as_array().unwrap().iter().find(|f| f["name"] == name).map(|f| &f["value"])
}

#[tokio::test]
async fn test_track_creates_record_and_parents_untracked() {
    let (app, repo, root) = setup("track").await;
    std::fs::create_dir_all(root.join("docs/notes")).unwrap();
    std::fs::write(root.join("docs/notes/todo.txt"), b"todo").unwrap();

    let abs = root.join("docs/notes/todo.txt");
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/track"),
        Some(json!({"path": abs.to_str().unwrap()})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "track failed: {body}");
    let uuid = body["uuid"].as_str().unwrap().to_string();

    // The entry carries stat fields and mf_watch = false.
    let entry = get_record(&app, &repo, &uuid).await;
    assert_eq!(field(&entry, "mfr_size").unwrap()["value"], 4);
    assert_eq!(field(&entry, "mf_watch").unwrap()["value"], false);

    // Tracking again → 409.
    let (status, _) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/track"),
        Some(json!({"path": abs.to_str().unwrap()})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Outside the root → 400.
    let (status, _) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/track"),
        Some(json!({"path": "/etc/hostname"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_full_reconcile_endpoint() {
    let (app, repo, root) = setup("rec").await;
    // Enable tracking on the root.
    let (_, roots) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": {"type": "is_present", "field": "mf_watch"}})),
    )
    .await;
    let root_uuid = roots[0].as_str().unwrap();
    request(
        &app,
        "PATCH",
        &format!("/repos/{repo}/records/{root_uuid}"),
        Some(json!({"name": "mf_watch", "value": {"type": "bool", "value": true}})),
    )
    .await;

    std::fs::write(root.join("one.txt"), b"1").unwrap();
    std::fs::write(root.join("two.txt"), b"2").unwrap();
    let (status, body) =
        request(&app, "POST", &format!("/repos/{repo}/reconcile"), None).await;
    assert_eq!(status, StatusCode::OK, "reconcile failed: {body}");
    assert_eq!(body["created"], 4, "one.txt + two.txt + .metafolder + config.json");
    assert_eq!(body["moved"], 0);
    assert_eq!(body["candidates"], json!([]));

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_single_record_reconcile_endpoint() {
    let (app, repo, root) = setup("recone").await;
    std::fs::create_dir_all(root.join("music")).unwrap();
    std::fs::write(root.join("music/a.mp3"), b"aaa").unwrap();

    // Track the directory, then activate it directly.
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/track"),
        Some(json!({"path": root.join("music").to_str().unwrap()})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "track failed: {body}");
    let dir_uuid = body["uuid"].as_str().unwrap().to_string();
    request(
        &app,
        "PATCH",
        &format!("/repos/{repo}/records/{dir_uuid}"),
        Some(json!({"name": "mf_watch", "value": {"type": "bool", "value": true}})),
    )
    .await;

    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/records/{dir_uuid}/reconcile"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "entry reconcile failed: {body}");
    assert_eq!(body["created"], 1, "a.mp3 gets an entry");
    assert_eq!(body["moved"], 0);

    // 404 for an unknown entry; 400 for an entry without a path.
    let bogus = Uuid::new_v4().as_simple().to_string();
    let (status, _) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/records/{bogus}/reconcile"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (_, no_path) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/records"),
        Some(json!({"fields": [{"name": "label", "value": {"type": "string", "value": "x"}}]})),
    )
    .await;
    let no_path_uuid = no_path["uuid"].as_str().unwrap();
    let (status, _) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/records/{no_path_uuid}/reconcile"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    std::fs::remove_dir_all(root).unwrap();
}
