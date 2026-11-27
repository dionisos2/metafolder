//! Tests for the user schema system (doc "Schema"): file loading and
//! validation, delta validation on writes, check/reload endpoints.

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

fn app() -> Router {
    routes::build(Arc::new(AppState::new(common::settings())))
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
async fn setup_with_schema(prefix: &str, schema: Value) -> (Router, String, TempDir) {
    let root = TempDir::new(&format!("sch_{prefix}"));
    {
        let first = app();
        common::seed_config(&root);
        let (status, body) =
            request(&first, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()})))
                .await;
        assert_eq!(status, StatusCode::OK, "init failed: {body}");
    } // First daemon dropped: exclusive lock released.

    std::fs::write(
        root.join(".metafolder/config/schema.json"),
        serde_json::to_string_pretty(&schema).unwrap(),
    )
    .unwrap();

    let second = app();
    let (status, body) =
        request(&second, "POST", "/repos/load", Some(json!({"root": root.to_str().unwrap()})))
            .await;
    assert_eq!(status, StatusCode::OK, "load failed: {body}");
    // A load returns before the repository is warm, and a warming repository
    // answers 503 to everything but its own state (doc "Creating and loading a repository").
    // Waiting for its `load` task is what a client does.
    wait_ready(&second, body["repo_uuid"].as_str().unwrap()).await;
    let repo = body["repo_uuid"].as_str().unwrap().to_string();
    (second, repo, root)
}

/// Polls until the repository serves data, as a client waiting on its `load`
/// task would.
async fn wait_ready(app: &Router, repo: &str) {
    for _ in 0..500 {
        let (status, _) = request(app, "GET", &format!("/repos/{repo}/schema"), None).await;
        if status != StatusCode::SERVICE_UNAVAILABLE {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("repository {repo} never became ready");
}

async fn create(app: &Router, repo: &str, fields: Value) -> (StatusCode, Value) {
    request(app, "POST", &format!("/repos/{repo}/metarecords"), Some(json!({"fields": fields})))
        .await
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
async fn test_field_catalog_includes_schema_declared_fields() {
    // The schema declares `rating: int` and `name: string`; no metarecord carries
    // them yet. They must still appear in GET /fields (schema-priority merge),
    // alongside a purely data-derived field.
    let (app, repo, root) = setup_with_schema("catalog", film_schema()).await;
    let (status, _) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/metarecords"),
        Some(json!({"fields": [{"name": "tag", "value": {"type": "string", "value": "x"}}]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = request(&app, "GET", &format!("/repos/{repo}/fields"), None).await;
    assert_eq!(status, StatusCode::OK);
    let pairs: Vec<(String, String)> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["name"].as_str().unwrap().to_string(), e["type"].as_str().unwrap().to_string()))
        .collect();
    assert!(pairs.contains(&("rating".into(), "int".into())), "schema-only int field: {pairs:?}");
    assert!(
        pairs.contains(&("name".into(), "string".into())),
        "schema-only string field: {pairs:?}"
    );
    assert!(pairs.contains(&("tag".into(), "string".into())), "data field: {pairs:?}");

    // The `?type=` filter is applied after the merge, so a schema-only field of
    // that type survives it.
    let (status, body) =
        request(&app, "GET", &format!("/repos/{repo}/fields?type=int"), None).await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> =
        body.as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"rating"), "schema-only int after filter: {names:?}");
    assert!(body.as_array().unwrap().iter().all(|e| e["type"] == "int"), "{body}");

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_edit_nothing_field_in_place_under_min_max_one() {
    // The GUI's in-place field edit: a `path` constrained to exactly one row
    // (min=1, max=1) and currently `Nothing` must be settable to a real value by
    // replacing the value in one write (PATCH …/fields/:name) — delete+add is impossible
    // here (delete would drop below min, add would exceed max). A `Nothing` row
    // counts toward cardinality, so the row count never changes.
    let schema = json!({
        "version": 1,
        "groups": [{"targets": ["tag"], "constraints": [
            {"field": "path", "type": "tree_ref", "min": 1, "max": 1}
        ]}]
    });
    let (app, repo, root) = setup_with_schema("editnothing", schema).await;

    let (status, m) = create(
        &app,
        &repo,
        json!([{"name": "mf_schema", "value": {"type": "string", "value": "tag"}},
               {"name": "path", "value": {"type": "nothing"}}]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create with path=nothing must be valid: {m}");
    let uuid = m["uuid"].as_str().unwrap();
    let path_url = format!("/repos/{repo}/metarecords/{uuid}/fields/path");
    let music = json!({"type": "tree_ref", "value": {"parent": null, "name": "music"}});

    // nothing → tree_ref, in one write (still one row).
    let (status, body) = request(
        &app,
        "PATCH",
        &path_url,
        Some(json!({"value": {"type": "nothing"}, "to": music.clone()})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "edit nothing→tree_ref must be accepted: {body}");
    let path = body["fields"].as_array().unwrap().iter().find(|f| f["name"] == "path").unwrap();
    assert_eq!(path["value"]["type"], "tree_ref");

    // tree_ref → nothing, in place (still one row, min/max satisfied).
    let (status, body) =
        request(&app, "PATCH", &path_url, Some(json!({"value": music, "to": {"type": "nothing"}})))
            .await;
    assert_eq!(status, StatusCode::OK, "edit tree_ref→nothing must be accepted: {body}");
    let path = body["fields"].as_array().unwrap().iter().find(|f| f["name"] == "path").unwrap();
    assert_eq!(path["value"]["type"], "nothing");

    let _ = uuid;
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
    let root = TempDir::new("sch_none");
    common::seed_config(&root);
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
        let root = TempDir::new("sch_bad");
        {
            let first = app();
            common::seed_config(&root);
            request(&first, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()})))
                .await;
        }
        std::fs::write(root.join(".metafolder/config/schema.json"), schema.to_string()).unwrap();
        let second = app();
        let (status, body) =
            request(&second, "POST", "/repos/load", Some(json!({"root": root.to_str().unwrap()})))
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
        create(&app, &repo, json!([{"name": "rating", "value": {"type": "int", "value": 5}}]))
            .await;
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
        create(&app, &repo, json!([{"name": "rating", "value": {"type": "int", "value": 1}}]))
            .await;
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
    let name =
        &film_now["fields"].as_array().unwrap().iter().find(|f| f["name"] == "name").unwrap()
            ["value"];
    let (status, body) = request(
        &app,
        "DELETE",
        &format!("/repos/{repo}/metarecords/{film_uuid}/fields/name"),
        Some(json!({"value": name})),
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

    // Declaring a type via mf_schema adds what it requires and lacks (film
    // requires one name: a `nothing` row), so it does not fail here.
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
        create(
            &app,
            &repo,
            json!([{"name": "genre", "value": {"type": "string", "value": genre}}]),
        )
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

/// A write to many records is checked once, not record by record
/// (doc "Mass operations in the log"): a constraint is still held.
#[tokio::test]
async fn test_a_mass_add_is_held_to_the_schema() {
    let (app, repo, root) = setup_with_schema("massadd", film_schema()).await;
    let film = |name: &str| {
        json!([
            {"name": "mf_schema", "value": {"type": "string", "value": "film"}},
            {"name": "name", "value": {"type": "string", "value": name}},
            {"name": "rating", "value": {"type": "int", "value": 5}}
        ])
    };
    for name in ["Alien", "Heat"] {
        let (status, body) = create(&app, &repo, film(name)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let add = |field: &str, value: Value| {
        json!({
            "query": {"type": "is_present", "field": "mf_schema"},
            "name": field,
            "value": value
        })
    };
    let url = format!("/repos/{repo}/query/fields/add");
    // A second rating on a film: the maximum.
    let (status, body) =
        request(&app, "POST", &url, Some(add("rating", json!({"type": "int", "value": 4})))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["violations"][0]["kind"], "max_cardinality");
    // A rating of the wrong type: the type.
    let (status, body) =
        request(&app, "POST", &url, Some(add("rating", json!({"type": "string", "value": "x"}))))
            .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    // A field no constraint names, on the same records: written.
    let (status, body) =
        request(&app, "POST", &url, Some(add("genre", json!({"type": "string", "value": "sf"}))))
            .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["updated"], 2);
    // And a removal that leaves a film without its name: the minimum.
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query/fields/remove"),
        Some(add("name", json!({"type": "string", "value": "Alien"}))),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["violations"][0]["kind"], "min_cardinality");
    std::fs::remove_dir_all(root).unwrap();
}

/// A batch is judged by what it leaves (doc "Mass operations in the log"): a
/// film's one rating replaced by a removal then an addition is within its
/// maximum, though the addition alone would not be; a second rating is not.
#[tokio::test]
async fn test_a_batch_is_held_to_the_schema_as_it_ends() {
    let (app, repo, root) = setup_with_schema("batchend", film_schema()).await;
    for name in ["Alien", "Heat"] {
        let fields = json!([
            {"name": "mf_schema", "value": {"type": "string", "value": "film"}},
            {"name": "name", "value": {"type": "string", "value": name}},
            {"name": "rating", "value": {"type": "int", "value": 5}}
        ]);
        let (status, body) = create(&app, &repo, fields).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let films = json!({"type": "is_present", "field": "mf_schema"});
    let url = format!("/repos/{repo}/query/fields/batch");
    let rating = |n: i64| json!({"type": "int", "value": n});
    // The addition first: the counters are read once the batch is done.
    let (status, body) = request(
        &app,
        "POST",
        &url,
        Some(json!({"ops": [
            {"op": "add", "query": films, "name": "rating", "value": rating(4)},
            {"op": "remove", "query": films, "name": "rating", "value": rating(5)},
        ]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // A second rating on each film: over the maximum.
    let (status, body) = request(
        &app,
        "POST",
        &url,
        Some(json!({"ops": [
            {"op": "add", "query": films, "name": "rating", "value": rating(3)},
        ]})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["violations"][0]["kind"], "max_cardinality");
    // A record that is no film holds no maximum: two ratings are fine.
    let (status, body) =
        create(&app, &repo, json!([{"name": "rating", "value": {"type": "int", "value": 1}}]))
            .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let plain = json!({"type": "uuid_in", "uuids": [body["uuid"]]});
    let (status, body) = request(
        &app,
        "POST",
        &url,
        Some(json!({"ops": [
            {"op": "add", "query": plain, "name": "rating", "value": rating(2)},
        ]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["results"][0]["updated"], 1);
    std::fs::remove_dir_all(root).unwrap();
}

// ── Completion: a type's required fields written for it ───────────────────────

/// A film requires a name (no default), a year (default 2000), a genre
/// (default "?") and a parent node (a reference type: its default is not
/// written); every metarecord requires an origin (default "me"). A `serie`
/// requires two episodes, which no default can give: a field never holds a
/// value twice.
fn completion_schema() -> Value {
    json!({
        "version": 1,
        "groups": [
            {"targets": "*", "constraints": [
                {"field": "origin", "type": "string", "min": 1, "default": "me"}
            ]},
            {"targets": ["film"], "constraints": [
                {"field": "name", "type": "string", "min": 1, "max": 1},
                {"field": "year", "type": "int", "min": 1, "default": 2000},
                {"field": "genre", "type": "string", "min": 1, "default": "?"},
                {"field": "parent", "type": "tree_ref", "min": 1,
                 "default": {"parent": null, "name": "films"}},
                {"field": "rating", "type": "int", "max": 1}
            ]},
            {"targets": ["serie"], "constraints": [
                {"field": "episode", "type": "string", "min": 2, "default": "?"}
            ]}
        ]
    })
}

fn string(s: &str) -> Value {
    json!({"type": "string", "value": s})
}

/// The values of `field` in a metarecord's JSON, in order.
fn values(record: &Value, field: &str) -> Vec<Value> {
    record["fields"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["name"] == field)
        .map(|f| f["value"].clone())
        .collect()
}

async fn get(app: &Router, repo: &str, uuid: &str) -> Value {
    let (status, body) =
        request(app, "GET", &format!("/repos/{repo}/metarecords/{uuid}"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

/// What a film gets when it is given nothing: each required field at its
/// default, `nothing` without one or for a reference type.
fn assert_completed_film(record: &Value) {
    assert_eq!(values(record, "name"), vec![json!({"type": "nothing"})], "{record}");
    assert_eq!(values(record, "year"), vec![json!({"type": "int", "value": 2000})], "{record}");
    assert_eq!(values(record, "genre"), vec![string("?")], "{record}");
    assert_eq!(values(record, "parent"), vec![json!({"type": "nothing"})], "{record}");
    assert!(values(record, "rating").is_empty(), "{record}");
}

#[tokio::test]
async fn test_a_creation_gets_the_required_fields_it_lacks() {
    let (app, repo, root) = setup_with_schema("complete_create", completion_schema()).await;
    let (status, film) =
        create(&app, &repo, json!([{"name": "mf_schema", "value": string("film")}])).await;
    assert_eq!(status, StatusCode::OK, "{film}");
    assert_completed_film(&film);
    assert_eq!(values(&film, "origin"), vec![string("me")]);

    // What is given is kept, and only what lacks is added.
    let (status, film) = create(
        &app,
        &repo,
        json!([{"name": "mf_schema", "value": string("film")},
               {"name": "name", "value": string("Alien")},
               {"name": "genre", "value": string("sf")}]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{film}");
    assert_eq!(values(&film, "name"), vec![string("Alien")]);
    assert_eq!(values(&film, "genre"), vec![string("sf")]);
    assert_eq!(values(&film, "year"), vec![json!({"type": "int", "value": 2000})]);

    // A minimum of two the record lacks both rows of: one default row is all
    // a completion can add, and the creation is refused.
    let (status, body) =
        create(&app, &repo, json!([{"name": "mf_schema", "value": string("serie")}])).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["violations"][0]["kind"], "min_cardinality", "{body}");
    assert_eq!(body["violations"][0]["field"], "episode", "{body}");

    // An untyped metarecord gets what every metarecord requires.
    let (status, plain) =
        create(&app, &repo, json!([{"name": "note", "value": string("x")}])).await;
    assert_eq!(status, StatusCode::OK, "{plain}");
    assert_eq!(values(&plain, "origin"), vec![string("me")]);
    assert!(values(&plain, "name").is_empty());
    drop(root);
}

#[tokio::test]
async fn test_every_way_of_creating_completes() {
    let (app, repo, root) = setup_with_schema("complete_create_all", completion_schema()).await;
    let film = json!([{"name": "mf_schema", "value": string("film")}]);
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/metarecords/bulk"),
        Some(json!({"metarecords": [{"fields": film}]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_completed_film(&get(&app, &repo, body["uuids"][0].as_str().unwrap()).await);

    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query/fields/batch"),
        Some(json!({"ops": [{"op": "create", "fields": film}]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_completed_film(&get(&app, &repo, body["results"][0]["uuid"].as_str().unwrap()).await);
    drop(root);
}

#[tokio::test]
async fn test_giving_a_type_adds_its_required_fields() {
    let (app, repo, root) = setup_with_schema("complete_type", completion_schema()).await;
    let (_, plain) = create(&app, &repo, json!([{"name": "note", "value": string("x")}])).await;
    let uuid = plain["uuid"].as_str().unwrap();
    let (status, film) = request(
        &app,
        "PUT",
        &format!("/repos/{repo}/metarecords/{uuid}/fields/mf_schema"),
        Some(json!({"value": string("film")})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{film}");
    assert_completed_film(&get(&app, &repo, uuid).await);

    // Taking the type away never fails, and leaves the fields.
    let (status, body) = request(
        &app,
        "DELETE",
        &format!("/repos/{repo}/metarecords/{uuid}/fields/mf_schema"),
        Some(json!({"value": string("film")})),
    )
    .await;
    assert!(status.is_success(), "{body}");
    assert_eq!(values(&get(&app, &repo, uuid).await, "year").len(), 1);
    drop(root);
}

#[tokio::test]
async fn test_a_type_is_refused_to_a_record_that_breaks_it() {
    let (app, repo, root) = setup_with_schema("complete_refused", completion_schema()).await;
    let int = |n: i64| json!({"type": "int", "value": n});
    for (rows, kind) in [
        (
            json!([{"name": "rating", "value": int(4)}, {"name": "rating", "value": int(5)}]),
            "max_cardinality",
        ),
        (json!([{"name": "year", "value": string("1979")}]), "type"),
    ] {
        let (status, plain) = create(&app, &repo, rows).await;
        assert_eq!(status, StatusCode::OK, "{plain}");
        let uuid = plain["uuid"].as_str().unwrap();
        let (status, body) = request(
            &app,
            "PUT",
            &format!("/repos/{repo}/metarecords/{uuid}/fields/mf_schema"),
            Some(json!({"value": string("film")})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["violations"][0]["kind"], kind, "{body}");
        // Nothing written: neither the type nor what it would have added.
        let now = get(&app, &repo, uuid).await;
        assert!(values(&now, "mf_schema").is_empty(), "{now}");
        assert!(values(&now, "genre").is_empty(), "{now}");
    }
    drop(root);
}

#[tokio::test]
async fn test_a_type_given_to_many_records_completes_each() {
    let (app, repo, root) = setup_with_schema("complete_mass", completion_schema()).await;
    let mut uuids = Vec::new();
    for n in ["a", "b"] {
        let (_, plain) = create(&app, &repo, json!([{"name": "batch", "value": string(n)}])).await;
        uuids.push(plain["uuid"].as_str().unwrap().to_string());
    }
    let batch = json!({"type": "is_present", "field": "batch"});
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query/fields/add"),
        Some(json!({"query": batch, "name": "mf_schema", "value": string("film")})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for uuid in &uuids {
        assert_completed_film(&get(&app, &repo, uuid).await);
    }

    // A replacement of one type by another gives the new one.
    let (_, other) = create(
        &app,
        &repo,
        json!([{"name": "mf_schema", "value": string("draft")}, {"name": "x", "value": string("1")}]),
    )
    .await;
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query/fields/replace"),
        Some(json!({"query": {"type": "is_present", "field": "x"}, "name": "mf_schema",
                    "value": string("draft"), "to": string("film")})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_completed_film(&get(&app, &repo, other["uuid"].as_str().unwrap()).await);

    // In a batch, and refused whole when one record breaks the type.
    let (_, broken) = create(
        &app,
        &repo,
        json!([{"name": "y", "value": string("1")},
               {"name": "rating", "value": {"type": "int", "value": 1}},
               {"name": "rating", "value": {"type": "int", "value": 2}}]),
    )
    .await;
    let (_, fine) = create(&app, &repo, json!([{"name": "y", "value": string("2")}])).await;
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query/fields/batch"),
        Some(json!({"ops": [{"op": "add", "query": {"type": "is_present", "field": "y"},
                             "name": "mf_schema", "value": string("film")}]})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    for uuid in [&broken["uuid"], &fine["uuid"]] {
        let now = get(&app, &repo, uuid.as_str().unwrap()).await;
        assert!(values(&now, "mf_schema").is_empty(), "{now}");
        assert!(values(&now, "genre").is_empty(), "{now}");
    }
    drop(root);
}

// ── Check and reload ──────────────────────────────────────────────────────────

/// Replaces the schema by [`film_schema`] and reloads it: the data already
/// there is not checked (doc "Delta validation").
async fn tighten_to_film_schema(app: &Router, repo: &str, root: &std::path::Path) {
    std::fs::write(root.join(".metafolder/config/schema.json"), film_schema().to_string()).unwrap();
    let (status, body) = request(app, "POST", &format!("/repos/{repo}/schema/reload"), None).await;
    assert_eq!(status, StatusCode::OK, "reload failed: {body}");
}

#[tokio::test]
async fn test_schema_check_reports_existing_violations() {
    // Data written while the schema says nothing, then the schema tightened:
    // a film of two ratings and no name.
    let (app, repo, root) = setup_with_schema("check", json!({"version": 1, "groups": []})).await;
    create(
        &app,
        &repo,
        json!([{"name": "mf_schema", "value": {"type": "string", "value": "film"}},
               {"name": "rating", "value": {"type": "int", "value": 1}},
               {"name": "rating", "value": {"type": "int", "value": 2}}]),
    )
    .await;
    tighten_to_film_schema(&app, &repo, &root).await;

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
async fn test_schema_check_limit_caps_violations() {
    let (app, repo, root) =
        setup_with_schema("checklimit", json!({"version": 1, "groups": []})).await;
    // Three films, each violating twice once the schema is tightened (two
    // ratings → max_cardinality; no name → min_cardinality), so well over a
    // small cap.
    for _ in 0..3 {
        create(
            &app,
            &repo,
            json!([{"name": "mf_schema", "value": {"type": "string", "value": "film"}},
                   {"name": "rating", "value": {"type": "int", "value": 1}},
                   {"name": "rating", "value": {"type": "int", "value": 2}}]),
        )
        .await;
    }
    tighten_to_film_schema(&app, &repo, &root).await;

    // Unbounded (no limit): every violation, not truncated.
    let (_, full) =
        request(&app, "POST", &format!("/repos/{repo}/schema/check"), Some(json!({}))).await;
    assert!(full["violations"].as_array().unwrap().len() > 2, "expected several: {full}");
    assert_eq!(full["truncated"], json!(false));

    // Capped: at most `limit` violations returned, flagged truncated.
    let (status, body) =
        request(&app, "POST", &format!("/repos/{repo}/schema/check"), Some(json!({"limit": 2})))
            .await;
    assert_eq!(status, StatusCode::OK, "got: {body}");
    assert_eq!(body["violations"].as_array().unwrap().len(), 2, "capped: {body}");
    assert_eq!(body["truncated"], json!(true));

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
    std::fs::write(root.join(".metafolder/config/schema.json"), film_schema().to_string()).unwrap();
    let (status, body) = request(&app, "POST", &format!("/repos/{repo}/schema/reload"), None).await;
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
    std::fs::write(root.join(".metafolder/config/schema.json"), "{broken").unwrap();
    let (status, _) = request(&app, "POST", &format!("/repos/{repo}/schema/reload"), None).await;
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

/// A bulk create is one transaction: a schema violation anywhere in the batch
/// rolls all of it back, and the `violations` array names the offending
/// metarecord (doc "Metarecord endpoints").
#[tokio::test]
async fn test_bulk_create_violation_rolls_the_whole_batch_back() {
    let (app, repo, root) = setup_with_schema("bulk_violation", film_schema()).await;
    let good = uuid::Uuid::new_v4().as_simple().to_string();
    let bad = uuid::Uuid::new_v4().as_simple().to_string();

    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/metarecords/bulk"),
        Some(json!({"metarecords": [
            {"uuid": good, "fields": [
                {"name": "mf_schema", "value": {"type": "string", "value": "film"}},
                {"name": "name", "value": {"type": "string", "value": "Alien"}},
                {"name": "rating", "value": {"type": "int", "value": 5}}
            ]},
            // Two `rating` rows on a film: the schema caps it at one. A
            // constraint only the schema knows — the field-type registry is
            // happy with two int rows.
            {"uuid": bad, "fields": [
                {"name": "mf_schema", "value": {"type": "string", "value": "film"}},
                {"name": "name", "value": {"type": "string", "value": "Solaris"}},
                {"name": "rating", "value": {"type": "int", "value": 4}},
                {"name": "rating", "value": {"type": "int", "value": 2}}
            ]}
        ]})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "the batch must be refused: {body}");
    assert_eq!(
        body["violations"][0]["kind"], "max_cardinality",
        "violations array expected, got {body}"
    );
    assert_eq!(body["violations"][0]["metarecord_uuid"], bad, "the violation names its record");

    for uuid in [&good, &bad] {
        let (status, _) =
            request(&app, "GET", &format!("/repos/{repo}/metarecords/{uuid}"), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "nothing of the batch is written");
    }

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_the_shipped_schema_places_a_tag_under_several_parents() {
    // The shipped default (seeded into every new repository) declares a tag's
    // position as a hierarchy (doc "Hierarchy references"): a marathon is a
    // race and an event, two `path` rows of one name.
    let shipped = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("default-config/repo-default/schema.json"),
    )
    .unwrap();
    let schema: Value = serde_json::from_str(&shipped).unwrap();
    let (app, repo, root) = setup_with_schema("shippedhier", schema).await;
    let tag = |path: Value| {
        json!([{"name": "mf_schema", "value": {"type": "string", "value": "tag"}},
               {"name": "path", "value": path}])
    };
    let node = |parent: &str, name: &str| {
        let parent = if parent.is_empty() { Value::Null } else { json!(parent) };
        json!({"type": "hierarchy_ref", "value": {"parent": parent, "name": name}})
    };
    let (status, sport) = create(&app, &repo, tag(node("", "sport"))).await;
    assert_eq!(status, StatusCode::OK, "{sport}");
    let (_, event) = create(&app, &repo, tag(node("", "évènement"))).await;
    let (sport, event) = (sport["uuid"].as_str().unwrap(), event["uuid"].as_str().unwrap());
    let mut fields = tag(node(sport, "maraton"));
    fields.as_array_mut().unwrap().push(json!({"name": "path", "value": node(event, "maraton")}));
    let (status, body) = create(&app, &repo, fields).await;
    assert_eq!(status, StatusCode::OK, "a tag of two parents is valid: {body}");
    drop(root);
}
