//! Log contexts and contexts over HTTP (doc "Log contexts"): creating a
//! log context, reading it with its ancestors, writing inside one through the
//! `X-Metafolder-Log-Context` header, and a write with no context opening its own.

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

async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    log_context: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(log_context) = log_context {
        builder = builder.header("x-metafolder-log-context", log_context);
    }
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
    let value =
        if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap() };
    (status, value)
}

async fn setup(prefix: &str) -> (Router, String, TempDir) {
    let app = routes::build(Arc::new(AppState::new(common::settings())));
    let root = TempDir::new(&format!("labels_{prefix}"));
    common::seed_config(&root);
    let (status, body) =
        send(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()})), None)
            .await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    let repo = body["repo_uuid"].as_str().unwrap().to_string();
    (app, repo, root)
}

async fn new_log_context(app: &Router, repo: &str, body: Value) -> i64 {
    let (status, log_context) =
        send(app, "POST", &format!("/repos/{repo}/contexts"), Some(body), None).await;
    assert_eq!(status, StatusCode::OK, "log context creation failed: {log_context}");
    log_context["id"].as_i64().expect("a log context id")
}

async fn create(app: &Router, repo: &str, log_context: Option<&str>) -> (StatusCode, Value) {
    let fields = json!([{"name": "a", "value": {"type": "int", "value": 1}}]);
    send(
        app,
        "POST",
        &format!("/repos/{repo}/metarecords"),
        Some(json!({"fields": fields})),
        log_context,
    )
    .await
}

async fn newest_op(app: &Router, repo: &str) -> Value {
    let (status, log) = send(app, "GET", &format!("/repos/{repo}/log?limit=1"), None, None).await;
    assert_eq!(status, StatusCode::OK, "{log}");
    log["operations"][0].clone()
}

#[tokio::test]
async fn a_log_context_is_read_back_with_its_ancestors() {
    let (app, repo, _root) = setup("ancestors").await;
    let run =
        new_log_context(&app, &repo, json!({"name": "tag run", "description": "all photos"})).await;
    let answer = new_log_context(&app, &repo, json!({"name": "answer 1", "parent": run})).await;

    let (status, log_context) =
        send(&app, "GET", &format!("/repos/{repo}/contexts/{answer}"), None, None).await;
    assert_eq!(status, StatusCode::OK, "{log_context}");
    assert_eq!(log_context["name"], "answer 1");
    assert_eq!(log_context["parent"], run);
    assert!(log_context["created"].as_i64().is_some(), "{log_context}");
    let ancestors = log_context["ancestors"].as_array().unwrap();
    assert_eq!(ancestors.len(), 1, "{log_context}");
    assert_eq!(ancestors[0]["id"], run);
    assert_eq!(ancestors[0]["description"], "all photos");

    let (status, _) = send(&app, "GET", &format!("/repos/{repo}/contexts/99999"), None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_log_context_under_an_unknown_parent_is_refused() {
    let (app, repo, _root) = setup("unknown_parent").await;
    let (status, body) = send(
        &app,
        "POST",
        &format!("/repos/{repo}/contexts"),
        Some(json!({"name": "x", "parent": 99999})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn a_write_inside_a_context_carries_its_log_context() {
    let (app, repo, _root) = setup("context").await;
    let run = new_log_context(&app, &repo, json!({"name": "tag run"})).await;
    let answer = new_log_context(&app, &repo, json!({"name": "answer 1", "parent": run})).await;

    let (status, body) = create(&app, &repo, Some(&answer.to_string())).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let op = newest_op(&app, &repo).await;
    assert_eq!(op["log_context_id"], answer, "{op}");

    // The listing names the log context and every ancestor of it.
    let (_, log) = send(&app, "GET", &format!("/repos/{repo}/log?limit=1"), None, None).await;
    let log_contexts = log["log_contexts"].as_array().unwrap();
    let ids: Vec<i64> = log_contexts.iter().map(|l| l["id"].as_i64().unwrap()).collect();
    assert!(ids.contains(&answer) && ids.contains(&run), "{log}");
}

#[tokio::test]
async fn a_write_without_a_context_opens_its_own_log_context() {
    let (app, repo, _root) = setup("own").await;
    create(&app, &repo, None).await;
    let first = newest_op(&app, &repo).await;
    create(&app, &repo, None).await;
    let second = newest_op(&app, &repo).await;
    assert_ne!(first["log_context_id"], second["log_context_id"], "one log context per request");

    let id = second["log_context_id"].as_i64().unwrap();
    let (_, log_context) =
        send(&app, "GET", &format!("/repos/{repo}/contexts/{id}"), None, None).await;
    assert!(!log_context["name"].as_str().unwrap().is_empty(), "{log_context}");
    assert_eq!(log_context["parent"], Value::Null);
}

#[tokio::test]
async fn a_write_inside_an_unknown_context_is_refused_and_writes_nothing() {
    let (app, repo, _root) = setup("unknown_context").await;
    let before = newest_op(&app, &repo).await;
    let (status, body) = create(&app, &repo, Some("99999")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = create(&app, &repo, Some("not-a-number")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(newest_op(&app, &repo).await["id"], before["id"]);
}

#[tokio::test]
async fn a_log_context_detail_lists_its_operations() {
    let (app, repo, _root) = setup("detail").await;
    let log_context = new_log_context(&app, &repo, json!({"name": "two writes"})).await;
    create(&app, &repo, Some(&log_context.to_string())).await;
    create(&app, &repo, Some(&log_context.to_string())).await;

    let (status, detail) =
        send(&app, "GET", &format!("/repos/{repo}/log/contexts/{log_context}"), None, None).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["log_context"]["name"], "two writes");
    assert_eq!(detail["log_context"]["operation_count"], 2);
    assert_eq!(detail["operations"].as_array().unwrap().len(), 2);

    let (status, head) =
        send(&app, "GET", &format!("/repos/{repo}/log/contexts/head"), None, None).await;
    assert_eq!(status, StatusCode::OK, "{head}");
    assert_eq!(head["log_context"]["id"], log_context);
}

/// Reverting a log context reverts what was written inside it, child log contexts
/// included: a run is undone whole.
#[tokio::test]
async fn reverting_a_log_context_reverts_its_children_too() {
    let (app, repo, _root) = setup("revert").await;
    let run = new_log_context(&app, &repo, json!({"name": "run"})).await;
    let one = new_log_context(&app, &repo, json!({"name": "one", "parent": run})).await;
    let two = new_log_context(&app, &repo, json!({"name": "two", "parent": run})).await;
    let (_, a) = create(&app, &repo, Some(&one.to_string())).await;
    let (_, b) = create(&app, &repo, Some(&two.to_string())).await;

    let (status, body) = send(
        &app,
        "POST",
        &format!("/repos/{repo}/revert"),
        Some(json!({"target": {"log_context_id": run}})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["reverted_operations"].as_array().unwrap().len(), 2, "{body}");
    for record in [&a, &b] {
        let uuid = record["uuid"].as_str().unwrap();
        let (status, _) =
            send(&app, "GET", &format!("/repos/{repo}/metarecords/{uuid}"), None, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uuid} was created inside the run");
    }
}

#[tokio::test]
async fn prune_keeps_a_log_context_whole() {
    let (app, repo, _root) = setup("prune").await;
    create(&app, &repo, None).await;
    let pruned = newest_op(&app, &repo).await["log_context_id"].as_i64().unwrap();
    let kept = new_log_context(&app, &repo, json!({"name": "kept"})).await;
    create(&app, &repo, Some(&kept.to_string())).await;
    create(&app, &repo, Some(&kept.to_string())).await;

    let (status, body) = send(
        &app,
        "POST",
        &format!("/repos/{repo}/log/prune"),
        Some(json!({"target": {"log_context": kept}})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, log) = send(&app, "GET", &format!("/repos/{repo}/log"), None, None).await;
    let ops = log["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 2, "{log}");
    assert!(ops.iter().all(|op| op["log_context_id"] == kept), "{log}");
    // A prune removes operations, never contexts: the emptied one stays
    // until a reindex (doc "Log contexts").
    assert_eq!(body["pruned_log_contexts"], Value::Null, "{body}");
    let (status, _) =
        send(&app, "GET", &format!("/repos/{repo}/contexts/{pruned}"), None, None).await;
    assert_eq!(status, StatusCode::OK);
}

/// `0` is no context: the write opens a log context of its own, as with no header.
#[tokio::test]
async fn a_context_of_zero_is_no_context() {
    let (app, repo, _root) = setup("zero").await;
    let (status, body) = create(&app, &repo, Some("0")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let op = newest_op(&app, &repo).await;
    let id = op["log_context_id"].as_i64().unwrap();
    assert!(id >= 1, "{op}");
    let (_, log_context) =
        send(&app, "GET", &format!("/repos/{repo}/contexts/{id}"), None, None).await;
    assert_eq!(log_context["parent"], Value::Null, "{log_context}");
}

/// Ids start at 1, `0` meaning "no context".
#[tokio::test]
async fn no_log_context_has_the_id_zero() {
    let (app, repo, _root) = setup("from_one").await;
    let first = new_log_context(&app, &repo, json!({"name": "first"})).await;
    assert!(first >= 1, "{first}");
    let (status, _) = send(&app, "GET", &format!("/repos/{repo}/contexts/0"), None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// The command headers are gone: a request naming one writes in the
/// context it names, or in one of its own — never in a log context found by a key.
#[tokio::test]
async fn a_command_key_opens_no_log_context() {
    let (app, repo, _root) = setup("no_command").await;
    let run = new_log_context(&app, &repo, json!({"name": "run"})).await;
    let fields = json!({"fields": [{"name": "a", "value": {"type": "int", "value": 1}}]});
    for _ in 0..2 {
        let request = Request::builder()
            .method("POST")
            .uri(format!("/repos/{repo}/metarecords"))
            .header("x-metafolder-command", "6a1f0c2e9b8d4f37a5e1c3b2d4f6a8e0")
            .header("x-metafolder-log-context", run.to_string())
            .header("content-type", "application/json")
            .body(Body::from(fields.to_string()))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
    let (_, log) = send(&app, "GET", &format!("/repos/{repo}/log?limit=2"), None, None).await;
    let ops = log["operations"].as_array().unwrap();
    assert!(ops.iter().all(|op| op["log_context_id"] == run), "{log}");
}

/// Reverting a log context nothing was written in reverts nothing — what a script
/// going back over an answer that wrote nothing asks for. An unknown log context is
/// still a `404`.
#[tokio::test]
async fn reverting_an_empty_log_context_reverts_nothing() {
    let (app, repo, _root) = setup("revert_empty").await;
    let empty = new_log_context(&app, &repo, json!({"name": "answer 3"})).await;
    let (status, body) = send(
        &app,
        "POST",
        &format!("/repos/{repo}/revert"),
        Some(json!({"target": {"log_context_id": empty}})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["log_context"], Value::Null);
    let (status, plan) = send(
        &app,
        "GET",
        &format!("/repos/{repo}/revert/plan?target_log_context_id={empty}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{plan}");
    assert_eq!(plan["operations"], json!([]));

    let (status, _) = send(
        &app,
        "POST",
        &format!("/repos/{repo}/revert"),
        Some(json!({"target": {"log_context_id": 99999}})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ── Undo and redo of a log context (doc "Undo and redo by log context") ─────────────────

/// Sets field `a` of `uuid` to `n`, inside `log_context`.
async fn set_a(app: &Router, repo: &str, uuid: &str, n: i64, log_context: i64) {
    let (status, body) = send(
        app,
        "PUT",
        &format!("/repos/{repo}/metarecords/{uuid}/fields/a"),
        Some(json!({"value": {"type": "int", "value": n}})),
        Some(&log_context.to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

async fn field_a(app: &Router, repo: &str, uuid: &str) -> Value {
    let (_, record) =
        send(app, "GET", &format!("/repos/{repo}/metarecords/{uuid}"), None, None).await;
    record["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "a")
        .map(|f| f["value"]["value"].clone())
        .unwrap_or(Value::Null)
}

async fn revert(app: &Router, repo: &str, target: Value) -> Value {
    let (status, body) =
        send(app, "POST", &format!("/repos/{repo}/revert"), Some(json!({"target": target})), None)
            .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

/// Undoing a run takes back what still stands of it — not an answer already
/// taken back, and not the going back itself, which would put that answer back.
#[tokio::test]
async fn undoing_a_log_context_takes_back_only_its_standing_changes() {
    let (app, repo, _root) = setup("undo_label").await;
    let (_, record) = create(&app, &repo, None).await;
    let uuid = record["uuid"].as_str().unwrap().to_string();
    let run = new_log_context(&app, &repo, json!({"name": "run"})).await;
    let first = new_log_context(&app, &repo, json!({"name": "answer 1", "parent": run})).await;
    set_a(&app, &repo, &uuid, 2, first).await;
    // Going back over answer 1, inside the run: an undo of it.
    revert(&app, &repo, json!({"log_context_id": first})).await;
    let second =
        new_log_context(&app, &repo, json!({"name": "answer 1 again", "parent": run})).await;
    set_a(&app, &repo, &uuid, 3, second).await;
    assert_eq!(field_a(&app, &repo, &uuid).await, 3);

    let (status, plan) = send(
        &app,
        "GET",
        &format!("/repos/{repo}/revert/plan?target_log_context_id={run}&only=changes"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{plan}");
    assert_eq!(plan["operations"].as_array().unwrap().len(), 1, "only the standing change: {plan}");

    let undo = revert(&app, &repo, json!({"log_context_id": run, "only": "changes"})).await;
    assert_eq!(undo["reverted_operations"].as_array().unwrap().len(), 1, "{undo}");
    assert_eq!(field_a(&app, &repo, &uuid).await, 1, "back to before the run");

    // Its redo takes the undo back: the run's standing undos, that is.
    let undo_log_context = undo["log_context"].as_i64().unwrap();
    let redo =
        revert(&app, &repo, json!({"log_context_id": undo_log_context, "only": "undos"})).await;
    assert_eq!(redo["reverted_operations"].as_array().unwrap().len(), 1, "{redo}");
    assert_eq!(field_a(&app, &repo, &uuid).await, 3);

    let (status, body) = send(
        &app,
        "POST",
        &format!("/repos/{repo}/revert"),
        Some(json!({"target": {"log_context_id": run, "only": "everything"}})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}
