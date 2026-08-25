//! HTTP-level tests for the orphan scan/clear endpoints
//! (`POST /repos/:repo/orphans/{scan,clear}`, spec-file-tracking "Orphan scan").


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

async fn setup(prefix: &str) -> (Router, String, TempDir) {
    let app = routes::build(std::sync::Arc::new(AppState::new()));
    let root = TempDir::new(&format!("ohttp_{prefix}"));
    let (status, body) =
        request(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    let repo = body["repo_uuid"].as_str().unwrap().to_string();
    (app, repo, root)
}

fn field<'a>(entry: &'a Value, name: &str) -> Option<&'a Value> {
    entry["fields"].as_array().unwrap().iter().find(|f| f["name"] == name).map(|f| &f["value"])
}

#[tokio::test]
async fn scan_then_clear_over_http() {
    let (app, repo, root) = setup("scanclear").await;
    std::fs::write(root.join("gone.txt"), b"data").unwrap();

    // Track the file so it gets an mfr_path metarecord.
    let abs = root.join("gone.txt");
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/track"),
        Some(json!({"path": abs.to_str().unwrap()})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "track failed: {body}");
    let uuid = body["uuid"].as_str().unwrap().to_string();

    // Delete it behind the daemon's back → stale mfr_path.
    std::fs::remove_file(&abs).unwrap();

    // Scan surfaces exactly this record.
    let (status, scan) = request(&app, "POST", &format!("/repos/{repo}/orphans/scan"), None).await;
    assert_eq!(status, StatusCode::OK, "scan failed: {scan}");
    assert_eq!(scan["count"], 1, "scan: {scan}");
    assert_eq!(scan["orphans"][0]["uuid"], uuid);
    assert_eq!(scan["orphans"][0]["stale_path"], "/gone.txt");

    // Clear it: mfr_path → Nothing, mfr_path_old frozen.
    let (status, cleared) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/orphans/clear"),
        Some(json!({ "uuids": [uuid] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "clear failed: {cleared}");
    assert_eq!(cleared["cleared"], 1);

    let (_, entry) = request(&app, "GET", &format!("/repos/{repo}/metarecords/{uuid}"), None).await;
    assert_eq!(field(&entry, "mfr_path"), Some(&json!({"type": "nothing"})));
    assert_eq!(
        field(&entry, "mfr_path_old"),
        Some(&json!({"type": "string", "value": "/gone.txt"}))
    );

    // A second scan finds nothing (already orphaned).
    let (_, scan2) = request(&app, "POST", &format!("/repos/{repo}/orphans/scan"), None).await;
    assert_eq!(scan2["count"], 0, "scan2: {scan2}");

    std::fs::remove_dir_all(root).unwrap();
}
