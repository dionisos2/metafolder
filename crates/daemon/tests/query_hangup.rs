//! A client that stops waiting for a query cancels it (doc "Query limits"): the GUI dropping a
//! query a newer one replaced, a Ctrl-C on
//! `mf`. Over a real socket, since what is tested is the connection closing.
//!
//! Deterministic by holding the repository: the query waits for the store
//! while the client hangs up, and must not run once it gets it.

use std::io::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use metafolder_daemon::routes;
use metafolder_daemon::state::AppState;
use metafolder_daemon::tasks::{TaskKind, TaskStatus};
use serde_json::{json, Value};
use tower::util::ServiceExt;

mod common;
use common::TempDir;

async fn call(app: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// Polls `cond` for up to ten seconds.
async fn eventually(what: &str, cond: impl Fn() -> bool) {
    let start = Instant::now();
    while !cond() {
        assert!(start.elapsed() < Duration::from_secs(10), "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// The store's own lock is held across awaits on purpose: the query, on a
// blocking thread, must wait for it while the client hangs up.
#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_hanging_up_cancels_its_query() {
    let state = Arc::new(AppState::new());
    let app = routes::build(state.clone());
    let root = TempDir::new("hangup");
    let (status, body) = call(&app, "/repos/init", json!({"root": root.to_str().unwrap()})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let repo_uuid = body["repo_uuid"].as_str().unwrap().to_string();
    let repo = state.ready_repo(repo_uuid.parse().unwrap()).unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });

    // The query will wait for the store: hold it.
    let held = repo.conn.lock().unwrap();

    let query = json!({"query": {"type": "is_present", "field": "mfr_path"}}).to_string();
    let mut client = std::net::TcpStream::connect(addr).unwrap();
    write!(
        client,
        "POST /repos/{repo_uuid}/query HTTP/1.1\r\nHost: x\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{query}",
        query.len()
    )
    .unwrap();
    eventually("the query task", || repo.tasks.active_id(TaskKind::Query).is_some()).await;
    let task = repo.tasks.active_id(TaskKind::Query).unwrap();

    // The client gives up.
    drop(client);
    eventually("the hang-up to be seen", || repo.tasks.is_cancel_requested(task)).await;

    // The store comes free: the query stops at its first read.
    drop(held);
    eventually("the task to end", || repo.tasks.get(task).is_some_and(|t| !t.status.is_active()))
        .await;
    assert_eq!(repo.tasks.get(task).unwrap().status, TaskStatus::Cancelled);
}
