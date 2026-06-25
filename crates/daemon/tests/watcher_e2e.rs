//! End-to-end test: a repository initialised through the HTTP API watches
//! its root via inotify; file operations show up as metadata entries after
//! the executor's quiet period.

use std::path::PathBuf;
use std::time::Duration;

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

/// Polls the query endpoint until the predicate yields a hit or times out.
async fn wait_for_match(app: &Router, repo: &str, query: Value, expect: usize) -> Vec<String> {
    for _ in 0..50 {
        let (status, body) = request(
            app,
            "POST",
            &format!("/repos/{repo}/query"),
            Some(json!({"query": query})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "query failed: {body}");
        let hits: Vec<String> = body
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        if hits.len() == expect {
            return hits;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {expect} match(es)");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_watcher_tracks_create_rename_delete() {
    let app = routes::build(std::sync::Arc::new(AppState::new()));
    let root: PathBuf =
        std::env::temp_dir().join(format!("metafolder_e2e_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();

    let (status, body) =
        request(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    let repo = body["repo_uuid"].as_str().unwrap().to_string();

    // Enable tracking on the root entry.
    let (_, roots) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": {"type": "is_present", "field": "mf_watch"}})),
    )
    .await;
    let root_uuid = roots[0].as_str().unwrap().to_string();
    let (status, _) = request(
        &app,
        "PUT",
        &format!("/repos/{repo}/metarecords/{root_uuid}/fields/mf_watch"),
        Some(json!({"value": {"type": "bool", "value": true}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Create.
    std::fs::write(root.join("track_me.txt"), b"hello watcher").unwrap();
    let by_name =
        json!({"type": "matches", "field": "mfr_path", "pattern": "^track_me\\.txt$"});
    let hits = wait_for_match(&app, &repo, by_name, 1).await;
    let metarecord_uuid = hits[0].clone();

    // Rename.
    std::fs::rename(root.join("track_me.txt"), root.join("renamed.txt")).unwrap();
    let renamed = json!({"type": "matches", "field": "mfr_path", "pattern": "^renamed\\.txt$"});
    let hits = wait_for_match(&app, &repo, renamed, 1).await;
    assert_eq!(hits[0], metarecord_uuid, "the entry must survive the rename");

    // Delete: mfr_path becomes Nothing, the entry is preserved.
    std::fs::remove_file(root.join("renamed.txt")).unwrap();
    let absent = json!({"type": "is_absent", "field": "mfr_path"});
    let hits = wait_for_match(&app, &repo, absent, 1).await;
    assert_eq!(hits[0], metarecord_uuid, "the entry must be preserved after deletion");

    std::fs::remove_dir_all(root).unwrap();
}
