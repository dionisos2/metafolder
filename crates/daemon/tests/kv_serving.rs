//! A repository on the key-value store serves its queries from the store's
//! derived key spaces (spec-storage increment 4 d): it never builds the
//! resident query index, and answers what a SQLite repository answers.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use metafolder_daemon::config::Storage;
use metafolder_daemon::routes;
use metafolder_daemon::state::AppState;
use serde_json::{json, Value};
use tower::util::ServiceExt;

mod common;
use common::TempDir;

fn temp_dir(prefix: &str) -> TempDir {
    TempDir::new(&format!("kvserve_{prefix}"))
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

async fn setup(prefix: &str) -> (Router, String, TempDir, std::sync::Arc<AppState>) {
    let state = std::sync::Arc::new(AppState::new());
    let app = routes::build(state.clone());
    let root = temp_dir(prefix);
    let (status, body) =
        request(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    let repo = body["repo_uuid"].as_str().unwrap().to_string();
    (app, repo, root, state)
}

async fn create(app: &Router, repo: &str, fields: Value) -> String {
    let (status, body) = request(
        app,
        "POST",
        &format!("/repos/{repo}/metarecords"),
        Some(json!({"fields": fields})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create failed: {body}");
    body["uuid"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn a_kv_repository_serves_queries_without_the_resident_index() {
    let (app, repo, _root, state) = setup("serve").await;
    let tag = |t: &str| json!([{"name": "tag", "value": {"type": "string", "value": t}}]);
    let jazz = create(&app, &repo, tag("jazz")).await;
    let rock = create(&app, &repo, tag("rock")).await;

    let eq =
        |t: &str| json!({"type": "eq", "field": "tag", "value": {"type": "string", "value": t}});
    let (status, body) =
        request(&app, "POST", &format!("/repos/{repo}/query"), Some(json!({"query": eq("jazz")})))
            .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!([jazz]));

    let present = json!({"type": "is_present", "field": "tag"});
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": present, "limit": 1, "count": true,
                    "sort": [{"field": "tag", "order": "desc"}]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["results"], json!([rock]));
    assert_eq!(body["total"], json!(2));

    let (status, body) = request(&app, "GET", &format!("/repos/{repo}/fields"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.to_string().contains("\"tag\""), "the catalog lists tag: {body}");

    let repo_state = state.repo(repo.parse().unwrap()).unwrap();
    let on_kv = repo_state.config.storage == Storage::Kv;
    assert_eq!(
        repo_state.index.lock().unwrap().is_none(),
        on_kv,
        "a KV repository never builds the resident index; a SQLite one does"
    );
}

/// `POST /repos/init` takes the backend: `storage` is `"kv"` or `"sqlite"`,
/// the daemon's default when absent.
#[tokio::test]
async fn init_takes_the_storage_backend() {
    let state = std::sync::Arc::new(AppState::new());
    let app = routes::build(state.clone());
    for (asked, want) in [("sqlite", Storage::Sqlite), ("kv", Storage::Kv)] {
        let root = temp_dir(asked);
        let (status, body) = request(
            &app,
            "POST",
            "/repos/init",
            Some(json!({"root": root.to_str().unwrap(), "storage": asked})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "init failed: {body}");
        let repo = state.repo(body["repo_uuid"].as_str().unwrap().parse().unwrap()).unwrap();
        assert_eq!(repo.config.storage, want);
    }
}
