//! Tests for the user schema system (spec-schema): file loading and
//! validation, delta validation on writes, check/reload endpoints.

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

fn app() -> Router {
    routes::build(Arc::new(AppState::new()))
}

fn film_schema() -> Value {
    json!({
        "version": 1,
        "groups": [
            {"targets": "*",
             "constraints": [{"field": "rating", "type": "int"}]},
            {"targets": ["film"],
             "constraints": [
                 {"field": "rating", "min": 0, "max": 1},
                 {"field": "name", "type": "string", "min": 1, "max": 1}
             ]}
        ]
    })
}

/// Initialises a repo, writes a schema file, then loads it in a fresh
/// daemon state (schemas are read at load time).
async fn setup_with_schema(prefix: &str, schema: Value) -> (Router, String, PathBuf) {
    let root = std::env::temp_dir().join(format!("metafolder_sch_{prefix}_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    {
        let first = app();
        let (status, body) = request(
            &first,
            "POST",
            "/repos/init",
            Some(json!({"root": root.to_str().unwrap()})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "init failed: {body}");
    } // First daemon dropped: exclusive lock released.

    std::fs::write(
        root.join(".metafolder/schema.json"),
        serde_json::to_string_pretty(&schema).unwrap(),
    )
    .unwrap();

    let second = app();
    let (status, body) = request(
        &second,
        "POST",
        "/repos/load",
        Some(json!({"root": root.to_str().unwrap()})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "load failed: {body}");
    let repo = body["repo_uuid"].as_str().unwrap().to_string();
    (second, repo, root)
}

async fn create(app: &Router, repo: &str, fields: Value) -> (StatusCode, Value) {
    request(app, "POST", &format!("/repos/{repo}/metarecords"), Some(json!({"fields": fields}))).await
}

// ── Loading ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_schema_loads_and_is_returned() {
    let (app, repo, root) = setup_with_schema("load", film_schema()).await;
    let (status, body) = request(&app, "GET", &format!("/repos/{repo}/schema"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, film_schema());
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_schema_with_default_loads_and_is_returned() {
    // A constraint may carry a `default` value (used by client templates); it
    // must load and be returned verbatim by GET /schema.
    let schema = json!({
        "version": 1,
        "groups": [
            {"targets": ["tag"], "constraints": [
                {"field": "name", "type": "string", "min": 1, "max": 1},
                {"field": "color", "type": "string", "default": "#888888"}
            ]}
        ]
    });
    let (app, repo, root) = setup_with_schema("default", schema.clone()).await;
    let (status, body) = request(&app, "GET", &format!("/repos/{repo}/schema"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, schema);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_repo_without_schema_returns_empty_schema() {
    let app = app();
    let root = std::env::temp_dir().join(format!("metafolder_sch_none_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let (_, body) =
        request(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    let repo = body["repo_uuid"].as_str().unwrap();
    let (status, body) = request(&app, "GET", &format!("/repos/{repo}/schema"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"version": 1, "groups": []}));
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_invalid_schema_makes_load_fail() {
    for (name, schema, fragment) in [
        (
            "unknown type",
            json!({"version": 1, "groups": [
                {"targets": "*", "constraints": [{"field": "x", "type": "varchar"}]}]}),
            "varchar",
        ),
        (
            "min greater than max",
            json!({"version": 1, "groups": [
                {"targets": "*", "constraints": [{"field": "x", "min": 3, "max": 1}]}]}),
            "min",
        ),
        (
            "reserved field",
            json!({"version": 1, "groups": [
                {"targets": "*", "constraints": [{"field": "mfr_size", "type": "int"}]}]}),
            "reserved",
        ),
        (
            "empty targets",
            json!({"version": 1, "groups": [
                {"targets": [], "constraints": [{"field": "x"}]}]}),
            "targets",
        ),
        (
            "default kind mismatch",
            json!({"version": 1, "groups": [
                {"targets": "*", "constraints": [
                    {"field": "x", "type": "int", "default": "nope"}]}]}),
            "default",
        ),
        (
            "default without a type",
            json!({"version": 1, "groups": [
                {"targets": "*", "constraints": [
                    {"field": "x", "default": "v"}]}]}),
            "default",
        ),
    ] {
        let root = std::env::temp_dir().join(format!("metafolder_sch_bad_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        {
            let first = app();
            request(&first, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()})))
                .await;
        }
        std::fs::write(root.join(".metafolder/schema.json"), schema.to_string()).unwrap();
        let second = app();
        let (status, body) = request(
            &second,
            "POST",
            "/repos/load",
            Some(json!({"root": root.to_str().unwrap()})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "case '{name}' must fail: {body}");
        assert!(
            body["error"].as_str().unwrap().to_lowercase().contains(fragment),
            "case '{name}': error must identify the problem, got {body}"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}

// ── Delta validation on writes ────────────────────────────────────────────────

#[tokio::test]
async fn test_global_type_constraint_rejects_wrong_type() {
    let (app, repo, root) = setup_with_schema("type", film_schema()).await;

    // rating must be an Int wherever it appears.
    let (status, body) = create(
        &app,
        &repo,
        json!([{"name": "rating", "value": {"type": "string", "value": "five"}}]),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["violations"][0]["kind"], "type", "violations array expected, got {body}");
    assert_eq!(body["violations"][0]["field"], "rating");

    // Int is fine; Nothing is always permitted.
    let (status, _) =
        create(&app, &repo, json!([{"name": "rating", "value": {"type": "int", "value": 5}}])).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) =
        create(&app, &repo, json!([{"name": "rating", "value": {"type": "nothing"}}])).await;
    assert_eq!(status, StatusCode::OK);

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_per_type_cardinality() {
    let (app, repo, root) = setup_with_schema("card", film_schema()).await;

    // A film: rating is capped at one row; untyped metarecords are not.
    let (status, film) = create(
        &app,
        &repo,
        json!([
            {"name": "mf_schema", "value": {"type": "string", "value": "film"}},
            {"name": "name", "value": {"type": "string", "value": "Alien"}},
            {"name": "rating", "value": {"type": "int", "value": 5}}
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "film creation failed: {film}");
    let film_uuid = film["uuid"].as_str().unwrap();

    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/metarecords/{film_uuid}/fields"),
        Some(json!({"name": "rating", "value": {"type": "int", "value": 4}})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "second rating row must violate max: {body}");
    assert_eq!(body["violations"][0]["kind"], "max_cardinality");
    assert_eq!(body["violations"][0]["type"], "film");

    // Untyped metarecord: no film constraints, two ratings allowed.
    let (_, untyped) =
        create(&app, &repo, json!([{"name": "rating", "value": {"type": "int", "value": 1}}])).await;
    let untyped_uuid = untyped["uuid"].as_str().unwrap();
    let (status, _) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/metarecords/{untyped_uuid}/fields"),
        Some(json!({"name": "rating", "value": {"type": "int", "value": 2}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // min = 1 on name: deleting the last name row is rejected.
    let (_, film_now) =
        request(&app, "GET", &format!("/repos/{repo}/metarecords/{film_uuid}"), None).await;
    let name_id = film_now["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "name")
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    let (status, body) = request(
        &app,
        "DELETE",
        &format!("/repos/{repo}/fields/{name_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "removing the last name must fail: {body}");
    assert_eq!(body["violations"][0]["kind"], "min_cardinality");

    // Deleting the whole metarecord stays allowed.
    let (status, _) =
        request(&app, "DELETE", &format!("/repos/{repo}/metarecords/{film_uuid}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_delta_validation_ignores_untouched_fields() {
    let (app, repo, root) = setup_with_schema("delta", film_schema()).await;

    // A metarecord violating the rating constraint is created while the schema
    // ignores it (rating written as int first, then the schema reloaded with
    // a stricter view is simulated by writing another field).
    let (_, metarecord) = create(
        &app,
        &repo,
        json!([{"name": "rating", "value": {"type": "int", "value": 5}},
               {"name": "genre", "value": {"type": "string", "value": "sf"}}]),
    )
    .await;
    let uuid = metarecord["uuid"].as_str().unwrap();

    // Declaring a type via mf_schema never fails, even if constraints of the
    // new type are violated (film requires one name).
    let (status, _) = request(
        &app,
        "PUT",
        &format!("/repos/{repo}/metarecords/{uuid}/fields/mf_schema"),
        Some(json!({"value": {"type": "string", "value": "film"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Writing an unrelated field is fine despite the missing required name.
    let (status, _) = request(
        &app,
        "PUT",
        &format!("/repos/{repo}/metarecords/{uuid}/fields/genre"),
        Some(json!({"value": {"type": "string", "value": "horror"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_batch_set_rolls_back_on_violation() {
    let (app, repo, root) = setup_with_schema("batch", film_schema()).await;
    for genre in ["a", "b"] {
        create(&app, &repo, json!([{"name": "genre", "value": {"type": "string", "value": genre}}]))
            .await;
    }
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query/fields/set"),
        Some(json!({
            "query": {"type": "is_present", "field": "genre"},
            "name": "rating",
            "value": {"type": "string", "value": "bad"}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "got: {body}");

    // Nothing was written.
    let (_, rated) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": {"type": "is_present", "field": "rating"}})),
    )
    .await;
    assert_eq!(rated.as_array().unwrap().len(), 0, "the whole batch must roll back");

    std::fs::remove_dir_all(root).unwrap();
}

// ── Check and reload ──────────────────────────────────────────────────────────

#[tokio::test]
async fn test_schema_check_reports_existing_violations() {
    // Start without schema, create violating data, then load the schema.
    let (app, repo, root) = setup_with_schema("check", film_schema()).await;

    // Bypass validation by writing a wrongly-typed rating… impossible via
    // the API (delta validation), so simulate pre-existing data: write a
    // valid int rating, then tighten via a type declared after the fact.
    let (_, metarecord) = create(
        &app,
        &repo,
        json!([{"name": "rating", "value": {"type": "int", "value": 1}},
               {"name": "rating", "value": {"type": "int", "value": 2}}]),
    )
    .await;
    let uuid = metarecord["uuid"].as_str().unwrap();
    // Declare it a film (never validated): now rating max=1 is violated.
    request(
        &app,
        "PUT",
        &format!("/repos/{repo}/metarecords/{uuid}/fields/mf_schema"),
        Some(json!({"value": {"type": "string", "value": "film"}})),
    )
    .await;

    let (status, body) =
        request(&app, "POST", &format!("/repos/{repo}/schema/check"), Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "check failed: {body}");
    assert!(body["checked"].as_u64().unwrap() >= 2);
    let kinds: Vec<&str> = body["violations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"max_cardinality"), "got {body}");
    assert!(kinds.contains(&"min_cardinality"), "film also misses its name: {body}");

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_schema_reload() {
    let (app, repo, root) = setup_with_schema("reload", json!({"version": 1, "groups": []})).await;

    // No constraint yet: a string rating passes.
    let (status, _) = create(
        &app,
        &repo,
        json!([{"name": "rating", "value": {"type": "string", "value": "ok"}}]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Tighten the schema file and reload.
    std::fs::write(root.join(".metafolder/schema.json"), film_schema().to_string()).unwrap();
    let (status, body) =
        request(&app, "POST", &format!("/repos/{repo}/schema/reload"), None).await;
    assert_eq!(status, StatusCode::OK, "reload failed: {body}");
    assert_eq!(body, film_schema());

    let (status, _) = create(
        &app,
        &repo,
        json!([{"name": "rating", "value": {"type": "string", "value": "no"}}]),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "new schema must be in effect");

    // Invalid edit: reload fails, the previous schema stays in effect.
    std::fs::write(root.join(".metafolder/schema.json"), "{broken").unwrap();
    let (status, _) =
        request(&app, "POST", &format!("/repos/{repo}/schema/reload"), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = create(
        &app,
        &repo,
        json!([{"name": "rating", "value": {"type": "string", "value": "still-no"}}]),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "previous schema must remain active");

    std::fs::remove_dir_all(root).unwrap();
}
