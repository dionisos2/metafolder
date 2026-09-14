//! HTTP-level tests for the revert endpoints (spec-event-log "Revert"):
//! the dependency check, its closure, and what a revert writes.

use std::sync::Arc;

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
    let app = routes::build(Arc::new(AppState::new()));
    let root = TempDir::new(&format!("revert_{prefix}"));
    let (status, body) =
        request(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    let repo = body["repo_uuid"].as_str().unwrap().to_string();
    (app, repo, root)
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

async fn set_field(app: &Router, repo: &str, uuid: &str, name: &str, value: Value) -> Value {
    let (status, body) = request(
        app,
        "PUT",
        &format!("/repos/{repo}/metarecords/{uuid}/fields/{name}"),
        Some(json!({"value": value})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "set field failed: {body}");
    body
}

async fn field_of(app: &Router, repo: &str, uuid: &str, name: &str) -> Option<Value> {
    let (status, body) =
        request(app, "GET", &format!("/repos/{repo}/metarecords/{uuid}"), None).await;
    assert_eq!(status, StatusCode::OK, "get record failed: {body}");
    body["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == name)
        .map(|f| f["value"].clone())
}

/// The id of the revision whose operations last touched `field` of `uuid`.
async fn last_revision(app: &Router, repo: &str) -> i64 {
    let (status, body) = request(app, "GET", &format!("/repos/{repo}/log"), None).await;
    assert_eq!(status, StatusCode::OK, "log failed: {body}");
    let head = body["head"].as_i64().unwrap();
    body["operations"].as_array().unwrap().iter().find(|o| o["id"].as_i64() == Some(head)).unwrap()
        ["rev_id"]
        .as_i64()
        .unwrap()
}

async fn revert(app: &Router, repo: &str, body: Value) -> (StatusCode, Value) {
    request(app, "POST", &format!("/repos/{repo}/revert"), Some(body)).await
}

async fn plan(app: &Router, repo: &str, query: &str) -> (StatusCode, Value) {
    request(app, "GET", &format!("/repos/{repo}/revert/plan?{query}"), None).await
}

// ── The plain case ────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_revert_restores_the_previous_value_without_moving_head() {
    let (app, repo, _root) = setup("plain").await;
    let uuid =
        create(&app, &repo, json!([{"name": "rating", "value": {"type": "int", "value": 3}}]))
            .await;
    set_field(&app, &repo, &uuid, "rating", json!({"type": "int", "value": 5})).await;
    let rev = last_revision(&app, &repo).await;

    let (status, body) = revert(&app, &repo, json!({"target": {"rev_id": rev}})).await;
    assert_eq!(status, StatusCode::OK, "revert failed: {body}");

    assert_eq!(
        field_of(&app, &repo, &uuid, "rating").await,
        Some(json!({"type": "int", "value": 3}))
    );
    // A revert is a new revision at HEAD, not a rewind: the log keeps both.
    assert!(body["revision"].as_i64().unwrap() > rev);
    assert_eq!(body["reverted_operations"].as_array().unwrap().len(), 1);

    // And it is itself revertable, restoring the original change.
    let rev2 = body["revision"].as_i64().unwrap();
    let (status, body) = revert(&app, &repo, json!({"target": {"rev_id": rev2}})).await;
    assert_eq!(status, StatusCode::OK, "revert of the revert failed: {body}");
    assert_eq!(
        field_of(&app, &repo, &uuid, "rating").await,
        Some(json!({"type": "int", "value": 5}))
    );
}

#[tokio::test]
async fn test_reverting_a_create_deletes_the_record() {
    let (app, repo, _root) = setup("create").await;
    let uuid =
        create(&app, &repo, json!([{"name": "a", "value": {"type": "int", "value": 1}}])).await;
    let rev = last_revision(&app, &repo).await;

    let (status, body) = revert(&app, &repo, json!({"target": {"rev_id": rev}})).await;
    assert_eq!(status, StatusCode::OK, "revert failed: {body}");

    let (status, _) =
        request(&app, "GET", &format!("/repos/{repo}/metarecords/{uuid}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "the record should be gone");
}

// ── The dependency check ──────────────────────────────────────────────────────

#[tokio::test]
async fn test_a_later_write_on_the_same_cell_blocks_the_revert() {
    let (app, repo, _root) = setup("blocked").await;
    let uuid =
        create(&app, &repo, json!([{"name": "rating", "value": {"type": "int", "value": 3}}]))
            .await;
    set_field(&app, &repo, &uuid, "rating", json!({"type": "int", "value": 5})).await;
    let rev = last_revision(&app, &repo).await;
    set_field(&app, &repo, &uuid, "rating", json!({"type": "int", "value": 7})).await;
    let blocker_rev = last_revision(&app, &repo).await;

    let (status, body) = revert(&app, &repo, json!({"target": {"rev_id": rev}})).await;
    assert_eq!(status, StatusCode::CONFLICT, "expected a blocked revert: {body}");
    let blocked = body["blocked"].as_array().expect("the body names the blockers");
    assert_eq!(blocked.len(), 1);
    assert_eq!(blocked[0]["rev_id"].as_i64().unwrap(), blocker_rev);
    // Nothing was written.
    assert_eq!(
        field_of(&app, &repo, &uuid, "rating").await,
        Some(json!({"type": "int", "value": 7}))
    );
}

#[tokio::test]
async fn test_a_write_on_another_cell_does_not_block() {
    let (app, repo, _root) = setup("independent").await;
    let uuid =
        create(&app, &repo, json!([{"name": "rating", "value": {"type": "int", "value": 3}}]))
            .await;
    set_field(&app, &repo, &uuid, "rating", json!({"type": "int", "value": 5})).await;
    let rev = last_revision(&app, &repo).await;
    // An unrelated cell, exactly like a watcher flush landing in between.
    set_field(&app, &repo, &uuid, "genre", json!({"type": "string", "value": "jazz"})).await;

    let (status, body) = revert(&app, &repo, json!({"target": {"rev_id": rev}})).await;
    assert_eq!(status, StatusCode::OK, "an independent write must not block: {body}");
    assert_eq!(
        field_of(&app, &repo, &uuid, "rating").await,
        Some(json!({"type": "int", "value": 3}))
    );
    // The independent write survives untouched.
    assert_eq!(
        field_of(&app, &repo, &uuid, "genre").await,
        Some(json!({"type": "string", "value": "jazz"}))
    );
}

#[tokio::test]
async fn test_an_entity_scoped_operation_blocks_every_cell_of_that_entity() {
    let (app, repo, _root) = setup("entity_scope").await;
    let uuid =
        create(&app, &repo, json!([{"name": "rating", "value": {"type": "int", "value": 3}}]))
            .await;
    set_field(&app, &repo, &uuid, "rating", json!({"type": "int", "value": 5})).await;
    let rev = last_revision(&app, &repo).await;
    // A whole-record overwrite carries no field name: it writes (entity, *).
    let (status, body) = request(
        &app,
        "PUT",
        &format!("/repos/{repo}/metarecords/{uuid}"),
        Some(json!({"fields": [{"name": "rating", "value": {"type": "int", "value": 9}}]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "set record failed: {body}");

    let (status, body) = revert(&app, &repo, json!({"target": {"rev_id": rev}})).await;
    assert_eq!(status, StatusCode::CONFLICT, "a whole-entity write must block: {body}");
}

// ── The closure ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_with_dependents_reverts_the_blockers_too() {
    let (app, repo, _root) = setup("closure").await;
    let uuid =
        create(&app, &repo, json!([{"name": "rating", "value": {"type": "int", "value": 3}}]))
            .await;
    set_field(&app, &repo, &uuid, "rating", json!({"type": "int", "value": 5})).await;
    let rev = last_revision(&app, &repo).await;
    set_field(&app, &repo, &uuid, "rating", json!({"type": "int", "value": 7})).await;

    // The plan reports the closure even when the flag was not passed, so a
    // client can offer "revert these as well" without a second round-trip.
    let (status, body) = plan(&app, &repo, &format!("target_rev_id={rev}")).await;
    assert_eq!(status, StatusCode::OK, "plan failed: {body}");
    assert_eq!(body["revertable"], json!(false));
    assert_eq!(body["dependents"].as_array().unwrap().len(), 1);

    let (status, body) =
        revert(&app, &repo, json!({"target": {"rev_id": rev}, "with_dependents": true})).await;
    assert_eq!(status, StatusCode::OK, "forced revert failed: {body}");
    assert_eq!(body["reverted_operations"].as_array().unwrap().len(), 2);
    assert_eq!(
        field_of(&app, &repo, &uuid, "rating").await,
        Some(json!({"type": "int", "value": 3}))
    );
}

#[tokio::test]
async fn test_the_closure_iterates_through_an_entity_scoped_blocker() {
    let (app, repo, _root) = setup("fixpoint").await;
    let uuid =
        create(&app, &repo, json!([{"name": "rating", "value": {"type": "int", "value": 3}}]))
            .await;
    set_field(&app, &repo, &uuid, "rating", json!({"type": "int", "value": 5})).await;
    let rev = last_revision(&app, &repo).await;
    // Blocks on `rating`, and widens the set's cells to the whole entity...
    let (status, _) = request(
        &app,
        "PUT",
        &format!("/repos/{repo}/metarecords/{uuid}"),
        Some(json!({"fields": [{"name": "rating", "value": {"type": "int", "value": 9}}]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // ...so this one, on a different cell, only becomes a blocker on the
    // second round of the fixpoint.
    set_field(&app, &repo, &uuid, "genre", json!({"type": "string", "value": "jazz"})).await;

    let (status, body) =
        plan(&app, &repo, &format!("target_rev_id={rev}&with_dependents=true")).await;
    assert_eq!(status, StatusCode::OK, "plan failed: {body}");
    let ops = body["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 3, "the closure must reach the second-round blocker: {body}");
    assert_eq!(ops.iter().filter(|o| o["origin"] == "requested").count(), 1);

    let (status, body) =
        revert(&app, &repo, json!({"target": {"rev_id": rev}, "with_dependents": true})).await;
    assert_eq!(status, StatusCode::OK, "forced revert failed: {body}");
    assert_eq!(
        field_of(&app, &repo, &uuid, "rating").await,
        Some(json!({"type": "int", "value": 3}))
    );
    assert_eq!(field_of(&app, &repo, &uuid, "genre").await, None, "the genre write is undone too");
}

// ── Targets ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_reverting_a_single_operation_of_a_revision() {
    let (app, repo, _root) = setup("op_target").await;
    let a = create(&app, &repo, json!([{"name": "x", "value": {"type": "int", "value": 1}}])).await;
    let b = create(&app, &repo, json!([{"name": "x", "value": {"type": "int", "value": 1}}])).await;
    // One revision writing the same field of two records: two operations.
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query/fields/set"),
        Some(json!({
            "query": {"type": "uuid_in", "uuids": [a, b]},
            "name": "x",
            "value": {"type": "int", "value": 9}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "batch set failed: {body}");

    let (status, log) = request(&app, "GET", &format!("/repos/{repo}/log"), None).await;
    assert_eq!(status, StatusCode::OK);
    let op_a = log["operations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["op_type"] == "set_field" && o["entity_uuid"] == json!(a))
        .expect("an operation for record a")["id"]
        .as_i64()
        .unwrap();

    let (status, body) = revert(&app, &repo, json!({"target": {"op_ids": [op_a]}})).await;
    assert_eq!(status, StatusCode::OK, "op-level revert failed: {body}");
    assert_eq!(
        field_of(&app, &repo, &a, "x").await,
        Some(json!({"type": "int", "value": 1})),
        "a is undone"
    );
    assert_eq!(
        field_of(&app, &repo, &b, "x").await,
        Some(json!({"type": "int", "value": 9})),
        "b, written by the same revision, is untouched"
    );
}

#[tokio::test]
async fn test_unknown_revision_is_not_found() {
    let (app, repo, _root) = setup("missing").await;
    let (status, _) = revert(&app, &repo, json!({"target": {"rev_id": 9999}})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ── The coordinated form ──────────────────────────────────────────────────────

async fn start(app: &Router, repo: &str, body: Value) -> (StatusCode, Value) {
    request(app, "POST", &format!("/repos/{repo}/revert/start"), Some(body)).await
}

#[tokio::test]
async fn test_the_lock_suspends_writes_until_commit_or_abort() {
    let (app, repo, _root) = setup("lock").await;
    let uuid =
        create(&app, &repo, json!([{"name": "rating", "value": {"type": "int", "value": 3}}]))
            .await;
    set_field(&app, &repo, &uuid, "rating", json!({"type": "int", "value": 5})).await;
    let rev = last_revision(&app, &repo).await;

    let (status, plan) = start(&app, &repo, json!({"target": {"rev_id": rev}})).await;
    assert_eq!(status, StatusCode::OK, "start failed: {plan}");
    assert_eq!(plan["operations"].as_array().unwrap().len(), 1);

    // While the lock is held the repository refuses metadata writes.
    let (status, _) = request(
        &app,
        "PUT",
        &format!("/repos/{repo}/metarecords/{uuid}/fields/genre"),
        Some(json!({"value": {"type": "string", "value": "jazz"}})),
    )
    .await;
    assert_eq!(status, StatusCode::LOCKED, "writes are refused under the lock");

    // A second coordinated operation is refused too.
    let (status, _) = start(&app, &repo, json!({"target": {"rev_id": rev}})).await;
    assert_eq!(status, StatusCode::CONFLICT);

    let op = plan["operations"][0]["id"].as_i64().unwrap();
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/revert/commit"),
        Some(json!({"apply": [op]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "commit failed: {body}");
    assert_eq!(
        field_of(&app, &repo, &uuid, "rating").await,
        Some(json!({"type": "int", "value": 3}))
    );

    // And the lock is gone: writes work again.
    set_field(&app, &repo, &uuid, "genre", json!({"type": "string", "value": "jazz"})).await;
}

#[tokio::test]
async fn test_abort_releases_the_lock_and_writes_nothing() {
    let (app, repo, _root) = setup("abort").await;
    let uuid =
        create(&app, &repo, json!([{"name": "rating", "value": {"type": "int", "value": 3}}]))
            .await;
    set_field(&app, &repo, &uuid, "rating", json!({"type": "int", "value": 5})).await;
    let rev = last_revision(&app, &repo).await;

    let (status, _) = start(&app, &repo, json!({"target": {"rev_id": rev}})).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) =
        request(&app, "POST", &format!("/repos/{repo}/revert/abort"), Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "abort failed: {body}");

    assert_eq!(
        field_of(&app, &repo, &uuid, "rating").await,
        Some(json!({"type": "int", "value": 5})),
        "an aborted revert writes nothing"
    );
    // Aborting twice is a conflict, not a silent success.
    let (status, _) =
        request(&app, "POST", &format!("/repos/{repo}/revert/abort"), Some(json!({}))).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn test_commit_may_narrow_the_set_but_not_widen_it() {
    let (app, repo, _root) = setup("narrow").await;
    let a = create(&app, &repo, json!([{"name": "x", "value": {"type": "int", "value": 1}}])).await;
    let b = create(&app, &repo, json!([{"name": "x", "value": {"type": "int", "value": 1}}])).await;
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query/fields/set"),
        Some(json!({
            "query": {"type": "uuid_in", "uuids": [a, b]},
            "name": "x",
            "value": {"type": "int", "value": 9}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "batch set failed: {body}");
    let rev = last_revision(&app, &repo).await;

    let (status, plan) = start(&app, &repo, json!({"target": {"rev_id": rev}})).await;
    assert_eq!(status, StatusCode::OK, "start failed: {plan}");
    let ops = plan["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 2);
    let first = ops[0]["id"].as_i64().unwrap();

    // An id the lock was not started for is a usage error.
    let (status, _) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/revert/commit"),
        Some(json!({"apply": [first, 999999]})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "commit may only narrow");

    // Narrowing to one is fine, and the other is reported as left out.
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/revert/commit"),
        Some(json!({"apply": [first]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "commit failed: {body}");
    assert_eq!(body["reverted_operations"].as_array().unwrap().len(), 1);
    let skipped = body["skipped_operations"].as_array().unwrap();
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0]["reason"], "client_skipped");
}

/// A real file move, so the plan carries a `move` action with the two paths the
/// client has to `mv` between.
#[tokio::test]
async fn test_reverting_a_file_move_plans_the_mv() {
    let (app, repo, root) = setup("fs_move").await;
    // Track the root, ignoring `.metafolder/` so only our file is seen.
    let (_, roots) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": {"type": "is_present", "field": "mf_watch"}})),
    )
    .await;
    let root_uuid = roots[0].as_str().unwrap().to_string();
    request(
        &app,
        "PUT",
        &format!("/repos/{repo}/metarecords/{root_uuid}/fields/mf_ignore"),
        Some(json!({"value": {"type": "string", "value": r"\.metafolder(/.*)?$"}})),
    )
    .await;
    request(
        &app,
        "PUT",
        &format!("/repos/{repo}/metarecords/{root_uuid}/fields/mf_watch"),
        Some(json!({"value": {"type": "bool", "value": true}})),
    )
    .await;

    std::fs::write(root.join("old.txt"), b"content").unwrap();
    reconcile(&app, &repo).await;
    // The watcher is running (mf_watch is set), so the rename reaches the log
    // through it, after the executor's quiet period.
    std::fs::rename(root.join("old.txt"), root.join("new.txt")).unwrap();
    wait_for_file_moved(&app, &repo).await;

    // Reverting that move must plan the mv back.
    let (status, log) = request(&app, "GET", &format!("/repos/{repo}/log"), None).await;
    assert_eq!(status, StatusCode::OK);
    let moved = log["operations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["op_type"] == "file_moved")
        .unwrap_or_else(|| panic!("no file_moved in: {}", log["operations"]))
        .clone();
    let op_id = moved["id"].as_i64().unwrap();

    let (status, plan) = start(&app, &repo, json!({"target": {"op_ids": [op_id]}})).await;
    assert_eq!(status, StatusCode::OK, "start failed: {plan}");
    let action = &plan["operations"][0]["filesystem"];
    assert_eq!(action["action"], "move");
    assert!(
        action["from"].as_str().unwrap().ends_with("new.txt"),
        "from is where the file is now: {action}"
    );
    assert!(
        action["to"].as_str().unwrap().ends_with("old.txt"),
        "to is where it goes back: {action}"
    );

    // The client does the mv, then commits.
    std::fs::rename(root.join("new.txt"), root.join("old.txt")).unwrap();
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/revert/commit"),
        Some(json!({"apply": [op_id]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "commit failed: {body}");

    // The metadata followed the file, and it is recorded as a move.
    let (status, log) = request(&app, "GET", &format!("/repos/{repo}/log"), None).await;
    assert_eq!(status, StatusCode::OK);
    let head = log["head"].as_i64().unwrap();
    let written = log["operations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["id"].as_i64() == Some(head))
        .unwrap();
    assert_eq!(written["op_type"], "file_moved", "not set_field: navigation keys on this");
}

/// Waits for the watcher's flush to record the rename.
async fn wait_for_file_moved(app: &Router, repo: &str) {
    for _ in 0..100 {
        let (status, log) = request(app, "GET", &format!("/repos/{repo}/log"), None).await;
        assert_eq!(status, StatusCode::OK, "log failed: {log}");
        if log["operations"].as_array().unwrap().iter().any(|o| o["op_type"] == "file_moved") {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("the watcher never recorded the rename");
}

async fn reconcile(app: &Router, repo: &str) {
    let (status, body) = request(app, "POST", &format!("/repos/{repo}/reconcile"), None).await;
    assert_eq!(status, StatusCode::ACCEPTED, "reconcile start failed: {body}");
    let task_id = body["task_id"].as_str().unwrap().to_string();
    for _ in 0..200 {
        let (status, task) =
            request(app, "GET", &format!("/repos/{repo}/tasks/{task_id}"), None).await;
        assert_eq!(status, StatusCode::OK, "task fetch failed: {task}");
        if task["status"] == "done" {
            return;
        }
        assert_ne!(task["status"], "failed", "reconcile failed: {task}");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("reconcile did not finish in time");
}

// ── What a revert leaves behind (spec-event-log "reverts_op_id") ──────────────

/// Every operation a revert writes names the operation it undid, so a later
/// reader — the undo selection above all — can tell a correction from a change
/// and see which changes are already undone.
#[tokio::test]
async fn test_a_revert_names_the_operations_it_undid() {
    let (app, repo, _root) = setup("reverts_op_id").await;
    let uuid =
        create(&app, &repo, json!([{"name": "rating", "value": {"type": "int", "value": 3}}]))
            .await;
    set_field(&app, &repo, &uuid, "rating", json!({"type": "int", "value": 5})).await;
    let rev = last_revision(&app, &repo).await;
    let (status, body) = request(&app, "GET", &format!("/repos/{repo}/log"), None).await;
    assert_eq!(status, StatusCode::OK, "log failed: {body}");
    let undone: Vec<i64> = body["operations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|o| o["rev_id"].as_i64() == Some(rev))
        .map(|o| o["id"].as_i64().unwrap())
        .collect();
    assert_eq!(undone.len(), 1, "the setup revision should hold one operation");

    let (status, body) = revert(&app, &repo, json!({"target": {"rev_id": rev}})).await;
    assert_eq!(status, StatusCode::OK, "revert failed: {body}");
    let revert_rev = body["revision"].as_i64().unwrap();

    let (_, body) = request(&app, "GET", &format!("/repos/{repo}/log"), None).await;
    let written: Vec<&Value> = body["operations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|o| o["rev_id"].as_i64() == Some(revert_rev))
        .collect();
    assert_eq!(written.len(), 1, "the revert should write one operation");
    assert_eq!(written[0]["reverts_op_id"].as_i64(), Some(undone[0]));

    // An ordinary write names nothing.
    for op in body["operations"].as_array().unwrap() {
        if op["rev_id"].as_i64() != Some(revert_rev) {
            assert_eq!(op["reverts_op_id"], Value::Null, "op {op} should name nothing");
        }
    }
}
