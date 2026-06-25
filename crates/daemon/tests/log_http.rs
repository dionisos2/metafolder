//! HTTP-level tests for the event log endpoints and the atomic rollback
//! (spec-event-log): history reading, labels, navigation, pruning.

use std::path::PathBuf;
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

async fn request(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
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

async fn setup(prefix: &str) -> (Router, String, PathBuf) {
    let app = routes::build(Arc::new(AppState::new()));
    let root = std::env::temp_dir().join(format!("metafolder_log_{prefix}_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let (status, body) =
        request(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    let repo = body["repo_uuid"].as_str().unwrap().to_string();
    (app, repo, root)
}

async fn create(app: &Router, repo: &str, fields: Value) -> Value {
    let (status, body) =
        request(app, "POST", &format!("/repos/{repo}/metarecords"), Some(json!({"fields": fields})))
            .await;
    assert_eq!(status, StatusCode::OK, "create failed: {body}");
    body
}

async fn patch(app: &Router, repo: &str, uuid: &str, name: &str, value: Value) -> Value {
    let (status, body) = request(
        app,
        "PATCH",
        &format!("/repos/{repo}/metarecords/{uuid}"),
        Some(json!({"name": name, "value": value})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "patch failed: {body}");
    body
}

async fn get_log(app: &Router, repo: &str, params: &str) -> Value {
    let (status, body) = request(app, "GET", &format!("/repos/{repo}/log{params}"), None).await;
    assert_eq!(status, StatusCode::OK, "log failed: {body}");
    body
}

async fn field_of(app: &Router, repo: &str, uuid: &str, name: &str) -> Option<Value> {
    let (status, body) =
        request(app, "GET", &format!("/repos/{repo}/metarecords/{uuid}"), None).await;
    if status != StatusCode::OK {
        return None;
    }
    body["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == name)
        .map(|f| f["value"].clone())
}

// ── Reading the log ───────────────────────────────────────────────────────────

#[tokio::test]
async fn test_log_linear_and_filters() {
    let (app, repo, root) = setup("read").await;
    let entry = create(&app, &repo, json!([{"name": "a", "value": {"type": "int", "value": 1}}]))
        .await;
    let uuid = entry["uuid"].as_str().unwrap();
    patch(&app, &repo, uuid, "a", json!({"type": "int", "value": 2})).await;

    let log = get_log(&app, &repo, "").await;
    let head = log["head"].as_i64().expect("head must be set");
    let ops = log["operations"].as_array().unwrap();
    // Root entry creation + our create + our set_field.
    assert_eq!(ops.len(), 3);
    assert_eq!(ops.last().unwrap()["id"].as_i64().unwrap(), head);
    assert_eq!(ops[0]["parent_id"], Value::Null);
    assert_eq!(ops[2]["op_type"], "set_field");
    assert_eq!(ops[2]["field_name"], "a");
    assert!(!log["revisions"].as_array().unwrap().is_empty());
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

#[tokio::test]
async fn test_log_active_line_through_head() {
    let (app, repo, root) = setup("active").await;
    let entry = create(&app, &repo, json!([{"name": "s", "value": {"type": "int", "value": 1}}]))
        .await;
    let uuid = entry["uuid"].as_str().unwrap().to_string();
    patch(&app, &repo, &uuid, "s", json!({"type": "int", "value": 2})).await;
    patch(&app, &repo, &uuid, "s", json!({"type": "int", "value": 3})).await;

    // Helpers reading op ids out of a `/log` body.
    let ids = |log: &Value| -> Vec<i64> {
        log["operations"].as_array().unwrap().iter().map(|o| o["id"].as_i64().unwrap()).collect()
    };

    // op_b is the most recent write (s=3); op_a is its parent (s=2).
    let tree = get_log(&app, &repo, "?mode=tree").await;
    let op_b = *ids(&tree).iter().max().unwrap();
    let op_a = tree["operations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["id"].as_i64() == Some(op_b))
        .unwrap()["parent_id"]
        .as_i64()
        .unwrap();

    // Rollback to op_a: op_b becomes the redo "future", a descendant of HEAD.
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/rollback"),
        Some(json!({"target": {"id": op_a}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "rollback failed: {body}");

    // active keeps the forward continuation; linear (ancestry only) drops it.
    let active = get_log(&app, &repo, "?mode=active").await;
    let linear = get_log(&app, &repo, "").await;
    assert!(ids(&active).contains(&op_b), "active keeps the redo future: {active}");
    assert!(!ids(&linear).contains(&op_b), "linear ancestry excludes the future");

    // A new write from HEAD=op_a creates a divergent branch (op_c); HEAD=op_c.
    patch(&app, &repo, &uuid, "s", json!({"type": "int", "value": 99})).await;
    let tree2 = get_log(&app, &repo, "?mode=tree").await;
    let op_c = *ids(&tree2).iter().max().unwrap();
    let active2 = get_log(&app, &repo, "?mode=active").await;
    assert!(ids(&active2).contains(&op_c), "active follows the branch to HEAD's leaf");
    assert!(!ids(&active2).contains(&op_b), "active hides the divergent branch: {active2}");
    assert!(ids(&tree2).contains(&op_b), "tree still shows every branch");

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_revision_detail_and_label() {
    let (app, repo, root) = setup("rev").await;
    let entry = create(&app, &repo, json!([{"name": "x", "value": {"type": "int", "value": 1}}]))
        .await;
    let _ = entry;

    // "head" targets the revision containing the current HEAD.
    let (status, detail) =
        request(&app, "GET", &format!("/repos/{repo}/log/revisions/head"), None).await;
    assert_eq!(status, StatusCode::OK, "got: {detail}");
    assert_eq!(detail["revision"]["is_head"], true);
    let rev_id = detail["revision"]["id"].as_i64().unwrap();
    let ops = detail["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 1);
    assert!(ops[0]["snapshots_after"].is_array(), "snapshots always included here");

    // Set and clear a label.
    let (status, _) = request(
        &app,
        "PATCH",
        &format!("/repos/{repo}/log/revisions/{rev_id}"),
        Some(json!({"label": "before-cleanup"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, detail) =
        request(&app, "GET", &format!("/repos/{repo}/log/revisions/{rev_id}"), None).await;
    assert_eq!(detail["revision"]["label"], "before-cleanup");

    // 404 on a missing revision.
    let (status, _) =
        request(&app, "GET", &format!("/repos/{repo}/log/revisions/99999"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    std::fs::remove_dir_all(root).unwrap();
}

// ── Rollback ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_rollback_by_id_and_redo() {
    let (app, repo, root) = setup("roll").await;
    let entry = create(&app, &repo, json!([{"name": "n", "value": {"type": "int", "value": 1}}]))
        .await;
    let uuid = entry["uuid"].as_str().unwrap().to_string();
    let v1 = patch(&app, &repo, &uuid, "n", json!({"type": "int", "value": 2})).await;
    assert_eq!(v1["version"], 1);

    let log = get_log(&app, &repo, "").await;
    let head_before = log["head"].as_i64().unwrap();
    let create_op = log["operations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["op_type"] == "create_metarecord" && o["entity_uuid"] == json!(uuid))
        .unwrap()["id"]
        .as_i64()
        .unwrap();

    // Undo the set_field by navigating to the create operation.
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/rollback"),
        Some(json!({"target": {"id": create_op}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "rollback failed: {body}");
    assert_eq!(body["previous_head"].as_i64().unwrap(), head_before);
    assert_eq!(body["new_head"].as_i64().unwrap(), create_op);
    assert_eq!(body["operations_unapplied"], 1);
    assert_eq!(body["operations_applied"], 0);

    let (_, after) = request(&app, "GET", &format!("/repos/{repo}/metarecords/{uuid}"), None).await;
    assert_eq!(after["fields"][0]["value"]["value"], 1, "old value restored");
    assert_eq!(after["version"], 0, "version restored exactly");

    // Redo: navigate forward to the previous head.
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/rollback"),
        Some(json!({"target": {"id": head_before}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "redo failed: {body}");
    assert_eq!(body["operations_applied"], 1);
    let (_, redone) = request(&app, "GET", &format!("/repos/{repo}/metarecords/{uuid}"), None).await;
    assert_eq!(redone["fields"][0]["value"]["value"], 2);
    assert_eq!(redone["version"], 1);

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_rollback_prev_revision_undoes_batch() {
    let (app, repo, root) = setup("batch").await;
    for v in [1, 2] {
        create(&app, &repo, json!([{"name": "g", "value": {"type": "int", "value": v}}])).await;
    }
    // One batch revision touching both entries.
    let (status, _) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/set"),
        Some(json!({
            "query": {"type": "is_present", "field": "g"},
            "name": "seen",
            "value": {"type": "bool", "value": true}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/rollback"),
        Some(json!({"target": {"prev_revision": true}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "rollback failed: {body}");
    assert_eq!(body["operations_unapplied"], 2, "the whole batch revision is undone");

    let (_, seen) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": {"type": "is_present", "field": "seen"}})),
    )
    .await;
    assert_eq!(seen.as_array().unwrap().len(), 0);

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_rollback_restores_deleted_record_with_field_ids() {
    let (app, repo, root) = setup("undelete").await;
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
    let original_ids: Vec<i64> = entry["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["id"].as_i64().unwrap())
        .collect();
    let version_before = entry["version"].as_u64().unwrap();

    let (status, _) =
        request(&app, "DELETE", &format!("/repos/{repo}/metarecords/{uuid}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/rollback"),
        Some(json!({"target": {"prev_revision": true}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "rollback failed: {body}");

    let (status, restored) =
        request(&app, "GET", &format!("/repos/{repo}/metarecords/{uuid}"), None).await;
    assert_eq!(status, StatusCode::OK, "entry must be restored");
    let restored_ids: Vec<i64> = restored["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["id"].as_i64().unwrap())
        .collect();
    assert_eq!(restored_ids, original_ids, "field ids restored exactly");
    assert_eq!(restored["version"].as_u64().unwrap(), version_before);

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_set_record_is_one_op_and_rolls_back_exactly() {
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
    let original_ids: Vec<i64> =
        entry["fields"].as_array().unwrap().iter().map(|f| f["id"].as_i64().unwrap()).collect();
    let version_before = entry["version"].as_u64().unwrap();

    let revisions = |body: &Value| body["revisions"].as_array().unwrap().len();
    let (_, log_before) = request(&app, "GET", &format!("/repos/{repo}/log"), None).await;
    let revs_before = revisions(&log_before);

    // Whole-record set: a totally different field set, one revision.
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
    assert_eq!(after["version"].as_u64().unwrap(), version_before + 1);

    // Exactly one new revision, whose single operation is set_metarecord.
    let (_, log_after) = request(&app, "GET", &format!("/repos/{repo}/log"), None).await;
    assert_eq!(revisions(&log_after) - revs_before, 1, "set_record must be one revision");
    let ops = log_after["operations"].as_array().unwrap();
    assert_eq!(ops.last().unwrap()["op_type"].as_str().unwrap(), "set_metarecord");

    // Rollback restores the prior field set, with the original ids and version.
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/rollback"),
        Some(json!({"target": {"prev_revision": true}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "rollback failed: {body}");
    let (_, restored) =
        request(&app, "GET", &format!("/repos/{repo}/metarecords/{uuid}"), None).await;
    let restored_ids: Vec<i64> =
        restored["fields"].as_array().unwrap().iter().map(|f| f["id"].as_i64().unwrap()).collect();
    assert_eq!(restored_ids, original_ids, "field ids restored exactly");
    assert_eq!(restored["version"].as_u64().unwrap(), version_before);
    let names: Vec<&str> =
        restored["fields"].as_array().unwrap().iter().map(|f| f["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["a", "b"]);

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_rollback_by_label_and_branching() {
    let (app, repo, root) = setup("label").await;
    let entry = create(&app, &repo, json!([{"name": "s", "value": {"type": "int", "value": 1}}]))
        .await;
    let uuid = entry["uuid"].as_str().unwrap().to_string();

    // Label the current revision, then keep writing.
    let (_, detail) =
        request(&app, "GET", &format!("/repos/{repo}/log/revisions/head"), None).await;
    let rev = detail["revision"]["id"].as_i64().unwrap();
    request(
        &app,
        "PATCH",
        &format!("/repos/{repo}/log/revisions/{rev}"),
        Some(json!({"label": "checkpoint"})),
    )
    .await;
    patch(&app, &repo, &uuid, "s", json!({"type": "int", "value": 2})).await;
    patch(&app, &repo, &uuid, "s", json!({"type": "int", "value": 3})).await;

    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/rollback"),
        Some(json!({"target": {"label": "checkpoint"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "rollback failed: {body}");
    assert_eq!(field_of(&app, &repo, &uuid, "s").await.unwrap()["value"], 1);

    // A new write creates a branch; tree mode still shows the old ops.
    patch(&app, &repo, &uuid, "s", json!({"type": "int", "value": 99})).await;
    let tree = get_log(&app, &repo, "?mode=tree").await;
    let linear = get_log(&app, &repo, "").await;
    assert!(
        tree["operations"].as_array().unwrap().len()
            > linear["operations"].as_array().unwrap().len(),
        "tree mode includes the abandoned branch"
    );

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_rollback_to_empty_state() {
    let (app, repo, root) = setup("empty").await;
    // Navigate before the very first operation (the root entry creation).
    let log = get_log(&app, &repo, "").await;
    let first = log["operations"].as_array().unwrap()[0]["id"].as_i64().unwrap();
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/rollback"),
        Some(json!({"target": {"id": first}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "got {body}");
    // Now undo that single create too.
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/rollback"),
        Some(json!({"target": {"prev_revision": true}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "got {body}");
    assert_eq!(body["new_head"], Value::Null);

    let (_, all) = request(&app, "GET", &format!("/repos/{repo}/metarecords"), None).await;
    assert_eq!(all.as_array().unwrap().len(), 0, "empty state");

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_rollback_unknown_target_is_404() {
    let (app, repo, root) = setup("badtarget").await;
    let (status, _) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/rollback"),
        Some(json!({"target": {"id": 424242}})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    std::fs::remove_dir_all(root).unwrap();
}

// ── Pruning ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_prune_before_makes_weak_root() {
    let (app, repo, root) = setup("prune").await;
    let entry = create(&app, &repo, json!([{"name": "p", "value": {"type": "int", "value": 1}}]))
        .await;
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
        Some(json!({"mode": "before", "target": {"id": target}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "prune failed: {body}");
    assert_eq!(body["pruned_operations"], 1, "the root-entry creation op is pruned");

    let log = get_log(&app, &repo, "").await;
    let ops = log["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 2);
    assert_eq!(ops[0]["id"].as_i64().unwrap(), target);
    assert_eq!(ops[0]["parent_id"], Value::Null, "the target became the (weak) root");

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_prune_linearize_removes_branches() {
    let (app, repo, root) = setup("linearize").await;
    let entry = create(&app, &repo, json!([{"name": "q", "value": {"type": "int", "value": 1}}]))
        .await;
    let uuid = entry["uuid"].as_str().unwrap().to_string();
    patch(&app, &repo, &uuid, "q", json!({"type": "int", "value": 2})).await;
    // Create a branch: roll back then write something else.
    request(
        &app,
        "POST",
        &format!("/repos/{repo}/rollback"),
        Some(json!({"target": {"prev_revision": true}})),
    )
    .await;
    patch(&app, &repo, &uuid, "q", json!({"type": "int", "value": 3})).await;

    let tree_before = get_log(&app, &repo, "?mode=tree").await;
    let n_before = tree_before["operations"].as_array().unwrap().len();
    let head = tree_before["head"].as_i64().unwrap();

    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/log/prune"),
        Some(json!({"mode": "linearize", "target": {"id": head}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "linearize failed: {body}");
    assert_eq!(body["pruned_operations"], 1, "the abandoned set_field branch is pruned");

    let tree_after = get_log(&app, &repo, "?mode=tree").await;
    assert_eq!(tree_after["operations"].as_array().unwrap().len(), n_before - 1);

    // Pruning with a non-ancestor target is rejected.
    let (status, _) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/log/prune"),
        Some(json!({"mode": "before", "target": {"id": 424242}})),
    )
    .await;
    assert_ne!(status, StatusCode::OK);

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
    let m2 = create(&app, &repo, json!([{"name": "b", "value": {"type": "string", "value": "2"}}])).await;
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
}
