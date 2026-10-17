//! Labels and contexts over HTTP (doc "Labels and contexts"): creating a
//! label, reading it with its ancestors, writing inside one through the
//! `X-Metafolder-Label` header, and a write with no context opening its own.

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
    label: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(label) = label {
        builder = builder.header("x-metafolder-label", label);
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
    let app = routes::build(Arc::new(AppState::new()));
    let root = TempDir::new(&format!("labels_{prefix}"));
    common::seed_config(&root);
    let (status, body) =
        send(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()})), None)
            .await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    let repo = body["repo_uuid"].as_str().unwrap().to_string();
    (app, repo, root)
}

async fn new_label(app: &Router, repo: &str, body: Value) -> i64 {
    let (status, label) =
        send(app, "POST", &format!("/repos/{repo}/labels"), Some(body), None).await;
    assert_eq!(status, StatusCode::OK, "label creation failed: {label}");
    label["id"].as_i64().expect("a label id")
}

async fn create(app: &Router, repo: &str, label: Option<&str>) -> (StatusCode, Value) {
    let fields = json!([{"name": "a", "value": {"type": "int", "value": 1}}]);
    send(app, "POST", &format!("/repos/{repo}/metarecords"), Some(json!({"fields": fields})), label)
        .await
}

async fn newest_op(app: &Router, repo: &str) -> Value {
    let (status, log) = send(app, "GET", &format!("/repos/{repo}/log?limit=1"), None, None).await;
    assert_eq!(status, StatusCode::OK, "{log}");
    log["operations"][0].clone()
}

#[tokio::test]
async fn a_label_is_read_back_with_its_ancestors() {
    let (app, repo, _root) = setup("ancestors").await;
    let run = new_label(&app, &repo, json!({"name": "tag run", "description": "all photos"})).await;
    let answer = new_label(&app, &repo, json!({"name": "answer 1", "parent": run})).await;

    let (status, label) =
        send(&app, "GET", &format!("/repos/{repo}/labels/{answer}"), None, None).await;
    assert_eq!(status, StatusCode::OK, "{label}");
    assert_eq!(label["name"], "answer 1");
    assert_eq!(label["parent"], run);
    assert!(label["created"].as_i64().is_some(), "{label}");
    let ancestors = label["ancestors"].as_array().unwrap();
    assert_eq!(ancestors.len(), 1, "{label}");
    assert_eq!(ancestors[0]["id"], run);
    assert_eq!(ancestors[0]["description"], "all photos");

    let (status, _) = send(&app, "GET", &format!("/repos/{repo}/labels/99999"), None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_label_under_an_unknown_parent_is_refused() {
    let (app, repo, _root) = setup("unknown_parent").await;
    let (status, body) = send(
        &app,
        "POST",
        &format!("/repos/{repo}/labels"),
        Some(json!({"name": "x", "parent": 99999})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn a_write_inside_a_context_carries_its_label() {
    let (app, repo, _root) = setup("context").await;
    let run = new_label(&app, &repo, json!({"name": "tag run"})).await;
    let answer = new_label(&app, &repo, json!({"name": "answer 1", "parent": run})).await;

    let (status, body) = create(&app, &repo, Some(&answer.to_string())).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let op = newest_op(&app, &repo).await;
    assert_eq!(op["label_id"], answer, "{op}");

    // The listing names the label and every ancestor of it.
    let (_, log) = send(&app, "GET", &format!("/repos/{repo}/log?limit=1"), None, None).await;
    let labels = log["labels"].as_array().unwrap();
    let ids: Vec<i64> = labels.iter().map(|l| l["id"].as_i64().unwrap()).collect();
    assert!(ids.contains(&answer) && ids.contains(&run), "{log}");
}

#[tokio::test]
async fn a_write_without_a_context_opens_its_own_label() {
    let (app, repo, _root) = setup("own").await;
    create(&app, &repo, None).await;
    let first = newest_op(&app, &repo).await;
    create(&app, &repo, None).await;
    let second = newest_op(&app, &repo).await;
    assert_ne!(first["label_id"], second["label_id"], "one label per request");

    let id = second["label_id"].as_i64().unwrap();
    let (_, label) = send(&app, "GET", &format!("/repos/{repo}/labels/{id}"), None, None).await;
    assert!(!label["name"].as_str().unwrap().is_empty(), "{label}");
    assert_eq!(label["parent"], Value::Null);
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
async fn a_label_detail_lists_its_operations() {
    let (app, repo, _root) = setup("detail").await;
    let label = new_label(&app, &repo, json!({"name": "two writes"})).await;
    create(&app, &repo, Some(&label.to_string())).await;
    create(&app, &repo, Some(&label.to_string())).await;

    let (status, detail) =
        send(&app, "GET", &format!("/repos/{repo}/log/labels/{label}"), None, None).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["label"]["name"], "two writes");
    assert_eq!(detail["label"]["operation_count"], 2);
    assert_eq!(detail["operations"].as_array().unwrap().len(), 2);

    let (status, head) =
        send(&app, "GET", &format!("/repos/{repo}/log/labels/head"), None, None).await;
    assert_eq!(status, StatusCode::OK, "{head}");
    assert_eq!(head["label"]["id"], label);
}

/// Reverting a label reverts what was written inside it, child labels
/// included: a run is undone whole.
#[tokio::test]
async fn reverting_a_label_reverts_its_children_too() {
    let (app, repo, _root) = setup("revert").await;
    let run = new_label(&app, &repo, json!({"name": "run"})).await;
    let one = new_label(&app, &repo, json!({"name": "one", "parent": run})).await;
    let two = new_label(&app, &repo, json!({"name": "two", "parent": run})).await;
    let (_, a) = create(&app, &repo, Some(&one.to_string())).await;
    let (_, b) = create(&app, &repo, Some(&two.to_string())).await;

    let (status, body) = send(
        &app,
        "POST",
        &format!("/repos/{repo}/revert"),
        Some(json!({"target": {"label_id": run}})),
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
async fn prune_keeps_a_label_whole() {
    let (app, repo, _root) = setup("prune").await;
    create(&app, &repo, None).await;
    let kept = new_label(&app, &repo, json!({"name": "kept"})).await;
    create(&app, &repo, Some(&kept.to_string())).await;
    create(&app, &repo, Some(&kept.to_string())).await;

    let (status, body) = send(
        &app,
        "POST",
        &format!("/repos/{repo}/log/prune"),
        Some(json!({"target": {"label": kept}})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, log) = send(&app, "GET", &format!("/repos/{repo}/log"), None, None).await;
    let ops = log["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 2, "{log}");
    assert!(ops.iter().all(|op| op["label_id"] == kept), "{log}");
}

// ── A command's label (doc "Labels and contexts") ────────────────────────────

/// Sends `request` as one step of a client command: the command's key and
/// name, and the context it runs in.
async fn send_in_command(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    command: (&str, &str),
    context: Option<i64>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("x-metafolder-command", command.0)
        .header("x-metafolder-command-name", metafolder_core::context::encode(command.1));
    if let Some(context) = context {
        builder = builder.header("x-metafolder-label", context.to_string());
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

/// A command opens its label at its first write, and writes all its requests
/// in it — inside the context it runs in. Its reads open nothing.
#[tokio::test]
async fn a_commands_writes_share_one_label_opened_at_the_first() {
    let (app, repo, _root) = setup("command").await;
    let run = new_label(&app, &repo, json!({"name": "tag run"})).await;
    let (_, before) = send(&app, "GET", &format!("/repos/{repo}/log?limit=1"), None, None).await;
    let key = "6a1f0c2e9b8d4f37a5e1c3b2d4f6a8e0";
    let name = "mf metarecord -q 'année = 2024' field set été:bool=true";

    let query = json!({"query": {"type": "is_present", "field": "a"}, "limit": 10});
    let (status, _) = send_in_command(
        &app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(query),
        (key, name),
        Some(run),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, after_read) =
        send(&app, "GET", &format!("/repos/{repo}/log?limit=1"), None, None).await;
    assert_eq!(after_read["total_labels"], before["total_labels"], "a read opens no label");

    let fields = json!({"fields": [{"name": "a", "value": {"type": "int", "value": 1}}]});
    for _ in 0..2 {
        let (status, body) = send_in_command(
            &app,
            "POST",
            &format!("/repos/{repo}/metarecords"),
            Some(fields.clone()),
            (key, name),
            Some(run),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let (_, log) = send(&app, "GET", &format!("/repos/{repo}/log?limit=2"), None, None).await;
    let ops = log["operations"].as_array().unwrap();
    assert_eq!(ops[0]["label_id"], ops[1]["label_id"], "one label for the command: {log}");
    let id = ops[0]["label_id"].as_i64().unwrap();
    let (_, label) = send(&app, "GET", &format!("/repos/{repo}/labels/{id}"), None, None).await;
    assert_eq!(label["name"], name, "the name comes back whole, accents included");
    assert_eq!(label["parent"], run, "inside the context the command runs in");

    // Another command is another label.
    let (_, _) = send_in_command(
        &app,
        "POST",
        &format!("/repos/{repo}/metarecords"),
        Some(fields),
        ("0b8f2a7e5c1d4e3fa9b8c7d6e5f4a3b2", "mf metarecord add"),
        None,
    )
    .await;
    let other = newest_op(&app, &repo).await;
    assert_ne!(other["label_id"], id);
}

/// Reverting a label nothing was written in reverts nothing — what a script
/// going back over an answer that wrote nothing asks for. An unknown label is
/// still a `404`.
#[tokio::test]
async fn reverting_an_empty_label_reverts_nothing() {
    let (app, repo, _root) = setup("revert_empty").await;
    let empty = new_label(&app, &repo, json!({"name": "answer 3"})).await;
    let (status, body) = send(
        &app,
        "POST",
        &format!("/repos/{repo}/revert"),
        Some(json!({"target": {"label_id": empty}})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["label"], Value::Null);
    let (status, plan) = send(
        &app,
        "GET",
        &format!("/repos/{repo}/revert/plan?target_label_id={empty}"),
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
        Some(json!({"target": {"label_id": 99999}})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ── Undo and redo of a label (doc "Undo and redo by label") ─────────────────

/// Sets field `a` of `uuid` to `n`, inside `label`.
async fn set_a(app: &Router, repo: &str, uuid: &str, n: i64, label: i64) {
    let (status, body) = send(
        app,
        "PUT",
        &format!("/repos/{repo}/metarecords/{uuid}/fields/a"),
        Some(json!({"value": {"type": "int", "value": n}})),
        Some(&label.to_string()),
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
async fn undoing_a_label_takes_back_only_its_standing_changes() {
    let (app, repo, _root) = setup("undo_label").await;
    let (_, record) = create(&app, &repo, None).await;
    let uuid = record["uuid"].as_str().unwrap().to_string();
    let run = new_label(&app, &repo, json!({"name": "run"})).await;
    let first = new_label(&app, &repo, json!({"name": "answer 1", "parent": run})).await;
    set_a(&app, &repo, &uuid, 2, first).await;
    // Going back over answer 1, inside the run: an undo of it.
    revert(&app, &repo, json!({"label_id": first})).await;
    let second = new_label(&app, &repo, json!({"name": "answer 1 again", "parent": run})).await;
    set_a(&app, &repo, &uuid, 3, second).await;
    assert_eq!(field_a(&app, &repo, &uuid).await, 3);

    let (status, plan) = send(
        &app,
        "GET",
        &format!("/repos/{repo}/revert/plan?target_label_id={run}&only=changes"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{plan}");
    assert_eq!(plan["operations"].as_array().unwrap().len(), 1, "only the standing change: {plan}");

    let undo = revert(&app, &repo, json!({"label_id": run, "only": "changes"})).await;
    assert_eq!(undo["reverted_operations"].as_array().unwrap().len(), 1, "{undo}");
    assert_eq!(field_a(&app, &repo, &uuid).await, 1, "back to before the run");

    // Its redo takes the undo back: the run's standing undos, that is.
    let undo_label = undo["label"].as_i64().unwrap();
    let redo = revert(&app, &repo, json!({"label_id": undo_label, "only": "undos"})).await;
    assert_eq!(redo["reverted_operations"].as_array().unwrap().len(), 1, "{redo}");
    assert_eq!(field_a(&app, &repo, &uuid).await, 3);

    let (status, body) = send(
        &app,
        "POST",
        &format!("/repos/{repo}/revert"),
        Some(json!({"target": {"label_id": run, "only": "everything"}})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}
