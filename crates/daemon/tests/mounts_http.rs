//! HTTP-level test for `GET /repos/:repo/mounts` (spec-file-tracking "Mount
//! status").

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
async fn mounts_lists_declared_mount_points_with_their_state() {
    let app = routes::build(std::sync::Arc::new(AppState::new()));
    let root = TempDir::new("mhttp_list");
    let (status, body) =
        request(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    let repo = body["repo_uuid"].as_str().unwrap().to_string();

    // A repository with no removable volume declares no mount point.
    let (status, body) = request(&app, "GET", &format!("/repos/{repo}/mounts"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["mounts"].as_array().unwrap().len(), 0);

    // Track a directory, then mark it as the mount point of a volume that is
    // not plugged in (an `mfr_*` write, hence `force`).
    std::fs::create_dir(root.join("photos")).unwrap();
    let abs = root.join("photos");
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/track"),
        Some(json!({"path": abs.to_str().unwrap()})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "track failed: {body}");
    let uuid = body["uuid"].as_str().unwrap().to_string();

    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/metarecords/{uuid}/fields"),
        Some(json!({"name": "mfr_mount", "value": {"type": "string", "value": "label:PHOTOS"},
                    "force": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "declaring the mount point failed: {body}");

    let (status, body) = request(&app, "GET", &format!("/repos/{repo}/mounts"), None).await;
    assert_eq!(status, StatusCode::OK);
    let mounts = body["mounts"].as_array().unwrap();
    assert_eq!(mounts.len(), 1, "{body}");
    assert_eq!(mounts[0]["uuid"], uuid);
    assert_eq!(mounts[0]["path"], "/photos");
    assert_eq!(mounts[0]["expected"], "label:PHOTOS");
    assert_eq!(mounts[0]["current"], Value::Null);
    assert_eq!(mounts[0]["state"], "offline");
}
