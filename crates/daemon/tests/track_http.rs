//! HTTP-level tests for `POST /reconcile`, `POST /metarecords` with a path
//! and the single-entry `POST /metadata/:uuid/reconcile`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use metafolder_daemon::routes;
use metafolder_daemon::state::AppState;
use serde_json::{json, Value};
use tower::util::ServiceExt;
use uuid::Uuid;

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
    let app = routes::build(std::sync::Arc::new(AppState::new()));
    let root = TempDir::new(&format!("thttp_{prefix}"));
    let (status, body) =
        request(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    let repo = body["repo_uuid"].as_str().unwrap().to_string();
    (app, repo, root)
}

async fn get_metarecord(app: &Router, repo: &str, uuid: &str) -> Value {
    let (status, body) =
        request(app, "GET", &format!("/repos/{repo}/metarecords/{uuid}"), None).await;
    assert_eq!(status, StatusCode::OK, "get failed: {body}");
    body
}

fn field<'a>(entry: &'a Value, name: &str) -> Option<&'a Value> {
    entry["fields"].as_array().unwrap().iter().find(|f| f["name"] == name).map(|f| &f["value"])
}

#[tokio::test]
async fn test_full_reconcile_endpoint() {
    let (app, repo, root) = setup("rec").await;
    // Enable tracking on the root.
    let (_, roots) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": {"type": "is_present", "field": "mf_watch"}})),
    )
    .await;
    let root_uuid = roots[0].as_str().unwrap();
    request(
        &app,
        "PUT",
        &format!("/repos/{repo}/metarecords/{root_uuid}/fields/mf_watch"),
        Some(json!({"value": {"type": "bool", "value": true}})),
    )
    .await;
    // The daemon writes no default mf_ignore any more (patterns come from the
    // client-side `default` preset); ignore .metafolder/ so this test isolates
    // the two files it creates.
    request(
        &app,
        "PUT",
        &format!("/repos/{repo}/metarecords/{root_uuid}/fields/mf_ignore"),
        Some(json!({"value": {"type": "string", "value": r"\.metafolder(/.*)?$"}})),
    )
    .await;

    std::fs::write(root.join("one.txt"), b"1").unwrap();
    std::fs::write(root.join("two.txt"), b"2").unwrap();
    // Reconcile is now asynchronous: 202 + task id, the result via the task.
    let (status, body) = request(&app, "POST", &format!("/repos/{repo}/reconcile"), None).await;
    assert_eq!(status, StatusCode::ACCEPTED, "reconcile start failed: {body}");
    let task_id = body["task_id"].as_str().unwrap().to_string();

    let task = poll_task(&app, &repo, &task_id).await;
    assert_eq!(task["status"], "done", "task: {task}");
    let result = &task["result"];
    assert_eq!(result["created"], 2, "one.txt + two.txt (.metafolder ignored via mf_ignore)");
    assert_eq!(result["moved"], 0);
    assert_eq!(result["candidates"], json!([]));

    std::fs::remove_dir_all(root).unwrap();
}

/// Polls a task until it is terminal, or panics after a generous timeout.
async fn poll_task(app: &axum::Router, repo: &str, task_id: &str) -> Value {
    for _ in 0..200 {
        let (status, body) =
            request(app, "GET", &format!("/repos/{repo}/tasks/{task_id}"), None).await;
        assert_eq!(status, StatusCode::OK, "task fetch failed: {body}");
        if body["status"] == "done" || body["status"] == "failed" {
            return body;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("task {task_id} did not finish in time");
}

#[tokio::test]
async fn test_single_metarecord_reconcile_endpoint() {
    let (app, repo, root) = setup("recone").await;
    std::fs::create_dir_all(root.join("music")).unwrap();
    std::fs::write(root.join("music/a.mp3"), b"aaa").unwrap();

    // Track the directory, then activate it directly.
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/metarecords"),
        Some(json!({"path": root.join("music").to_str().unwrap(), "fields": []})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create at path failed: {body}");
    let dir_uuid = body["uuid"].as_str().unwrap().to_string();
    request(
        &app,
        "PUT",
        &format!("/repos/{repo}/metarecords/{dir_uuid}/fields/mf_watch"),
        Some(json!({"value": {"type": "bool", "value": true}})),
    )
    .await;

    // Scoped reconcile via the unified endpoint (metarecord in the body):
    // 202 + task id, the result via the task.
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/reconcile"),
        Some(json!({"metarecord": dir_uuid})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "scoped reconcile start failed: {body}");
    let task = poll_task(&app, &repo, body["task_id"].as_str().unwrap()).await;
    assert_eq!(task["status"], "done", "task: {task}");
    assert_eq!(task["result"]["created"], 1, "a.mp3 gets an entry");
    assert_eq!(task["result"]["moved"], 0);

    // An unknown metarecord scope fails the task (was a synchronous 404).
    let bogus = Uuid::new_v4().as_simple().to_string();
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/reconcile"),
        Some(json!({"metarecord": bogus})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let task = poll_task(&app, &repo, body["task_id"].as_str().unwrap()).await;
    assert_eq!(task["status"], "failed");
    assert!(task["error"].as_str().unwrap().contains("not found"), "error: {}", task["error"]);

    // A metarecord scope without a valid path fails the task (was a 400).
    let (_, no_path) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/metarecords"),
        Some(json!({"fields": [{"name": "label", "value": {"type": "string", "value": "x"}}]})),
    )
    .await;
    let no_path_uuid = no_path["uuid"].as_str().unwrap();
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/reconcile"),
        Some(json!({"metarecord": no_path_uuid})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let task = poll_task(&app, &repo, body["task_id"].as_str().unwrap()).await;
    assert_eq!(task["status"], "failed");
    assert!(task["error"].as_str().unwrap().contains("mfr_path"), "error: {}", task["error"]);

    std::fs::remove_dir_all(root).unwrap();
}

/// `POST …/metarecords/:uuid/refresh` re-reads a record's file *now*, with the
/// watcher's semantics, and answers the record it left — so a client that has
/// just written a file can make the database agree with it before the
/// watcher's own event arrives (doc "Suppressing sync's echoes").
#[tokio::test]
async fn test_refresh_rereads_the_file_now() {
    let (app, repo, root) = setup("refresh").await;
    std::fs::write(root.join("f.txt"), b"one").unwrap();
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/metarecords"),
        Some(json!({"path": root.join("f.txt").to_str().unwrap(), "fields": []})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create at path failed: {body}");
    let uuid = body["uuid"].as_str().unwrap().to_string();
    // A hash the content change must invalidate.
    let (status, _) = request(
        &app,
        "PUT",
        &format!("/repos/{repo}/metarecords/{uuid}/fields/mfr_partial_hash"),
        Some(json!({"value": {"type": "string", "value": "abc"}, "force": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let before = get_metarecord(&app, &repo, &uuid).await;

    // New content, new size.
    std::fs::write(root.join("f.txt"), b"three").unwrap();
    let refresh = format!("/repos/{repo}/metarecords/{uuid}/refresh");
    let (status, after) = request(&app, "POST", &refresh, None).await;
    assert_eq!(status, StatusCode::OK, "refresh failed: {after}");
    assert_eq!(after["uuid"], uuid, "answers the record: {after}");
    assert_eq!(field(&after, "mfr_size").unwrap()["value"], 5);
    assert_eq!(field(&after, "mfr_partial_hash"), None, "a content change drops the hashes");
    assert_ne!(after["version"], before["version"]);

    // Nothing changed since: the same state, the same version — which is what
    // makes the watcher's echo of a write the client already refreshed a no-op.
    let (status, again) = request(&app, "POST", &refresh, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["version"], after["version"], "idempotent: {again}");

    // Fenced like every single-record write.
    let (status, body) =
        request(&app, "POST", &format!("{refresh}?expected_version=12345"), None).await;
    assert_eq!(status, StatusCode::CONFLICT, "stale expected_version: {body}");
    let (status, _) =
        request(&app, "POST", &format!("{refresh}?expected_version={}", after["version"]), None)
            .await;
    assert_eq!(status, StatusCode::OK);

    // Nothing on disk to read: refused, the record untouched.
    std::fs::remove_file(root.join("f.txt")).unwrap();
    let (status, body) = request(&app, "POST", &refresh, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "missing file: {body}");
    // A record with no path at all: refused too.
    let (_, abstract_record) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/metarecords"),
        Some(json!({"fields": [{"name": "label", "value": {"type": "string", "value": "x"}}]})),
    )
    .await;
    let (status, _) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/metarecords/{}/refresh", abstract_record["uuid"].as_str().unwrap()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// A record created bare — by a cross-repo sync, which writes the metadata
/// before the file exists — gets *every* stat field from its first refresh,
/// not only the size and mtime a watcher refresh updates.
#[tokio::test]
async fn test_refresh_fills_a_bare_record() {
    let (app, repo, root) = setup("refreshbare").await;
    let (_, roots) =
        request(&app, "GET", &format!("/repos/{repo}/tree/roots?field=mfr_path"), None).await;
    let root_uuid = roots[0]["uuid"].as_str().unwrap().to_string();
    let (status, created) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/metarecords"),
        Some(json!({"fields": [{"name": "mfr_path", "value": {"type": "tree_ref",
            "value": {"parent": root_uuid, "name": "new.txt"}}}], "force": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    std::fs::write(root.join("new.txt"), b"hello").unwrap();

    let (status, after) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/metarecords/{}/refresh", created["uuid"].as_str().unwrap()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{after}");
    assert_eq!(field(&after, "mfr_type").unwrap()["value"], "file");
    assert_eq!(field(&after, "mfr_size").unwrap()["value"], 5);
    assert!(field(&after, "mfr_mtime").is_some());
    #[cfg(unix)]
    assert!(field(&after, "mfr_permissions").is_some());
}

// ── POST /metarecords with a path (doc "Recording a path") ────────────────────

async fn create_at(app: &Router, repo: &str, body: Value) -> (StatusCode, Value) {
    request(app, "POST", &format!("/repos/{repo}/metarecords"), Some(body)).await
}

/// The metarecord at a repo-root-relative path, if any.
async fn uuid_at(app: &Router, repo: &str, rel: &str) -> Option<String> {
    let query =
        json!({"type": "eq", "field": "mfr_path", "value": {"type": "string", "value": rel}});
    let (status, body) = request(
        app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": query, "limit": 10})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "query failed: {body}");
    body["results"].as_array().unwrap().first().map(|r| r.as_str().unwrap().to_string())
}

/// A path whose parent is recorded becomes a metarecord with its stat fields,
/// `mf_watch = false` and the caller's own fields, in one revision.
#[tokio::test]
async fn test_create_at_path_records_the_path_not_watched() {
    let (app, repo, root) = setup("atpath").await;
    std::fs::write(root.join("song.flac"), b"music").unwrap();
    let abs = root.join("song.flac");

    let (status, body) = create_at(
        &app,
        &repo,
        json!({"path": abs, "fields": [{"name": "rating", "value": {"type": "int", "value": 4}}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create failed: {body}");
    let uuid = body["uuid"].as_str().unwrap().to_string();
    let entry = get_metarecord(&app, &repo, &uuid).await;
    assert_eq!(field(&entry, "mfr_size").unwrap()["value"], 5);
    assert_eq!(field(&entry, "mf_watch").unwrap()["value"], false);
    assert_eq!(field(&entry, "rating").unwrap()["value"], 4);
    assert_eq!(uuid_at(&app, &repo, "/song.flac").await.as_deref(), Some(uuid.as_str()));
}

/// A path that already has its metarecord is refused, naming it: `add` does not
/// quietly hand back what was there.
#[tokio::test]
async fn test_create_at_path_refuses_a_recorded_path() {
    let (app, repo, root) = setup("atpath-twice").await;
    std::fs::write(root.join("a.txt"), b"a").unwrap();
    let abs = root.join("a.txt");
    let (status, first) = create_at(&app, &repo, json!({"path": abs, "fields": []})).await;
    assert_eq!(status, StatusCode::OK, "{first}");

    let (status, body) = create_at(&app, &repo, json!({"path": abs, "fields": []})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["uuid"], first["uuid"], "the conflict names the existing metarecord");
    assert!(body["error"].as_str().unwrap().contains("/a.txt"), "{body}");
}

/// Missing parent directories are refused and listed, root first, unless the
/// caller asks for them; then they are created as plain scaffolding — no
/// `mf_watch` of their own, so a watched ancestor still covers them.
#[tokio::test]
async fn test_create_at_path_needs_parents_to_create_them() {
    let (app, repo, root) = setup("atpath-parents").await;
    std::fs::create_dir_all(root.join("docs/notes")).unwrap();
    std::fs::write(root.join("docs/notes/todo.txt"), b"todo").unwrap();
    let abs = root.join("docs/notes/todo.txt");

    let (status, body) = create_at(&app, &repo, json!({"path": abs, "fields": []})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["missing_parents"], json!(["/docs", "/docs/notes"]), "{body}");
    assert_eq!(uuid_at(&app, &repo, "/docs").await, None, "a refusal creates nothing");

    let (status, body) =
        create_at(&app, &repo, json!({"path": abs, "fields": [], "parents": true})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(field(&body, "mf_watch").unwrap()["value"], false, "the target is not watched");
    for dir in ["/docs", "/docs/notes"] {
        let uuid = uuid_at(&app, &repo, dir).await.expect("parent created");
        let parent = get_metarecord(&app, &repo, &uuid).await;
        assert_eq!(field(&parent, "mfr_type").unwrap()["value"], "dir");
        assert!(field(&parent, "mf_watch").is_none(), "{dir} inherits: {parent}");
    }
}

/// A caller's own `mf_watch` replaces the default: recording and watching a
/// directory is one request.
#[tokio::test]
async fn test_create_at_path_takes_the_callers_mf_watch() {
    let (app, repo, root) = setup("atpath-watch").await;
    std::fs::create_dir_all(root.join("music")).unwrap();
    let (status, body) = create_at(
        &app,
        &repo,
        json!({"path": root.join("music"),
               "fields": [{"name": "mf_watch", "value": {"type": "bool", "value": true}}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let watch: Vec<&Value> =
        body["fields"].as_array().unwrap().iter().filter(|f| f["name"] == "mf_watch").collect();
    assert_eq!(watch.len(), 1, "one mf_watch row: {body}");
    assert_eq!(watch[0]["value"]["value"], true);
}

/// What cannot be a path's metarecord is a 400: outside the root, the root
/// itself, a missing file, a caller-chosen uuid, an `mfr_path` of the caller's.
#[tokio::test]
async fn test_create_at_path_rejections() {
    let (app, repo, root) = setup("atpath-bad").await;
    std::fs::write(root.join("f"), b"f").unwrap();
    let f = root.join("f");
    for body in [
        json!({"path": "/etc/hostname", "fields": []}),
        json!({"path": root.to_str().unwrap(), "fields": []}),
        json!({"path": root.join("absent"), "fields": []}),
        json!({"path": f, "fields": [], "uuid": "0123456789abcdef0123456789abcdef"}),
        json!({"path": f, "force": true,
               "fields": [{"name": "mfr_path", "value": {"type": "string", "value": "x"}}]}),
    ] {
        let (status, resp) = create_at(&app, &repo, body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body} → {resp}");
    }
    // A reserved field still needs force, as on any create.
    let (status, resp) = create_at(
        &app,
        &repo,
        json!({"path": f, "fields": [{"name": "mfr_size", "value": {"type": "int", "value": 1}}]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{resp}");
    assert_eq!(uuid_at(&app, &repo, "/f").await, None, "nothing was created");
}
