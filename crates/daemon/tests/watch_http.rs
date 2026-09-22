//! HTTP-level tests for `GET /repos/:repo/watch` and its pause/resume pair
//! (spec-file-tracking "Watch status, pause and resume").

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

async fn init_repo(app: &Router, root: &TempDir) -> String {
    let (status, body) =
        request(app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    body["repo_uuid"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn watch_reports_running_and_pause_resume_flip_it() {
    let app = routes::build(std::sync::Arc::new(AppState::new()));
    let root = TempDir::new("watch_http");
    let repo = init_repo(&app, &root).await;

    let (status, body) = request(&app, "GET", &format!("/repos/{repo}/watch"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["paused"], json!(false), "a freshly loaded repository ingests");
    assert_eq!(body["pending_events"], json!(0));

    let (status, body) =
        request(&app, "POST", &format!("/repos/{repo}/watch/pause"), Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["paused"], json!(true));

    // Idempotent: pausing a paused repository is not an error.
    let (status, body) =
        request(&app, "POST", &format!("/repos/{repo}/watch/pause"), Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["paused"], json!(true));

    let (status, body) = request(&app, "GET", &format!("/repos/{repo}/watch"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["paused"], json!(true), "the pause is visible to a later reader");

    let (status, body) =
        request(&app, "POST", &format!("/repos/{repo}/watch/resume"), Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["paused"], json!(false));
}

#[tokio::test]
async fn watch_on_an_unknown_repository_is_404() {
    let app = routes::build(std::sync::Arc::new(AppState::new()));
    let unknown = uuid::Uuid::new_v4().as_simple().to_string();
    let (status, _) = request(&app, "GET", &format!("/repos/{unknown}/watch"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ── Watch check (spec-file-tracking "Watch check") ───────────────────────────

async fn check_paths(app: &Router, repo: &str, paths: Value) -> (StatusCode, Value) {
    request(app, "POST", &format!("/repos/{repo}/watch/check"), Some(paths)).await
}

/// The result for one requested path, in the order given.
fn result_for<'a>(body: &'a Value, path: &str) -> &'a Value {
    body["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["path"] == path)
        .unwrap_or_else(|| panic!("no result for {path}"))
}

/// The repository's filesystem root metarecord (the one carrying `mf_watch`).
async fn root_metarecord(app: &Router, repo: &str) -> String {
    let (_, roots) = request(
        app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": {"type": "is_present", "field": "mf_watch"}})),
    )
    .await;
    roots[0].as_str().expect("the filesystem root metarecord").to_string()
}

async fn put_field(app: &Router, repo: &str, uuid: &str, name: &str, value: Value) {
    let (status, body) = request(
        app,
        "PUT",
        &format!("/repos/{repo}/metarecords/{uuid}/fields/{name}"),
        Some(json!({ "value": value })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "setting {name} failed: {body}");
}

#[tokio::test]
async fn watch_check_on_a_fresh_repo_reports_nothing_watched() {
    // A fresh repository is opt-in: `mf_watch = false` on the root decides for
    // everything, and the check says so rather than blaming the budget.
    let app = routes::build(std::sync::Arc::new(AppState::new()));
    let root = TempDir::new("watch_check_fresh");
    let repo = init_repo(&app, &root).await;
    std::fs::write(root.join("a.txt"), b"x").unwrap();

    let (status, body) = check_paths(&app, &repo, json!({"paths": ["", "/a.txt"]})).await;
    assert_eq!(status, StatusCode::OK);
    for path in ["", "/a.txt"] {
        let r = result_for(&body, path);
        assert_eq!(r["watched"], json!(false), "{path}");
        assert_eq!(r["reason"], json!("untracked"), "{path}");
        assert_eq!(r["eligibility_reason"], json!("watch_false"), "{path}");
        assert_eq!(r["eligible"], json!(false), "{path}");
    }
}

#[tokio::test]
async fn watch_check_reports_the_live_watch_set() {
    let app = routes::build(std::sync::Arc::new(AppState::new()));
    let root = TempDir::new("watch_check_live");
    std::fs::create_dir_all(root.join("docs/notes")).unwrap();
    std::fs::write(root.join("docs/notes/todo.txt"), b"todo").unwrap();
    std::fs::create_dir_all(root.join("hidden")).unwrap();
    std::fs::write(root.join("hidden/x.txt"), b"x").unwrap();
    let repo = init_repo(&app, &root).await;
    let root_uuid = root_metarecord(&app, &repo).await;

    // Nothing is watched yet. Enabling tracking on the root re-places the
    // watches synchronously (the write touches mf_watch), so the check answers
    // from the live set at once.
    put_field(&app, &repo, &root_uuid, "mf_watch", json!({"type": "bool", "value": true})).await;

    // The root and an existing file are watched; the file is reported through
    // its containing directory's watch, the root through its own.
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/watch/check"),
        Some(json!({"paths": ["", "/docs/notes/todo.txt", "/docs"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let root_result = result_for(&body, "");
    assert_eq!(root_result["watched"], json!(true));
    assert_eq!(root_result["reason"], json!("watched"));
    assert_eq!(root_result["watched_dir"], json!(""));
    let file = result_for(&body, "/docs/notes/todo.txt");
    assert_eq!(file["watched"], json!(true));
    assert_eq!(file["watched_dir"], json!("/docs/notes"));
    let dir = result_for(&body, "/docs");
    assert_eq!(dir["watched"], json!(true));
    assert_eq!(dir["watched_dir"], json!("/docs"), "a directory is covered by its own watch");

    // A pattern matching a subtree: the file and its directory are untracked,
    // not merely unwatched, and the response says which pattern.
    let (status, body) = request(
        &app,
        "PUT",
        &format!("/repos/{repo}/metarecords/{root_uuid}/fields/mf_ignore"),
        Some(json!({"values": [{"type": "string", "value": r"hidden(/.*)?$"}]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "setting mf_ignore failed: {body}");
    let (_, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/watch/check"),
        Some(json!({"paths": ["/hidden/x.txt", "/hidden"]})),
    )
    .await;
    let file = result_for(&body, "/hidden/x.txt");
    assert_eq!(file["watched"], json!(false));
    assert_eq!(file["reason"], json!("untracked"));
    assert_eq!(file["eligibility_reason"], json!("ignored"));
    assert_eq!(file["pattern"], json!(r"hidden(/.*)?$"));

    // An exclusion (`mf watch exceeded set`): still tracked, no longer watched.
    // The subtree must be a *tracked* directory for the set to apply (the
    // endpoint refuses a path with no metarecord), so reconcile first.
    let (status, body) = request(&app, "POST", &format!("/repos/{repo}/reconcile"), None).await;
    assert_eq!(status, StatusCode::ACCEPTED, "reconcile start failed: {body}");
    let task_id = body["task_id"].as_str().unwrap().to_string();
    let mut task = Value::Null;
    for _ in 0..400 {
        let (_, body) = request(&app, "GET", &format!("/repos/{repo}/tasks/{task_id}"), None).await;
        if body["status"] == "done" || body["status"] == "failed" {
            task = body;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(task["status"], "done", "reconcile did not finish: {task}");
    let (status, _) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/watch/exceeded"),
        Some(json!({"path": "/docs", "exceeded": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/watch/check"),
        Some(json!({"paths": ["/docs/notes/todo.txt", "/docs"]})),
    )
    .await;
    let file = result_for(&body, "/docs/notes/todo.txt");
    assert_eq!(file["watched"], json!(false));
    assert_eq!(file["reason"], json!("excluded"));
    assert_eq!(file["excluded_by"], json!("/docs"));
    assert_eq!(file["eligible"], json!(true), "tracking is untouched by the exclusion");
    let dir = result_for(&body, "/docs");
    assert_eq!(dir["watched"], json!(false));
    assert_eq!(dir["reason"], json!("excluded"));

    // The daemon's own runtime directory is never watched, whatever the fields
    // say — the hard skip of the placement walk, not a decision to explain by
    // budget or pattern.
    let (_, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/watch/check"),
        Some(json!({"paths": ["/.metafolder/internal"]})),
    )
    .await;
    let internal = result_for(&body, "/.metafolder/internal");
    assert_eq!(internal["watched"], json!(false));
    assert_eq!(internal["reason"], json!("internal"));
}

#[tokio::test]
async fn watch_check_validates_its_input() {
    let app = routes::build(std::sync::Arc::new(AppState::new()));
    let root = TempDir::new("watch_check_validation");
    let repo = init_repo(&app, &root).await;

    // Paths are repo-root-relative with a leading slash ("" is the root).
    let (status, _) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/watch/check"),
        Some(json!({"paths": ["relative/path"]})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // A batch is a directory listing, not a bulk dump.
    let many: Vec<String> = (0..1001).map(|i| format!("/{i}")).collect();
    let (status, _) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/watch/check"),
        Some(json!({ "paths": many })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = request(
        &app,
        "POST",
        &format!("/repos/{}/watch/check", uuid::Uuid::new_v4().as_simple()),
        Some(json!({"paths": []})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
