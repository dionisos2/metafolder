//! `POST /query/fields/replace` and `POST /query/fields/rename`: a value
//! replaced, or a field renamed, on every match of a query, in one write
//! made of mass writes (doc "Query endpoints").
//!
//! Every test runs twice: with a mass-write threshold of 0, every write to
//! many records in bulk, and with the shipped one, the few records of a test
//! written one at a time (doc "Mass writes in the store").

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

/// The two regimes every test runs under: in bulk, and one at a time.
const THRESHOLDS: [Option<u64>; 2] = [Some(0), None];

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

async fn setup(prefix: &str, threshold: Option<u64>) -> (Router, String, TempDir) {
    let mut settings = common::settings();
    if let Some(t) = threshold {
        settings.mass_write_threshold = t;
    }
    let app = routes::build(std::sync::Arc::new(AppState::new(settings)));
    let root = TempDir::new(&format!("qrr_{prefix}"));
    common::seed_config(&root);
    let (status, body) =
        request(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    let repo = body["repo_uuid"].as_str().unwrap().to_string();
    (app, repo, root)
}

fn s(v: &str) -> Value {
    json!({"type": "string", "value": v})
}

fn row(name: &str, value: Value) -> Value {
    json!({"name": name, "value": value})
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

/// The fields of a metarecord as `(name, value)` pairs, sorted.
async fn fields(app: &Router, repo: &str, uuid: &str) -> Vec<(String, Value)> {
    let (status, body) =
        request(app, "GET", &format!("/repos/{repo}/metarecords/{uuid}"), None).await;
    assert_eq!(status, StatusCode::OK, "get failed: {body}");
    let mut out: Vec<(String, Value)> = body["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| (f["name"].as_str().unwrap().to_string(), f["value"].clone()))
        .collect();
    out.sort_by_key(|(n, v)| (n.clone(), v.to_string()));
    out
}

fn pairs(list: &[(&str, Value)]) -> Vec<(String, Value)> {
    let mut out: Vec<(String, Value)> =
        list.iter().map(|(n, v)| (n.to_string(), v.clone())).collect();
    out.sort_by_key(|(n, v)| (n.clone(), v.to_string()));
    out
}

/// The log contexts of the repository, and the number of operations of the
/// head one.
async fn log_state(app: &Router, repo: &str) -> (usize, u64) {
    let (_, log) = request(app, "GET", &format!("/repos/{repo}/log"), None).await;
    let contexts = log["log_contexts"].as_array().unwrap();
    let head = contexts.iter().find(|c| c["is_head"] == json!(true));
    (contexts.len(), head.map_or(0, |c| c["operations"].as_u64().unwrap()))
}

async fn undo(app: &Router, repo: &str) {
    let (status, body) = request(
        app,
        "POST",
        &format!("/repos/{repo}/revert"),
        Some(json!({"target": {"log_context_id": "head"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "revert failed: {body}");
}

fn present(field: &str) -> Value {
    json!({"type": "is_present", "field": field})
}

#[tokio::test]
async fn test_replace_by_query_replaces_the_value_on_the_matches_holding_it() {
    for threshold in THRESHOLDS {
        let (app, repo, _root) = setup("replace", threshold).await;
        let mut jazz = Vec::new();
        for _ in 0..3 {
            jazz.push(
                create(&app, &repo, json!([row("tag", s("jazz")), row("tag", s("keep"))])).await,
            );
        }
        let rock = create(&app, &repo, json!([row("tag", s("rock"))])).await;
        let (contexts_before, _) = log_state(&app, &repo).await;

        let (status, body) = request(
            &app,
            "POST",
            &format!("/repos/{repo}/query/fields/replace"),
            Some(json!({"query": present("tag"), "name": "tag",
                        "value": s("jazz"), "to": s("blues")})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "replace failed: {body}");
        assert_eq!(body, json!({"updated": 3}), "threshold {threshold:?}");

        for uuid in &jazz {
            assert_eq!(
                fields(&app, &repo, uuid).await,
                pairs(&[("tag", s("blues")), ("tag", s("keep"))]),
                "threshold {threshold:?}"
            );
        }
        assert_eq!(fields(&app, &repo, &rock).await, pairs(&[("tag", s("rock"))]));
        let (contexts_after, _) = log_state(&app, &repo).await;
        assert_eq!(contexts_after - contexts_before, 1, "the replacement is one write");

        // Undone in one step.
        undo(&app, &repo).await;
        for uuid in &jazz {
            assert_eq!(
                fields(&app, &repo, uuid).await,
                pairs(&[("tag", s("jazz")), ("tag", s("keep"))]),
                "threshold {threshold:?}"
            );
        }
    }
}

#[tokio::test]
async fn test_replace_by_query_onto_a_value_held_leaves_that_one() {
    for threshold in THRESHOLDS {
        let (app, repo, _root) = setup("replace_onto", threshold).await;
        let both =
            create(&app, &repo, json!([row("tag", s("jazz")), row("tag", s("blues"))])).await;
        let one = create(&app, &repo, json!([row("tag", s("jazz"))])).await;
        let (status, body) = request(
            &app,
            "POST",
            &format!("/repos/{repo}/query/fields/replace"),
            Some(json!({"query": present("tag"), "name": "tag",
                        "value": s("jazz"), "to": s("blues")})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "replace failed: {body}");
        assert_eq!(body, json!({"updated": 2}));
        assert_eq!(fields(&app, &repo, &both).await, pairs(&[("tag", s("blues"))]));
        assert_eq!(fields(&app, &repo, &one).await, pairs(&[("tag", s("blues"))]));
    }
}

#[tokio::test]
async fn test_replace_by_query_by_nothing_is_refused_whole_beside_other_values() {
    for threshold in THRESHOLDS {
        let (app, repo, _root) = setup("replace_nothing", threshold).await;
        let alone = create(&app, &repo, json!([row("tag", s("jazz"))])).await;
        let beside =
            create(&app, &repo, json!([row("tag", s("jazz")), row("tag", s("keep"))])).await;
        let (contexts_before, _) = log_state(&app, &repo).await;

        let (status, body) = request(
            &app,
            "POST",
            &format!("/repos/{repo}/query/fields/replace"),
            Some(json!({"query": present("tag"), "name": "tag",
                        "value": s("jazz"), "to": {"type": "nothing"}})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "threshold {threshold:?}: {body}");
        let message = body["error"].as_str().unwrap_or_default();
        assert!(message.contains(&beside), "the refusal names {beside}: {message}");
        assert!(!message.contains(&alone), "only the metarecords in breach: {message}");
        // Nothing written.
        assert_eq!(fields(&app, &repo, &alone).await, pairs(&[("tag", s("jazz"))]));
        assert_eq!(log_state(&app, &repo).await.0, contexts_before);

        // Alone, the value is replaced by an absence.
        let (status, body) = request(
            &app,
            "POST",
            &format!("/repos/{repo}/query/fields/replace"),
            Some(json!({"query": {"type": "uuid_in", "uuids": [alone]}, "name": "tag",
                        "value": s("jazz"), "to": {"type": "nothing"}})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "replace failed: {body}");
        assert_eq!(
            fields(&app, &repo, &alone).await,
            pairs(&[("tag", json!({"type": "nothing"}))])
        );
    }
}

#[tokio::test]
async fn test_replace_by_query_by_a_value_of_another_type_is_refused() {
    for threshold in THRESHOLDS {
        let (app, repo, _root) = setup("replace_type", threshold).await;
        let uuid =
            create(&app, &repo, json!([row("rating", json!({"type": "int", "value": 4}))])).await;
        let (status, body) = request(
            &app,
            "POST",
            &format!("/repos/{repo}/query/fields/replace"),
            Some(json!({"query": present("rating"), "name": "rating",
                        "value": {"type": "int", "value": 4}, "to": s("four")})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "threshold {threshold:?}: {body}");
        assert_eq!(
            fields(&app, &repo, &uuid).await,
            pairs(&[("rating", json!({"type": "int", "value": 4}))])
        );
    }
}

#[tokio::test]
async fn test_rename_by_query_moves_every_value_of_the_matches() {
    for threshold in THRESHOLDS {
        let (app, repo, _root) = setup("rename", threshold).await;
        let mut matched = Vec::new();
        for _ in 0..3 {
            matched.push(
                create(
                    &app,
                    &repo,
                    json!([row("tag", s("jazz")), row("tag", s("keep")), row("pick", s("y"))]),
                )
                .await,
            );
        }
        let other = create(&app, &repo, json!([row("tag", s("rock"))])).await;
        let (contexts_before, _) = log_state(&app, &repo).await;

        let (status, body) = request(
            &app,
            "POST",
            &format!("/repos/{repo}/query/fields/rename"),
            Some(json!({"query": present("pick"), "name": "tag", "to": "genre"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "rename failed: {body}");
        assert_eq!(body, json!({"updated": 3}), "threshold {threshold:?}");
        for uuid in &matched {
            assert_eq!(
                fields(&app, &repo, uuid).await,
                pairs(&[("genre", s("jazz")), ("genre", s("keep")), ("pick", s("y"))]),
                "threshold {threshold:?}"
            );
        }
        assert_eq!(fields(&app, &repo, &other).await, pairs(&[("tag", s("rock"))]));
        assert_eq!(log_state(&app, &repo).await.0 - contexts_before, 1, "one write");

        undo(&app, &repo).await;
        for uuid in &matched {
            assert_eq!(
                fields(&app, &repo, uuid).await,
                pairs(&[("pick", s("y")), ("tag", s("jazz")), ("tag", s("keep"))]),
                "threshold {threshold:?}"
            );
        }
    }
}

#[tokio::test]
async fn test_rename_by_query_onto_a_name_a_match_holds_is_refused_whole() {
    for threshold in THRESHOLDS {
        let (app, repo, _root) = setup("rename_taken", threshold).await;
        let free = create(&app, &repo, json!([row("tag", s("jazz"))])).await;
        let taken = create(&app, &repo, json!([row("tag", s("jazz")), row("genre", s("x"))])).await;
        let (contexts_before, _) = log_state(&app, &repo).await;
        let (status, body) = request(
            &app,
            "POST",
            &format!("/repos/{repo}/query/fields/rename"),
            Some(json!({"query": present("tag"), "name": "tag", "to": "genre"})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "threshold {threshold:?}: {body}");
        let message = body["error"].as_str().unwrap_or_default();
        assert!(message.contains(&taken), "the refusal names {taken}: {message}");
        assert!(!message.contains(&free), "only the metarecords in breach: {message}");
        assert_eq!(fields(&app, &repo, &free).await, pairs(&[("tag", s("jazz"))]));
        assert_eq!(log_state(&app, &repo).await.0, contexts_before);
    }
}

#[tokio::test]
async fn test_rename_by_query_to_a_name_of_another_type_is_refused() {
    for threshold in THRESHOLDS {
        let (app, repo, _root) = setup("rename_type", threshold).await;
        let uuid = create(&app, &repo, json!([row("tag", s("jazz"))])).await;
        create(&app, &repo, json!([row("rating", json!({"type": "int", "value": 4}))])).await;
        let (status, body) = request(
            &app,
            "POST",
            &format!("/repos/{repo}/query/fields/rename"),
            Some(json!({"query": present("tag"), "name": "tag", "to": "rating"})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "threshold {threshold:?}: {body}");
        assert_eq!(fields(&app, &repo, &uuid).await, pairs(&[("tag", s("jazz"))]));
    }
}

#[tokio::test]
async fn test_a_reserved_name_needs_force() {
    let (app, repo, _root) = setup("reserved", None).await;
    create(&app, &repo, json!([row("tag", s("jazz"))])).await;
    let (status, _) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query/fields/rename"),
        Some(json!({"query": present("tag"), "name": "tag", "to": "mf_tag"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query/fields/replace"),
        Some(json!({"query": present("mf_path"), "name": "mf_path",
                    "value": s("a"), "to": s("b")})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// The writes that wait for their fold, one per write to many records and
/// part of the ids (doc "Mass writes in the store"): what tells a write in
/// bulk from one record at a time, which leaves none.
async fn pending_writes(app: &Router, repo: &str) -> u64 {
    let (status, wear) = request(app, "GET", &format!("/repos/{repo}/wear"), None).await;
    assert_eq!(status, StatusCode::OK, "{wear}");
    wear["pending_writes"].as_u64().unwrap()
}

/// In bulk, a replacement or a rename over many records is a few mass
/// writes, not one or two writes per record.
#[tokio::test]
async fn test_in_bulk_a_few_mass_writes_not_one_per_record() {
    for (threshold, bulk) in [(Some(0), true), (Some(1000), false)] {
        let (app, repo, _root) = setup("bulk_log", threshold).await;
        for i in 0..30 {
            let tag = if i % 2 == 0 { "jazz" } else { "rock" };
            create(&app, &repo, json!([row("tag", s(tag))])).await;
        }
        let (status, body) = request(
            &app,
            "POST",
            &format!("/repos/{repo}/query/fields/replace"),
            Some(json!({"query": present("tag"), "name": "tag",
                        "value": s("jazz"), "to": s("blues")})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "replace failed: {body}");
        assert_eq!(body, json!({"updated": 15}));
        // A remove and an add.
        assert_eq!(pending_writes(&app, &repo).await, if bulk { 2 } else { 0 });

        let (status, body) = request(
            &app,
            "POST",
            &format!("/repos/{repo}/query/fields/rename"),
            Some(json!({"query": present("tag"), "name": "tag", "to": "genre"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "rename failed: {body}");
        assert_eq!(body, json!({"updated": 30}));
        // An add per value, two, and an unset.
        assert_eq!(pending_writes(&app, &repo).await, if bulk { 5 } else { 0 });
    }
}
