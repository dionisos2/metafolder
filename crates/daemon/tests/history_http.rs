//! Integration tests for the per-repo input-history endpoints
//! (`GET`/`POST /repos/:repo/history/:zone`, spec-gui "Input history").

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

fn temp_dir(prefix: &str) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("metafolder_history_{prefix}_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&path).unwrap();
    path
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

async fn app_with_repo(prefix: &str) -> (Router, String, PathBuf) {
    let app = app();
    let root = temp_dir(prefix);
    let (status, body) =
        request(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    let repo = body["repo_uuid"].as_str().unwrap().to_string();
    (app, repo, root)
}

async fn append(app: &Router, repo: &str, zone: &str, entry: &str) -> (StatusCode, Value) {
    request(app, "POST", &format!("/repos/{repo}/history/{zone}"), Some(json!({"entry": entry})))
        .await
}

async fn entries(app: &Router, repo: &str, zone: &str) -> Vec<String> {
    let (status, body) = request(app, "GET", &format!("/repos/{repo}/history/{zone}"), None).await;
    assert_eq!(status, StatusCode::OK, "get failed: {body}");
    body["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

// ── Basics ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_get_history_on_fresh_repo_is_empty() {
    let (app, repo, _root) = app_with_repo("fresh").await;
    assert_eq!(entries(&app, &repo, "shell:command").await, Vec::<String>::new());
}

#[tokio::test]
async fn test_append_then_get_roundtrip() {
    let (app, repo, root) = app_with_repo("roundtrip").await;
    let (status, body) = append(&app, &repo, "shell:command", "repo:list").await;
    assert_eq!(status, StatusCode::OK, "append failed: {body}");
    assert_eq!(body["appended"], json!(true));
    assert_eq!(entries(&app, &repo, "shell:command").await, vec!["repo:list"]);
    // The entry is persisted as a newline-terminated line under internal/history/.
    let file = root.join(".metafolder/internal/history/shell:command");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "repo:list\n");
}

#[tokio::test]
async fn test_entries_are_returned_oldest_first_newest_last() {
    let (app, repo, _root) = app_with_repo("order").await;
    append(&app, &repo, "z", "one").await;
    append(&app, &repo, "z", "two").await;
    append(&app, &repo, "z", "three").await;
    assert_eq!(entries(&app, &repo, "z").await, vec!["one", "two", "three"]);
}

#[tokio::test]
async fn test_zones_are_independent() {
    let (app, repo, _root) = app_with_repo("zones").await;
    append(&app, &repo, "shell:command", "a").await;
    append(&app, &repo, "shell:bash", "b").await;
    assert_eq!(entries(&app, &repo, "shell:command").await, vec!["a"]);
    assert_eq!(entries(&app, &repo, "shell:bash").await, vec!["b"]);
}

// ── Dedup ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_consecutive_duplicate_is_deduped() {
    let (app, repo, _root) = app_with_repo("dedup").await;
    append(&app, &repo, "z", "same").await;
    let (status, body) = append(&app, &repo, "z", "same").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["appended"], json!(false));
    assert_eq!(entries(&app, &repo, "z").await, vec!["same"]);
}

#[tokio::test]
async fn test_non_consecutive_duplicate_is_kept() {
    let (app, repo, _root) = app_with_repo("dup_kept").await;
    append(&app, &repo, "z", "a").await;
    append(&app, &repo, "z", "b").await;
    append(&app, &repo, "z", "a").await;
    assert_eq!(entries(&app, &repo, "z").await, vec!["a", "b", "a"]);
}

// ── Limit ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_limit_returns_the_newest_n() {
    let (app, repo, _root) = app_with_repo("limit").await;
    append(&app, &repo, "z", "one").await;
    append(&app, &repo, "z", "two").await;
    append(&app, &repo, "z", "three").await;
    let (status, body) = request(&app, "GET", &format!("/repos/{repo}/history/z?limit=2"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["entries"], json!(["two", "three"]));
}

// ── Validation ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_invalid_zone_is_rejected() {
    let (app, repo, root) = app_with_repo("bad_zone").await;
    for zone in ["Bad", "a%2Fb", "a.b", &"x".repeat(65)] {
        let (status, body) = append(&app, &repo, zone, "entry").await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "zone '{zone}' accepted: {body}");
        assert!(body["error"].is_string());
        let (status, _) = request(&app, "GET", &format!("/repos/{repo}/history/{zone}"), None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    // No history directory (nor any file) was created by the rejected writes.
    assert!(!root.join(".metafolder/internal/history").exists());
}

#[tokio::test]
async fn test_empty_or_multiline_entry_is_rejected() {
    let (app, repo, _root) = app_with_repo("bad_entry").await;
    for entry in ["", "   ", "a\nb", "a\rb"] {
        let (status, body) = append(&app, &repo, "z", entry).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "entry {entry:?} accepted: {body}");
    }
    assert_eq!(entries(&app, &repo, "z").await, Vec::<String>::new());
}

#[tokio::test]
async fn test_unknown_repo_is_404() {
    let app = app();
    let missing = Uuid::new_v4().as_simple().to_string();
    let (status, _) = request(&app, "GET", &format!("/repos/{missing}/history/z"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) =
        request(&app, "POST", &format!("/repos/{missing}/history/z"), Some(json!({"entry": "x"})))
            .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ── Cap ───────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_cap_keeps_the_newest_1000() {
    let (app, repo, root) = app_with_repo("cap").await;
    // Pre-write a full 1000-line file directly, then append one more via HTTP.
    let dir = root.join(".metafolder/internal/history");
    std::fs::create_dir_all(&dir).unwrap();
    let lines: String = (0..1000).map(|i| format!("entry-{i}\n")).collect();
    std::fs::write(dir.join("z"), lines).unwrap();
    append(&app, &repo, "z", "the-newest").await;
    let got = entries(&app, &repo, "z").await;
    assert_eq!(got.len(), 1000);
    assert_eq!(got.first().unwrap(), "entry-1"); // entry-0 dropped
    assert_eq!(got.last().unwrap(), "the-newest");
}

// ── Persistence ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_history_survives_unload_and_reload() {
    let (app, repo, root) = app_with_repo("persist").await;
    append(&app, &repo, "z", "kept").await;
    let (status, _) = request(&app, "POST", &format!("/repos/{repo}/unload"), None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) =
        request(&app, "POST", "/repos/load", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "reload failed: {body}");
    assert_eq!(entries(&app, &repo, "z").await, vec!["kept"]);
}
