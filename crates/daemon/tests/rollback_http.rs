//! HTTP-level tests for the v2 coordinated-navigation protocol
//! (spec-event-log "Coordinated navigation"): plan/summary, the
//! start/step/abort lock cycle, and the 423-Locked write guard.

use std::sync::Arc;

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
    let app = routes::build(Arc::new(AppState::new()));
    let root = TempDir::new(&format!("rbk_{prefix}"));
    let (status, body) =
        request(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    (app, body["repo_uuid"].as_str().unwrap().to_string(), root)
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

async fn set(app: &Router, repo: &str, uuid: &str, name: &str, value: Value) {
    let (status, body) = request(
        app,
        "PUT",
        &format!("/repos/{repo}/metarecords/{uuid}/fields/{name}"),
        Some(json!({"value": value})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "set failed: {body}");
}

async fn rating(app: &Router, repo: &str, uuid: &str) -> Option<i64> {
    let (_, body) = request(app, "GET", &format!("/repos/{repo}/metarecords/{uuid}"), None).await;
    body["fields"]
        .as_array()?
        .iter()
        .find(|f| f["name"] == "rating")
        .and_then(|f| f["value"]["value"].as_i64())
}

async fn head(app: &Router, repo: &str) -> Option<i64> {
    let (_, body) = request(app, "GET", &format!("/repos/{repo}/log"), None).await;
    body["head"].as_i64()
}

#[tokio::test]
async fn test_plan_lists_the_operations_to_undo() {
    let (app, repo, _root) = setup("plan").await;
    let uuid =
        create(&app, &repo, json!([{"name": "rating", "value": {"type": "int", "value": 3}}]))
            .await;
    set(&app, &repo, &uuid, "rating", json!({"type": "int", "value": 5})).await;

    let (status, body) = request(
        &app,
        "GET",
        &format!("/repos/{repo}/rollback/plan?target_prev_revision=true"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], 1, "{body}");
    assert_eq!(body["operations"][0]["op_type"], "set_field");
    // A pure-metadata op carries no from/to.
    assert!(body["operations"][0]["from"].is_null());
}

#[tokio::test]
async fn test_plan_summary_counts_by_type() {
    let (app, repo, _root) = setup("summary").await;
    let uuid =
        create(&app, &repo, json!([{"name": "rating", "value": {"type": "int", "value": 3}}]))
            .await;
    set(&app, &repo, &uuid, "rating", json!({"type": "int", "value": 5})).await;
    set(&app, &repo, &uuid, "rating", json!({"type": "int", "value": 7})).await;

    // Undo back to just after the create.
    let create_op = create_op_id(&app, &repo, &uuid).await;
    let (status, body) = request(
        &app,
        "GET",
        &format!("/repos/{repo}/rollback/plan/summary?target_id={create_op}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total_operations"], 2, "{body}");
    assert_eq!(body["by_type"]["set_field"], 2, "{body}");
}

/// The id of the create_metarecord operation for `uuid`.
async fn create_op_id(app: &Router, repo: &str, uuid: &str) -> i64 {
    let (_, body) = request(app, "GET", &format!("/repos/{repo}/log?mode=tree"), None).await;
    body["operations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["op_type"] == "create_metarecord" && o["entity_uuid"] == *uuid)
        .and_then(|o| o["id"].as_i64())
        .expect("create op")
}

#[tokio::test]
async fn test_start_step_undoes_last_revision() {
    let (app, repo, _root) = setup("startstep").await;
    let uuid =
        create(&app, &repo, json!([{"name": "rating", "value": {"type": "int", "value": 3}}]))
            .await;
    set(&app, &repo, &uuid, "rating", json!({"type": "int", "value": 5})).await;
    assert_eq!(rating(&app, &repo, &uuid).await, Some(5));

    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/rollback/start"),
        Some(json!({"target": {"prev_revision": true}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["op"]["op_type"], "set_field", "{body}");
    // Inverse steps carry the metarecord version this step restores to, so the
    // CLI can correlate a trashed file with the exact deletion it undoes
    // (spec-trash "rollback auto-restore").
    assert!(body["op"]["entity_version_before"].is_u64(), "{body}");

    let (status, body) =
        request(&app, "POST", &format!("/repos/{repo}/rollback/step"), Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["op"].is_null(), "navigation should be complete: {body}");

    // The set was undone; the value is back to the create-time value.
    assert_eq!(rating(&app, &repo, &uuid).await, Some(3));
}

// An orphaning revision writes several fields of one record (`mfr_path` and
// `mfr_path_old`), so each operation restores to its own version.
// `entity_version_before_revision` is the record's version before the *whole*
// revision — the version an external side-effect recorded when it last observed
// the record (a trash entry's version, spec-trash "rollback auto-restore") —
// and is identical on every operation of the revision.
#[tokio::test]
async fn test_inverse_step_exposes_the_pre_revision_version() {
    use metafolder_daemon::executor::{self, FsEvent};

    let state = Arc::new(AppState::new());
    let app = routes::build(state.clone());
    let root = TempDir::new("rbk_prerev");
    std::fs::write(root.join("doc.txt"), b"precious").unwrap();
    let (status, body) =
        request(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    let repo = body["repo_uuid"].as_str().unwrap().to_string();
    let repo_state = state.repo(Uuid::parse_str(&repo).unwrap()).unwrap();

    // Tracking is opt-in: watch the root so the events are eligible.
    let root_uuid = {
        let conn = repo_state.conn.lock().unwrap();
        metafolder_daemon::db::find_tree_child(&conn, "mfr_path", None, "").unwrap().unwrap()
    };
    set(
        &app,
        &repo,
        &root_uuid.as_simple().to_string(),
        "mf_watch",
        json!({"type": "bool", "value": true}),
    )
    .await;

    // Track the file, then orphan it — one revision, two operations.
    executor::enqueue(&repo_state, FsEvent::Create("/doc.txt".into()), None);
    executor::flush_pending(&repo_state).unwrap();
    let file_uuid = {
        let conn = repo_state.conn.lock().unwrap();
        let mut cache = repo_state.cache.lock().unwrap();
        cache.resolve_path(&conn, "mfr_path", "/doc.txt").unwrap().expect("the tracked file")
    };
    let uuid = file_uuid.as_simple().to_string();
    let before = {
        let conn = repo_state.conn.lock().unwrap();
        metafolder_daemon::db::get_version(&conn, file_uuid).unwrap().expect("version")
    };

    std::fs::remove_file(root.join("doc.txt")).unwrap();
    executor::enqueue(&repo_state, FsEvent::Remove("/doc.txt".into()), None);
    executor::flush_pending(&repo_state).unwrap();

    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/rollback/start"),
        Some(json!({"target": {"prev_revision": true}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["op"]["entity_uuid"], uuid, "{body}");
    // The last op of the revision is navigated first: it restores to a version
    // *inside* the revision, while the pre-revision version is the record's
    // state before any of it.
    assert!(
        body["op"]["entity_version_before"].as_u64().unwrap() > before,
        "the first inverse step restores to an intra-revision version: {body}"
    );
    assert_eq!(
        body["op"]["entity_version_before_revision"].as_u64(),
        Some(before),
        "every op of the revision exposes the pre-revision version: {body}"
    );

    let (status, body) =
        request(&app, "POST", &format!("/repos/{repo}/rollback/step"), Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["op"]["entity_version_before"].as_u64(),
        Some(before),
        "the second op restores to the pre-revision version: {body}"
    );
    assert_eq!(
        body["op"]["entity_version_before_revision"].as_u64(),
        Some(before),
        "same pre-revision version on the second op: {body}"
    );
}

#[tokio::test]
async fn test_lock_blocks_writes_but_allows_reads() {
    let (app, repo, _root) = setup("lock").await;
    let uuid =
        create(&app, &repo, json!([{"name": "rating", "value": {"type": "int", "value": 3}}]))
            .await;
    set(&app, &repo, &uuid, "rating", json!({"type": "int", "value": 5})).await;

    let (status, _) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/rollback/start"),
        Some(json!({"target": {"prev_revision": true}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // A metadata write is rejected with 423 Locked.
    let (status, _) = request(
        &app,
        "PUT",
        &format!("/repos/{repo}/metarecords/{uuid}/fields/rating"),
        Some(json!({"value": {"type": "int", "value": 9}})),
    )
    .await;
    assert_eq!(status, StatusCode::LOCKED, "writes must be locked");

    // Reads still work.
    let (status, _) =
        request(&app, "GET", &format!("/repos/{repo}/metarecords/{uuid}"), None).await;
    assert_eq!(status, StatusCode::OK);

    // Starting again is a conflict.
    let (status, _) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/rollback/start"),
        Some(json!({"target": {"prev_revision": true}})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Finishing releases the lock; writes work again.
    let (status, _) =
        request(&app, "POST", &format!("/repos/{repo}/rollback/step"), Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = request(
        &app,
        "PUT",
        &format!("/repos/{repo}/metarecords/{uuid}/fields/rating"),
        Some(json!({"value": {"type": "int", "value": 9}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "lock should be released");
}

#[tokio::test]
async fn test_abort_keeps_head_and_releases_lock() {
    let (app, repo, _root) = setup("abort").await;
    let uuid =
        create(&app, &repo, json!([{"name": "rating", "value": {"type": "int", "value": 3}}]))
            .await;
    set(&app, &repo, &uuid, "rating", json!({"type": "int", "value": 5})).await;
    let before = head(&app, &repo).await;

    let (status, _) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/rollback/start"),
        Some(json!({"target": {"prev_revision": true}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) =
        request(&app, "POST", &format!("/repos/{repo}/rollback/abort"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["head"].as_i64(), before, "abort must not move HEAD");
    // No metadata change happened; the value is unchanged.
    assert_eq!(rating(&app, &repo, &uuid).await, Some(5));

    // Lock released: writes work.
    set(&app, &repo, &uuid, "rating", json!({"type": "int", "value": 9})).await;
}

#[tokio::test]
async fn test_start_at_head_is_a_noop_without_lock() {
    let (app, repo, _root) = setup("noop").await;
    let uuid =
        create(&app, &repo, json!([{"name": "rating", "value": {"type": "int", "value": 3}}]))
            .await;
    let head_op = head(&app, &repo).await.unwrap();

    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/rollback/start"),
        Some(json!({"target": {"id": head_op}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["op"].is_null());
    assert_eq!(body["remaining"], 0);

    // No lock was taken: a write succeeds immediately.
    set(&app, &repo, &uuid, "rating", json!({"type": "int", "value": 5})).await;
}

#[tokio::test]
async fn test_redo_navigates_forward_to_a_descendant() {
    let (app, repo, _root) = setup("redo").await;
    let uuid =
        create(&app, &repo, json!([{"name": "rating", "value": {"type": "int", "value": 3}}]))
            .await;
    set(&app, &repo, &uuid, "rating", json!({"type": "int", "value": 5})).await;

    // The set_field operation id, used as a forward (redo) target.
    let (_, log) = request(&app, "GET", &format!("/repos/{repo}/log?mode=tree"), None).await;
    let set_op = log["operations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["op_type"] == "set_field" && o["entity_uuid"] == uuid)
        .and_then(|o| o["id"].as_i64())
        .unwrap();

    // Undo the set, then redo it forward.
    for body in [json!({"target": {"prev_revision": true}}), json!({"target": {"id": set_op}})] {
        let (status, start) =
            request(&app, "POST", &format!("/repos/{repo}/rollback/start"), Some(body)).await;
        assert_eq!(status, StatusCode::OK, "{start}");
        if start["op"].is_null() {
            continue;
        }
        loop {
            let (status, step) =
                request(&app, "POST", &format!("/repos/{repo}/rollback/step"), Some(json!({})))
                    .await;
            assert_eq!(status, StatusCode::OK, "{step}");
            if step["op"].is_null() {
                break;
            }
        }
    }
    assert_eq!(rating(&app, &repo, &uuid).await, Some(5), "redo should reapply the set");
}

#[tokio::test]
async fn test_step_without_start_is_a_conflict() {
    let (app, repo, _root) = setup("nostart").await;
    create(&app, &repo, json!([{"name": "rating", "value": {"type": "int", "value": 3}}])).await;
    let (status, _) =
        request(&app, "POST", &format!("/repos/{repo}/rollback/step"), Some(json!({}))).await;
    assert_eq!(status, StatusCode::CONFLICT);
}
