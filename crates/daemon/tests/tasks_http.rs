//! Integration tests for the task read endpoints (spec-tasks): per-repo and
//! global listing, single-task fetch, 404s. Tasks are seeded directly through
//! the public registry so these tests don't depend on reconcile/query wiring.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use metafolder_daemon::routes;
use metafolder_daemon::state::AppState;
use metafolder_daemon::tasks::TaskKind;
use serde_json::Value;
use tower::util::ServiceExt;
use uuid::Uuid;

fn temp_dir(prefix: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("metafolder_tasks_{prefix}_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&path).unwrap();
    path
}

async fn request(app: &Router, method: &str, uri: &str) -> (StatusCode, Value) {
    let request = Request::builder().method(method).uri(uri).body(Body::empty()).unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap() };
    (status, value)
}

async fn post(app: &Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap() };
    (status, value)
}

/// Returns (router, state, repo_uuid hex).
fn app_with_repo(prefix: &str) -> (Router, Arc<AppState>, String) {
    let state = Arc::new(AppState::new());
    let app = routes::build(state.clone());
    let root = temp_dir(prefix);
    let repo = state.init_repo(&root, None, None).unwrap();
    (app, state, repo.as_simple().to_string())
}

#[tokio::test]
async fn list_repo_tasks_is_empty_initially() {
    let (app, _state, repo) = app_with_repo("empty");
    let (status, body) = request(&app, "GET", &format!("/repos/{repo}/tasks")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, serde_json::json!([]));
}

#[tokio::test]
async fn list_repo_tasks_returns_seeded_task() {
    let (app, state, repo) = app_with_repo("seed");
    let repo_uuid = Uuid::parse_str(&repo).unwrap();
    let id = state.repo(repo_uuid).unwrap().tasks.start(TaskKind::Reconcile);
    state.repo(repo_uuid).unwrap().tasks.mark_running(id);
    state.repo(repo_uuid).unwrap().tasks.set_progress(id, "fingerprint", Some(2), Some(10));

    let (status, body) = request(&app, "GET", &format!("/repos/{repo}/tasks")).await;
    assert_eq!(status, StatusCode::OK);
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    let t = &arr[0];
    assert_eq!(t["id"].as_str().unwrap(), id.as_simple().to_string());
    assert_eq!(t["repo_uuid"].as_str().unwrap(), repo);
    assert_eq!(t["kind"], "reconcile");
    assert_eq!(t["status"], "running");
    assert_eq!(t["phase"], "fingerprint");
    assert_eq!(t["done"], 2);
    assert_eq!(t["total"], 10);
    assert!(t["started_at"].as_str().unwrap().ends_with('Z'));
}

#[tokio::test]
async fn get_single_task_by_id() {
    let (app, state, repo) = app_with_repo("single");
    let repo_uuid = Uuid::parse_str(&repo).unwrap();
    let id = state.repo(repo_uuid).unwrap().tasks.start(TaskKind::Query);
    let hex = id.as_simple().to_string();

    let (status, body) = request(&app, "GET", &format!("/repos/{repo}/tasks/{hex}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"].as_str().unwrap(), hex);
    assert_eq!(body["kind"], "query");
    assert_eq!(body["status"], "pending");
}

#[tokio::test]
async fn get_unknown_task_is_404() {
    let (app, _state, repo) = app_with_repo("ghost");
    let ghost = Uuid::new_v4().as_simple().to_string();
    let (status, _) = request(&app, "GET", &format!("/repos/{repo}/tasks/{ghost}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn tasks_on_unknown_repo_is_404() {
    let (app, _state, _repo) = app_with_repo("norepo");
    let other = Uuid::new_v4().as_simple().to_string();
    let (status, _) = request(&app, "GET", &format!("/repos/{other}/tasks")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn query_registers_an_observable_task() {
    let (app, _state, repo) = app_with_repo("queryobs");
    // A query is synchronous; its observation task is retained after completion.
    let (status, _) = post(
        &app,
        &format!("/repos/{repo}/query"),
        serde_json::json!({"query": {"type": "is_present", "field": "mfr_path"}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = request(&app, "GET", &format!("/repos/{repo}/tasks")).await;
    assert_eq!(status, StatusCode::OK);
    let query_task = body
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["kind"] == "query")
        .expect("a query task is recorded");
    assert_eq!(query_task["status"], "done");
    // Observation-only: no result payload, counts unknown.
    assert!(query_task["result"].is_null());
    assert!(query_task["done"].is_null());
    assert!(query_task["total"].is_null());
}

#[tokio::test]
async fn concurrent_reconcile_is_rejected_with_409() {
    let (app, state, repo) = app_with_repo("dedup");
    let repo_uuid = Uuid::parse_str(&repo).unwrap();
    // Occupy the reconcile slot with an active task.
    state.repo(repo_uuid).unwrap().tasks.start_unique(TaskKind::Reconcile).unwrap();

    let (status, _) = request(&app, "POST", &format!("/repos/{repo}/reconcile")).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn cancel_active_reconcile_sets_the_flag_and_returns_the_task() {
    let (app, state, repo) = app_with_repo("cancelok");
    let repo_uuid = Uuid::parse_str(&repo).unwrap();
    // Seed a running reconcile (no real worker, so it stays running with the
    // flag set; the route's contract is what we assert here).
    let id = state.repo(repo_uuid).unwrap().tasks.start(TaskKind::Reconcile);
    state.repo(repo_uuid).unwrap().tasks.mark_running(id);
    let hex = id.as_simple().to_string();

    let (status, body) = request(&app, "POST", &format!("/repos/{repo}/tasks/{hex}/cancel")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"].as_str().unwrap(), hex);
    assert!(state.repo(repo_uuid).unwrap().tasks.is_cancel_requested(id));
}

#[tokio::test]
async fn cancel_flush_task_is_400() {
    let (app, state, repo) = app_with_repo("cancelflush");
    let repo_uuid = Uuid::parse_str(&repo).unwrap();
    let id = state.repo(repo_uuid).unwrap().tasks.start(TaskKind::Flush);
    let hex = id.as_simple().to_string();
    let (status, _) = request(&app, "POST", &format!("/repos/{repo}/tasks/{hex}/cancel")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn cancel_terminal_task_is_409() {
    let (app, state, repo) = app_with_repo("cancelterm");
    let repo_uuid = Uuid::parse_str(&repo).unwrap();
    let id = state.repo(repo_uuid).unwrap().tasks.start(TaskKind::Reconcile);
    state.repo(repo_uuid).unwrap().tasks.finish(id, None);
    let hex = id.as_simple().to_string();
    let (status, _) = request(&app, "POST", &format!("/repos/{repo}/tasks/{hex}/cancel")).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn cancel_unknown_task_is_404() {
    let (app, _state, repo) = app_with_repo("cancelghost");
    let ghost = Uuid::new_v4().as_simple().to_string();
    let (status, _) = request(&app, "POST", &format!("/repos/{repo}/tasks/{ghost}/cancel")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn load_runs_an_observable_warmup_task() {
    // Loading a repository returns immediately and warms it (tree cache + query
    // index) in the background as an observable `load` task, so the GUI can show
    // a load progress bar (the point of the feature). Init then unload to get an
    // on-disk repo to load through the HTTP route.
    let state = Arc::new(AppState::new());
    let app = routes::build(state.clone());
    let root = temp_dir("loadwarmup");
    let repo = state.init_repo(&root, None, None).unwrap().as_simple().to_string();
    let (st, _) = post(&app, &format!("/repos/{repo}/unload"), Value::Null).await;
    assert_eq!(st, StatusCode::OK);

    let (st, body) = post(&app, "/repos/load", serde_json::json!({ "root": root })).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["repo_uuid"], repo, "load returns the uuid immediately");

    // The warmup is observable as a `load` task that reaches `done` (terminal
    // tasks are retained for a while, so polling catches it even when quick).
    let mut saw_load = false;
    for _ in 0..50 {
        let (_, tasks) = request(&app, "GET", &format!("/repos/{repo}/tasks")).await;
        if let Some(t) = tasks.as_array().and_then(|a| a.iter().find(|t| t["kind"] == "load")) {
            saw_load = true;
            if t["status"] == "done" {
                return;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("did not observe a completed `load` task (saw one: {saw_load})");
}

#[tokio::test]
async fn unload_is_refused_while_a_load_warmup_is_active() {
    // A `load` warmup holds the connection; unloading mid-warmup would leave the
    // database locked with no reachable task to wait on, so it is refused (409)
    // until the warmup finishes. Seed an active load task directly (a real
    // warmup of a tiny repo finishes too fast to observe).
    let (app, state, repo) = app_with_repo("loadunload");
    let repo_uuid = Uuid::parse_str(&repo).unwrap();
    let id = state.repo(repo_uuid).unwrap().tasks.start(TaskKind::Load);
    state.repo(repo_uuid).unwrap().tasks.mark_running(id);

    let (status, _) = post(&app, &format!("/repos/{repo}/unload"), Value::Null).await;
    assert_eq!(status, StatusCode::CONFLICT, "unload must wait for the warmup");

    // Once the warmup completes, the unload succeeds.
    state.repo(repo_uuid).unwrap().tasks.finish(id, None);
    let (status, _) = post(&app, &format!("/repos/{repo}/unload"), Value::Null).await;
    assert_eq!(status, StatusCode::OK, "unload succeeds after the warmup finishes");
}

#[tokio::test]
async fn global_tasks_lists_across_repos() {
    let (app, state, repo) = app_with_repo("global");
    let repo_uuid = Uuid::parse_str(&repo).unwrap();
    let id = state.repo(repo_uuid).unwrap().tasks.start(TaskKind::Reconcile);

    let (status, body) = request(&app, "GET", "/tasks").await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = body.as_array().unwrap().iter().map(|t| t["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&id.as_simple().to_string().as_str()));
}
