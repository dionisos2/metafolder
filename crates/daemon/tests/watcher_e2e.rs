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

/// Regression: a repository whose root contains a symlink to a directory the
/// daemon cannot read (the classic Wine `~/.wine/dosdevices/z: -> /` case) must
/// still load. The watcher must not follow the symlink, and one unwatchable
/// path must never abort the whole load.
#[tokio::test(flavor = "multi_thread")]
async fn test_load_succeeds_with_symlink_to_unreadable_dir() {
    use std::os::unix::fs::PermissionsExt;

    let app = routes::build(std::sync::Arc::new(AppState::new()));
    let root: PathBuf =
        std::env::temp_dir().join(format!("metafolder_symlink_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();

    // A readable directory outside the repo that itself contains an *unreadable*
    // subdirectory, and a symlink to it inside the repo root. This mirrors the
    // real case `z: -> /` where `/` is readable but `/opt/containerd` is not:
    // following the symlink and trying to add an inotify watch on the deep
    // unreadable directory EACCESes.
    let secret: PathBuf =
        std::env::temp_dir().join(format!("metafolder_secret_{}", Uuid::new_v4()));
    let locked = secret.join("locked");
    std::fs::create_dir_all(&locked).unwrap();
    std::os::unix::fs::symlink(&secret, root.join("z")).unwrap();
    let mut perm = std::fs::metadata(&locked).unwrap().permissions();
    perm.set_mode(0o000);
    std::fs::set_permissions(&locked, perm).unwrap();

    // Init: must succeed (a fresh repo is mf_watch=false — nothing watched).
    let (status, body) =
        request(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "init must not fail on the symlink: {body}");
    let repo = body["repo_uuid"].as_str().unwrap().to_string();

    // Enabling watch on the root triggers a watch refresh that walks the tree:
    // it must skip the symlink (not a real directory) and not EACCES.
    let (_, roots) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": {"type": "is_present", "field": "mf_watch"}})),
    )
    .await;
    let root_uuid = roots[0].as_str().unwrap().to_string();
    let (status, body) = request(
        &app,
        "PUT",
        &format!("/repos/{repo}/metarecords/{root_uuid}/fields/mf_watch"),
        Some(json!({"value": {"type": "bool", "value": true}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "enabling watch must not fail on the symlink: {body}");

    // A real file created in the root is still detected: the refresh placed a
    // watch on the (eligible) root directory.
    std::fs::write(root.join("real.txt"), b"hello").unwrap();
    let by_name = json!({"type": "matches", "field": "mfr_path", "pattern": "^real\\.txt$"});
    wait_for_match(&app, &repo, by_name, 1).await;

    // Cleanup: restore permissions so the trees can be removed.
    let mut perm = std::fs::metadata(&locked).unwrap().permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(&locked, perm).unwrap();
    std::fs::remove_dir_all(&secret).ok();
    std::fs::remove_dir_all(&root).ok();
}
