//! `POST /repos/:repo/activity`: the GUI says the user is at work, which holds
//! the folding of what writes left pending back (doc "Folding in idle time").

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

#[tokio::test]
async fn activity_is_noted_for_a_loaded_repository() {
    let state = std::sync::Arc::new(AppState::new(common::settings()));
    let app = routes::build(state.clone());
    let root = TempDir::new("activity_http");
    common::seed_config(&root);
    let (status, body) =
        request(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    let repo = body["repo_uuid"].as_str().unwrap().to_string();
    let uuid = uuid::Uuid::parse_str(&repo).unwrap();

    let (status, _) = request(&app, "POST", &format!("/repos/{repo}/activity"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // Noted just now: not idle for a minute.
    let repo_state = state.repo(uuid).unwrap();
    assert_eq!(
        metafolder_daemon::folding::step(&repo_state, std::time::Duration::from_secs(60)).unwrap(),
        None
    );

    let unknown = "0".repeat(32);
    let (status, _) = request(&app, "POST", &format!("/repos/{unknown}/activity"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
