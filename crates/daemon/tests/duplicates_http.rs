//! HTTP shape of the duplicate scan (doc "POST /repos/:repo/duplicates/scan"): 202 + task id, then
//! the summary on the
//! task — never a listing, since the groups are read back with a query.

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
    let app = routes::build(std::sync::Arc::new(AppState::new()));
    let root = TempDir::new(&format!("duphttp_{prefix}"));
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
    panic!("the scan task never reached a terminal state");
}

#[tokio::test]
async fn scan_returns_202_and_a_task_carrying_the_summary() {
    let (app, repo, root) = setup("summary").await;
    std::fs::write(root.join("a.txt"), b"identical bytes").unwrap();
    std::fs::write(root.join("b.txt"), b"identical bytes").unwrap();
    for name in ["a.txt", "b.txt"] {
        let abs = root.join(name);
        let (status, body) = request(
            &app,
            "POST",
            &format!("/repos/{repo}/metarecords"),
            Some(json!({"path": abs.to_str().unwrap(), "fields": []})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "create at path failed: {body}");
    }

    let (status, body) =
        request(&app, "POST", &format!("/repos/{repo}/duplicates/scan"), None).await;
    assert_eq!(status, StatusCode::ACCEPTED, "expected 202, got {body}");
    let task_id = body["task_id"].as_str().expect("a task id").to_string();

    let task = await_task(&app, &repo, &task_id).await;
    assert_eq!(task["status"], "done", "task: {task}");
    assert_eq!(task["kind"], "duplicates");
    let result = &task["result"];
    assert_eq!(result["groups"], 1, "result: {result}");
    assert_eq!(result["files"], 2);
    assert_eq!(result["reclaimable"], 15);
    // A summary, never a listing.
    assert!(result.get("uuids").is_none());
    assert!(result.get("groups_list").is_none());
}

#[tokio::test]
async fn the_groups_are_read_back_with_an_ordinary_query() {
    // The whole point of writing the result: no listing endpoint is needed.
    let (app, repo, root) = setup("query").await;
    for name in ["x.bin", "y.bin"] {
        std::fs::write(root.join(name), b"shared").unwrap();
        let abs = root.join(name);
        request(
            &app,
            "POST",
            &format!("/repos/{repo}/metarecords"),
            Some(json!({"path": abs.to_str().unwrap(), "fields": []})),
        )
        .await;
    }
    let (_, body) = request(&app, "POST", &format!("/repos/{repo}/duplicates/scan"), None).await;
    let task_id = body["task_id"].as_str().unwrap().to_string();
    await_task(&app, &repo, &task_id).await;

    let (status, groups) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": {"type": "eq", "field": "mf_schema",
                              "value": {"type": "string", "value": "duplicate_group"}}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "query failed: {groups}");
    assert_eq!(groups.as_array().map(Vec::len), Some(1), "one group: {groups}");

    let (status, files) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": {"type": "is_present", "field": "mfr_duplicate_group"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "query failed: {files}");
    assert_eq!(files.as_array().map(Vec::len), Some(2), "both files: {files}");
}

#[tokio::test]
async fn a_negative_min_size_is_rejected() {
    let (app, repo, _root) = setup("badmin").await;
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/duplicates/scan"),
        Some(json!({"min_size": -1})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "got {body}");
}

/// Makes `names` identical tracked files, scans, and returns the group's uuid
/// with the members' uuids in the order of `names`.
async fn scanned_group(
    app: &Router,
    repo: &str,
    root: &TempDir,
    names: &[&str],
) -> (String, Vec<String>) {
    let mut members = Vec::new();
    for name in names {
        let abs = root.join(name);
        std::fs::write(&abs, b"shared").unwrap();
        let (status, body) = request(
            app,
            "POST",
            &format!("/repos/{repo}/metarecords"),
            Some(json!({"path": abs.to_str().unwrap(), "fields": []})),
        )
        .await;
        assert!(status.is_success(), "create failed: {body}");
        members.push(body["uuid"].as_str().unwrap().to_string());
    }
    let (_, body) = request(app, "POST", &format!("/repos/{repo}/duplicates/scan"), None).await;
    await_task(app, repo, body["task_id"].as_str().unwrap()).await;
    let (_, groups) = request(
        app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": {"type": "eq", "field": "mf_schema",
                              "value": {"type": "string", "value": "duplicate_group"}}})),
    )
    .await;
    (groups[0].as_str().unwrap().to_string(), members)
}

#[tokio::test]
async fn trashing_a_member_recounts_its_group_at_once() {
    // The trash deletes the metarecord through the daemon before it moves the
    // bytes (doc "Trash"), so a panel reading the group right after the
    // trashing sees the new count — no watcher flush to wait for.
    let (app, repo, root) = setup("trash_recount").await;
    let (group, members) = scanned_group(&app, &repo, &root, &["a.bin", "b.bin", "c.bin"]).await;

    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/metarecords/trash"),
        Some(json!({"uuids": [members[0]], "force": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "trash failed: {body}");

    let (status, record) =
        request(&app, "GET", &format!("/repos/{repo}/metarecords/{group}"), None).await;
    assert_eq!(status, StatusCode::OK, "group read failed: {record}");
    let count = record["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "mfr_duplicate_count")
        .map(|f| f["value"]["value"].clone());
    assert_eq!(count, Some(json!(2)), "group: {record}");
}

#[tokio::test]
async fn trashing_one_of_two_members_dissolves_the_group_at_once() {
    let (app, repo, root) = setup("trash_dissolve").await;
    let (group, members) = scanned_group(&app, &repo, &root, &["a.bin", "b.bin"]).await;

    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/metarecords/trash"),
        Some(json!({"uuids": [members[0]], "force": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "trash failed: {body}");

    let (status, _) =
        request(&app, "GET", &format!("/repos/{repo}/metarecords/{group}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "the group should be gone");
}
