//! HTTP shape of `POST /repos/:repo/hash` (doc "Content hashes kept fresh"): 202 + task
//! id, then the summary on the task.

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

async fn setup(prefix: &str) -> (Router, String, TempDir) {
    let app = routes::build(std::sync::Arc::new(AppState::new(common::settings())));
    let root = TempDir::new(&format!("hashhttp_{prefix}"));
    common::seed_config(&root);
    let (status, body) =
        request(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    let repo = body["repo_uuid"].as_str().unwrap().to_string();
    (app, repo, root)
}

/// Polls the task until it leaves the non-terminal states.
async fn await_task(app: &Router, repo: &str, task_id: &str) -> Value {
    for _ in 0..200 {
        let (status, task) =
            request(app, "GET", &format!("/repos/{repo}/tasks/{task_id}"), None).await;
        assert_eq!(status, StatusCode::OK, "task read failed: {task}");
        match task["status"].as_str() {
            Some("done") | Some("failed") | Some("cancelled") => return task,
            _ => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
        }
    }
    panic!("the hash task never reached a terminal state");
}

#[tokio::test]
async fn hash_returns_202_and_a_task_carrying_the_summary() {
    let (app, repo, root) = setup("summary").await;
    std::fs::write(root.join("a.txt"), b"some bytes").unwrap();
    let abs = root.join("a.txt");
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/metarecords"),
        Some(json!({"path": abs.to_str().unwrap(), "fields": []})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create at path failed: {body}");

    let query = json!({"type": "eq", "field": "mfr_type",
                       "value": {"type": "string", "value": "file"}});
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/hash"),
        Some(json!({"query": query, "rehash": false})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "expected 202, got {body}");
    let task_id = body["task_id"].as_str().expect("a task id").to_string();

    let task = await_task(&app, &repo, &task_id).await;
    assert_eq!(task["status"], "done", "task: {task}");
    assert_eq!(task["kind"], "hash");
    assert_eq!(task["result"], json!({"hashed": 1, "reused": 0, "skipped": 0}));
}

#[tokio::test]
async fn a_malformed_query_is_refused_before_any_task() {
    let (app, repo, _root) = setup("malformed").await;
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/hash"),
        Some(json!({"query": {"type": "no_such_operator"}})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "got {body}");
}
