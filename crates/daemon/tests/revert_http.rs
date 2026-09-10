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
