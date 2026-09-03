//! HTTP-level tests for `GET /repos/:repo/watch` and its pause/resume pair
//! (spec-file-tracking "Watch status, pause and resume").

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

async fn init_repo(app: &Router, root: &TempDir) -> String {
    let (status, body) =
        request(app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    body["repo_uuid"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn watch_reports_running_and_pause_resume_flip_it() {
    let app = routes::build(std::sync::Arc::new(AppState::new()));
    let root = TempDir::new("watch_http");
    let repo = init_repo(&app, &root).await;

    let (status, body) = request(&app, "GET", &format!("/repos/{repo}/watch"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["paused"], json!(false), "a freshly loaded repository ingests");
    assert_eq!(body["pending_events"], json!(0));

    let (status, body) =
        request(&app, "POST", &format!("/repos/{repo}/watch/pause"), Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["paused"], json!(true));

    // Idempotent: pausing a paused repository is not an error.
    let (status, body) =
        request(&app, "POST", &format!("/repos/{repo}/watch/pause"), Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["paused"], json!(true));

    let (status, body) = request(&app, "GET", &format!("/repos/{repo}/watch"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["paused"], json!(true), "the pause is visible to a later reader");

    let (status, body) =
        request(&app, "POST", &format!("/repos/{repo}/watch/resume"), Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["paused"], json!(false));
}

#[tokio::test]
async fn watch_on_an_unknown_repository_is_404() {
    let app = routes::build(std::sync::Arc::new(AppState::new()));
    let unknown = uuid::Uuid::new_v4().as_simple().to_string();
    let (status, _) = request(&app, "GET", &format!("/repos/{unknown}/watch"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
