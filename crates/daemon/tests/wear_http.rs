//! `GET /repos/:repo/wear`: how worn the store is (doc "Storage wear").

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
async fn the_wear_of_a_new_repository_is_nothing_pending() {
    let app = routes::build(std::sync::Arc::new(AppState::new(common::settings())));
    let root = TempDir::new("wear_http");
    common::seed_config(&root);
    let (status, body) =
        request(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    let repo = body["repo_uuid"].as_str().unwrap().to_string();

    let (status, wear) = request(&app, "GET", &format!("/repos/{repo}/wear"), None).await;
    assert_eq!(status, StatusCode::OK, "{wear}");
    assert_eq!(wear["mass_entries"], json!(0));
    assert_eq!(wear["events"], json!(0));
    assert_eq!(wear["pending_bytes"], json!(0));
    assert!(wear["dead_ids"].is_u64() && wear["dead_fraction"].is_f64(), "{wear}");

    let unknown = "0".repeat(32);
    let (status, _) = request(&app, "GET", &format!("/repos/{unknown}/wear"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
