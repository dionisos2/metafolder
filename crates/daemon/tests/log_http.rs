//! HTTP-level tests for the event log endpoints (doc "Event log"): history
//! reading, labels, pruning, the change feed.

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
    let root = TempDir::new(&format!("log_{prefix}"));
    common::seed_config(&root);
    let (status, body) =
        request(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    let repo = body["repo_uuid"].as_str().unwrap().to_string();
    (app, repo, root)
}

async fn create(app: &Router, repo: &str, fields: Value) -> Value {
    let (status, body) = request(
        app,
        "POST",
        &format!("/repos/{repo}/metarecords"),
        Some(json!({"fields": fields})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create failed: {body}");
    body
}

async fn patch(app: &Router, repo: &str, uuid: &str, name: &str, value: Value) -> Value {
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

async fn get_log(app: &Router, repo: &str, params: &str) -> Value {
    let (status, body) = request(app, "GET", &format!("/repos/{repo}/log{params}"), None).await;
    assert_eq!(status, StatusCode::OK, "log failed: {body}");
    body
}

// ── Reading the log ───────────────────────────────────────────────────────────

#[tokio::test]
async fn test_log_and_filters() {
    let (app, repo, root) = setup("read").await;
    let entry =
        create(&app, &repo, json!([{"name": "a", "value": {"type": "int", "value": 1}}])).await;
    let uuid = entry["uuid"].as_str().unwrap();
    patch(&app, &repo, uuid, "a", json!({"type": "int", "value": 2})).await;

    let log = get_log(&app, &repo, "").await;
    let head = log["head"].as_i64().expect("head must be set");
    let ops = log["operations"].as_array().unwrap();
    // Root entry creation + our create + our set_field.
    assert_eq!(ops.len(), 3);
    assert_eq!(ops.last().unwrap()["id"].as_i64().unwrap(), head);
    assert!(ops.windows(2).all(|w| w[0]["id"].as_i64() < w[1]["id"].as_i64()), "oldest first");
    assert!(ops[0].get("parent_id").is_none(), "the log is a sequence: no parent");
    // The log's first operation, where undo stops (doc "The undo selection").
    assert_eq!(log["first"], ops[0]["id"]);
    let limited = get_log(&app, &repo, "?limit=1").await;
    assert_eq!(limited["first"], ops[0]["id"], "whatever the window");
    assert_eq!(ops[2]["op_type"], "set_field");
    assert_eq!(ops[2]["field_name"], "a");
    assert!(!log["labels"].as_array().unwrap().is_empty());
    assert!(ops[0].get("snapshots_before").is_none(), "snapshots off by default");

    // Filter by entry.
    let filtered = get_log(&app, &repo, &format!("?metarecord_uuid={uuid}")).await;
    assert_eq!(filtered["operations"].as_array().unwrap().len(), 2);

    // Limit keeps the most recent.
    let limited = get_log(&app, &repo, "?limit=1").await;
    let limited_ops = limited["operations"].as_array().unwrap();
    assert_eq!(limited_ops.len(), 1);
    assert_eq!(limited_ops[0]["id"].as_i64().unwrap(), head);

    // include_snapshots.
    let with_snaps = get_log(&app, &repo, "?include_snapshots=true").await;
    let last = with_snaps["operations"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["snapshots_before"][0]["value_int"], 1);
    assert_eq!(last["snapshots_after"][0]["value_int"], 2);

    std::fs::remove_dir_all(root).unwrap();
}

/// The log response carries the repository-wide totals, so a client showing a
/// bounded window (the GUI log panel fetches the most recent `limit`
/// operations) can still say how much log there is. The totals count the whole
/// log, not the returned window, and are unaffected by `limit`.
#[tokio::test]
async fn test_log_reports_repository_wide_totals() {
    let (app, repo, root) = setup("totals").await;
    let entry =
        create(&app, &repo, json!([{"name": "a", "value": {"type": "int", "value": 1}}])).await;
    let uuid = entry["uuid"].as_str().unwrap();
    for n in 2..6 {
        patch(&app, &repo, uuid, "a", json!({"type": "int", "value": n})).await;
    }

    let full = get_log(&app, &repo, "").await;
    let operations = full["operations"].as_array().unwrap().len() as i64;
    let revisions = full["labels"].as_array().unwrap().len() as i64;
    assert_eq!(full["total_operations"], operations);
    assert_eq!(full["total_labels"], revisions);

    // A bounded window still reports the whole log's size.
    let limited = get_log(&app, &repo, "?limit=2").await;
    assert_eq!(limited["operations"].as_array().unwrap().len(), 2);
    assert_eq!(limited["total_operations"], operations);
    assert_eq!(limited["total_labels"], revisions);

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_label_detail() {
    let (app, repo, root) = setup("rev").await;
    let entry =
        create(&app, &repo, json!([{"name": "x", "value": {"type": "int", "value": 1}}])).await;
    let _ = entry;

    // "head" targets the label of the newest operation.
    let (status, detail) =
        request(&app, "GET", &format!("/repos/{repo}/log/labels/head"), None).await;
    assert_eq!(status, StatusCode::OK, "got: {detail}");
    assert_eq!(detail["label"]["is_head"], true);
    let ops = detail["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 1);
    assert!(ops[0]["snapshots_after"].is_array(), "snapshots always included here");

    // 404 on a missing label.
    let (status, _) = request(&app, "GET", &format!("/repos/{repo}/log/labels/99999"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    std::fs::remove_dir_all(root).unwrap();
}

// A write can hold every operation of a reconcile — hundreds of thousands,
// each with its snapshots. The detail view pages through them, or asks for the
// one operation the user selected, instead of pulling the whole write.
#[tokio::test]
async fn test_revision_detail_pages_its_operations() {
    let (app, repo, _root) = setup("rev_page").await;
    for v in [1, 2, 3] {
        create(&app, &repo, json!([{"name": "g", "value": {"type": "int", "value": v}}])).await;
    }
    // One write, three operations.
    let (status, _) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query/fields/set"),
        Some(json!({
            "query": {"type": "is_present", "field": "g"},
            "name": "seen",
            "value": {"type": "bool", "value": true}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (_, whole) = request(&app, "GET", &format!("/repos/{repo}/log/labels/head"), None).await;
    let all: Vec<i64> =
        whole["operations"].as_array().unwrap().iter().map(|o| o["id"].as_i64().unwrap()).collect();
    assert_eq!(all.len(), 3);
    assert_eq!(whole["label"]["operation_count"], 3);

    // A window: offset + limit, the count still the whole write's.
    let (status, page) =
        request(&app, "GET", &format!("/repos/{repo}/log/labels/head?offset=1&limit=1"), None)
            .await;
    assert_eq!(status, StatusCode::OK, "got: {page}");
    let ids: Vec<i64> =
        page["operations"].as_array().unwrap().iter().map(|o| o["id"].as_i64().unwrap()).collect();
    assert_eq!(ids, vec![all[1]]);
    assert_eq!(page["label"]["operation_count"], 3);
    assert_eq!(page["label"]["is_head"], true, "HEAD is judged on the whole revision");

    // One operation by id, with its snapshots.
    let (status, one) =
        request(&app, "GET", &format!("/repos/{repo}/log/labels/head?op={}", all[2]), None).await;
    assert_eq!(status, StatusCode::OK, "got: {one}");
    let ops = one["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0]["id"].as_i64().unwrap(), all[2]);
    assert!(ops[0]["snapshots_after"].is_array());

    // An operation of another write is not this write's.
    let (status, _) =
        request(&app, "GET", &format!("/repos/{repo}/log/labels/head?op=1"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ── Whole-record writes ───────────────────────────────────────────────────────

#[tokio::test]
async fn test_set_record_is_one_op_and_reverts_exactly() {
    let (app, repo, root) = setup("setrecord").await;
    let entry = create(
        &app,
        &repo,
        json!([
            {"name": "a", "value": {"type": "int", "value": 1}},
            {"name": "b", "value": {"type": "string", "value": "keep"}}
        ]),
    )
    .await;
    let uuid = entry["uuid"].as_str().unwrap().to_string();
    let version_before = entry["version"].as_u64().unwrap();

    let revisions = |body: &Value| body["labels"].as_array().unwrap().len();
    let (_, log_before) = request(&app, "GET", &format!("/repos/{repo}/log"), None).await;
    let revs_before = revisions(&log_before);

    // Whole-record set: a totally different field set, one write.
    let (status, after) = request(
        &app,
        "PUT",
        &format!("/repos/{repo}/metarecords/{uuid}"),
        Some(json!({"fields": [{"name": "c", "value": {"type": "string", "value": "new"}}]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "set_record failed: {after}");
    let new_fields = after["fields"].as_array().unwrap();
    assert_eq!(new_fields.len(), 1);
    assert_eq!(new_fields[0]["name"].as_str().unwrap(), "c");
    assert_ne!(after["version"].as_u64().unwrap(), version_before, "the write moves the version");

    // Exactly one new write, whose single operation is set_metarecord.
    let (_, log_after) = request(&app, "GET", &format!("/repos/{repo}/log"), None).await;
    assert_eq!(revisions(&log_after) - revs_before, 1, "set_record must be one revision");
    let ops = log_after["operations"].as_array().unwrap();
    assert_eq!(ops.last().unwrap()["op_type"].as_str().unwrap(), "set_metarecord");

    // Reverting it restores the prior field set — and its version, which is
    // the content's (doc "Metarecord version").
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/revert"),
        Some(json!({"target": {"label_id": "head"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "revert failed: {body}");
    let (_, restored) =
        request(&app, "GET", &format!("/repos/{repo}/metarecords/{uuid}"), None).await;
    assert_eq!(restored["version"].as_u64().unwrap(), version_before);
    let names: Vec<&str> = restored["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["a", "b"]);

    std::fs::remove_dir_all(root).unwrap();
}

// ── Pruning ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_prune_before_drops_the_older_operations() {
    let (app, repo, root) = setup("prune").await;
    let entry =
        create(&app, &repo, json!([{"name": "p", "value": {"type": "int", "value": 1}}])).await;
    let uuid = entry["uuid"].as_str().unwrap().to_string();
    patch(&app, &repo, &uuid, "p", json!({"type": "int", "value": 2})).await;

    let log = get_log(&app, &repo, "").await;
    let ops = log["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 3);
    let target = ops[1]["id"].as_i64().unwrap(); // our create_metarecord

    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/log/prune"),
        Some(json!({"target": {"id": target}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "prune failed: {body}");
    assert_eq!(body["pruned_operations"], 1, "the root-entry creation op is pruned");

    let log = get_log(&app, &repo, "").await;
    let ops = log["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 2);
    assert_eq!(ops[0]["id"].as_i64().unwrap(), target, "the target is kept");

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_prune_to_an_unknown_target_is_refused() {
    let (app, repo, root) = setup("prune_unknown").await;
    let (status, _) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/log/prune"),
        Some(json!({"target": {"id": 424242}})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    std::fs::remove_dir_all(root).unwrap();
}

// ── GET /log/since (cache change feed) ─────────────────────────────────────

#[tokio::test]
async fn test_log_since_reports_head_and_delta() {
    let (app, repo, _root) = setup("since").await;
    create(&app, &repo, json!([{"name": "a", "value": {"type": "string", "value": "1"}}])).await;

    // No `op` given: just the current head, empty delta (baseline fetch).
    let (status, base) = request(&app, "GET", &format!("/repos/{repo}/log/since"), None).await;
    assert_eq!(status, StatusCode::OK);
    let h0 = base["head"].as_i64().unwrap();
    assert!(base["operations"].as_array().unwrap().is_empty());

    // Nothing changed since h0.
    let (_, same) = request(&app, "GET", &format!("/repos/{repo}/log/since?op={h0}"), None).await;
    assert_eq!(same["head"].as_i64().unwrap(), h0);
    assert!(same["operations"].as_array().unwrap().is_empty());

    // A second write touches a new metarecord.
    let m2 = create(&app, &repo, json!([{"name": "b", "value": {"type": "string", "value": "2"}}]))
        .await;
    let uuid2 = m2["uuid"].as_str().unwrap();

    let (_, delta) = request(&app, "GET", &format!("/repos/{repo}/log/since?op={h0}"), None).await;
    let h1 = delta["head"].as_i64().unwrap();
    assert!(h1 > h0, "head should advance: {h0} -> {h1}");
    let ops = delta["operations"].as_array().unwrap();
    assert!(!ops.is_empty(), "delta should contain the new ops");
    // Every returned op is newer than the requested op, across all branches.
    assert!(ops.iter().all(|o| o["id"].as_i64().unwrap() > h0));
    // The delta names the changed metarecord (so the cache can invalidate it).
    assert!(ops.iter().any(|o| o["entity_uuid"] == uuid2));
    // A small delta is not truncated.
    assert_eq!(delta["truncated"], json!(false));
}

// A large delta (e.g. after a reconcile of thousands of files) must NOT be
// streamed op-by-op — that floods the GUI. The daemon caps it and signals
// truncation so the client does one coarse refresh instead.
#[tokio::test]
async fn test_log_since_truncates_oversized_delta() {
    let (app, repo, _root) = setup("since_trunc").await;
    // Baseline head before the burst.
    let (_, base) = request(&app, "GET", &format!("/repos/{repo}/log/since"), None).await;
    let h0 = base["head"].as_i64().unwrap();

    // Five writes, each a separate operation.
    for i in 0..5 {
        create(&app, &repo, json!([{"name": "n", "value": {"type": "int", "value": i}}])).await;
    }

    // With a limit below the delta size, the response is truncated: head still
    // advances, but no operations are carried.
    let (status, capped) =
        request(&app, "GET", &format!("/repos/{repo}/log/since?op={h0}&limit=2"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(capped["head"].as_i64().unwrap() > h0);
    assert_eq!(capped["truncated"], json!(true));
    assert!(capped["operations"].as_array().unwrap().is_empty());

    // With a generous limit, the full delta comes through untruncated.
    let (_, full) =
        request(&app, "GET", &format!("/repos/{repo}/log/since?op={h0}&limit=100"), None).await;
    assert_eq!(full["truncated"], json!(false));
    assert!(full["operations"].as_array().unwrap().len() >= 5);
}
