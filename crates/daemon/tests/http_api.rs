//! Integration tests for the HTTP API: repository management, metadata CRUD,
//! field operations, reserved-field enforcement, pagination.

use std::path::PathBuf;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use metafolder_daemon::routes;
use metafolder_daemon::state::AppState;
use serde_json::{json, Value};
use tower::util::ServiceExt;
use uuid::Uuid;

fn temp_dir(prefix: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("metafolder_http_{prefix}_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn app() -> Router {
    routes::build(std::sync::Arc::new(AppState::new()))
}

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

/// Initialises a repository in a temp dir; returns (app, repo segment, root).
async fn app_with_repo(prefix: &str) -> (Router, String, PathBuf) {
    let app = app();
    let root = temp_dir(prefix);
    let (status, body) =
        request(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    let repo = body["repo_uuid"].as_str().unwrap().to_string();
    (app, repo, root)
}

/// Creates an entry and returns its full metadata object.
async fn create_metarecord(app: &Router, repo: &str, fields: Value) -> Value {
    let (status, body) =
        request(app, "POST", &format!("/repos/{repo}/metarecords"), Some(json!({"fields": fields})))
            .await;
    assert_eq!(status, StatusCode::OK, "create failed: {body}");
    body
}

// ── Health and repository management ──────────────────────────────────────────

#[tokio::test]
async fn test_health() {
    let (status, body) = request(&app(), "GET", "/health", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"status": "ok"}));
}

#[tokio::test]
async fn test_init_load_and_list_repos() {
    let (app, repo, root) = app_with_repo("repos").await;

    let (status, body) = request(&app, "GET", "/repos", None).await;
    assert_eq!(status, StatusCode::OK);
    let repos = body.as_array().unwrap();
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[0]["repo_uuid"].as_str().unwrap(), repo);
    assert_eq!(repos[0]["root"].as_str().unwrap(), root.canonicalize().unwrap().to_str().unwrap());
    assert!(repos[0]["name"].is_string());
    // The always-excluded directory, exposed so clients (e.g. the GUI
    // file-manager) can flag it without guessing the metafolder location.
    assert_eq!(
        repos[0]["internal_dir"].as_str().unwrap(),
        root.canonicalize().unwrap().join(".metafolder/internal").to_str().unwrap()
    );

    // Re-init → 409; re-load → same uuid (idempotent).
    let (status, body) =
        request(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body["error"].is_string());

    let (status, body) =
        request(&app, "POST", "/repos/load", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "load failed: {body}");
    assert_eq!(body["repo_uuid"].as_str().unwrap(), repo);

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_init_with_explicit_name_is_reflected_in_the_repo_list() {
    let app = app();
    let root = temp_dir("init_named_http");
    let (status, body) = request(
        &app,
        "POST",
        "/repos/init",
        Some(json!({"root": root.to_str().unwrap(), "name": "My Music"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");

    let (_, repos) = request(&app, "GET", "/repos", None).await;
    assert_eq!(repos[0]["name"].as_str().unwrap(), "My Music");

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_init_with_missing_root_is_bad_request() {
    let (status, body) =
        request(&app(), "POST", "/repos/init", Some(json!({"root": "/nonexistent/xyz"}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].is_string());
}

#[tokio::test]
async fn test_malformed_json_body_is_bad_request() {
    let app = app();
    let request_ = Request::builder()
        .method("POST")
        .uri("/repos/init")
        .header("content-type", "application/json")
        .body(Body::from("{not json"))
        .unwrap();
    let response = app.oneshot(request_).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(value["error"].is_string(), "errors must use the JSON error shape");
}

#[tokio::test]
async fn test_unknown_repo_is_not_found() {
    let app = app();
    let bogus = Uuid::new_v4().as_simple().to_string();
    let (status, body) = request(&app, "GET", &format!("/repos/{bogus}/metarecords"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].is_string());
}

// ── MetaRecord CRUD ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_create_get_delete_metarecord() {
    let (app, repo, root) = app_with_repo("crud").await;

    let created = create_metarecord(
        &app,
        &repo,
        json!([
            {"name": "rating", "value": {"type": "int", "value": 5}},
            {"name": "tag", "value": {"type": "string", "value": "jazz"}},
            {"name": "note", "value": {"type": "nothing"}}
        ]),
    )
    .await;
    let uuid = created["uuid"].as_str().unwrap();
    assert_eq!(created["version"], 0);
    assert_eq!(created["db_ids"][0].as_str().unwrap(), repo);
    assert_eq!(created["fields"].as_array().unwrap().len(), 3);
    assert!(created["fields"][0]["id"].is_i64(), "fields must carry their row id");

    let (status, got) = request(&app, "GET", &format!("/repos/{repo}/metarecords/{uuid}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, created);

    let (status, _) =
        request(&app, "DELETE", &format!("/repos/{repo}/metarecords/{uuid}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = request(&app, "GET", &format!("/repos/{repo}/metarecords/{uuid}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_get_unknown_record_is_not_found() {
    let (app, repo, root) = app_with_repo("get404").await;
    let bogus = Uuid::new_v4().as_simple().to_string();
    let (status, _) = request(&app, "GET", &format!("/repos/{repo}/metarecords/{bogus}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_patch_sets_field_and_bumps_version() {
    let (app, repo, root) = app_with_repo("patch").await;
    let created = create_metarecord(
        &app,
        &repo,
        json!([
            {"name": "tag", "value": {"type": "string", "value": "jazz"}},
            {"name": "tag", "value": {"type": "string", "value": "live"}}
        ]),
    )
    .await;
    let uuid = created["uuid"].as_str().unwrap();

    let (status, updated) = request(
        &app,
        "PATCH",
        &format!("/repos/{repo}/metarecords/{uuid}"),
        Some(json!({"name": "tag", "value": {"type": "string", "value": "blues"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["version"], 1);
    let tags: Vec<&Value> = updated["fields"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["name"] == "tag")
        .collect();
    assert_eq!(tags.len(), 1, "set_field must collapse the multi-map");
    assert_eq!(tags[0]["value"]["value"], "blues");

    // PATCH on a missing entry → 404.
    let bogus = Uuid::new_v4().as_simple().to_string();
    let (status, _) = request(
        &app,
        "PATCH",
        &format!("/repos/{repo}/metarecords/{bogus}"),
        Some(json!({"name": "tag", "value": {"type": "string", "value": "x"}})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_field_append_replace_delete() {
    let (app, repo, root) = app_with_repo("fields").await;
    let created = create_metarecord(
        &app,
        &repo,
        json!([{"name": "tag", "value": {"type": "string", "value": "jazz"}}]),
    )
    .await;
    let uuid = created["uuid"].as_str().unwrap();

    // Append keeps the existing row.
    let (status, updated) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/metarecords/{uuid}/fields"),
        Some(json!({"name": "tag", "value": {"type": "string", "value": "live"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let fields = updated["fields"].as_array().unwrap();
    assert_eq!(fields.len(), 2);
    let live_id = fields.iter().find(|f| f["value"]["value"] == "live").unwrap()["id"]
        .as_i64()
        .unwrap();

    // PUT replaces that single row, keeping its id.
    let (status, updated) = request(
        &app,
        "PUT",
        &format!("/repos/{repo}/metarecords/{uuid}/fields/{live_id}"),
        Some(json!({"value": {"type": "string", "value": "studio"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "put failed: {updated}");
    let replaced =
        updated["fields"].as_array().unwrap().iter().find(|f| f["id"] == live_id).unwrap();
    assert_eq!(replaced["value"]["value"], "studio");

    // PUT with a field id belonging to another entry → 404.
    let other = create_metarecord(
        &app,
        &repo,
        json!([{"name": "x", "value": {"type": "int", "value": 1}}]),
    )
    .await;
    let foreign_id = other["fields"][0]["id"].as_i64().unwrap();
    let (status, _) = request(
        &app,
        "PUT",
        &format!("/repos/{repo}/metarecords/{uuid}/fields/{foreign_id}"),
        Some(json!({"value": {"type": "int", "value": 2}})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // DELETE the row.
    let (status, _) = request(
        &app,
        "DELETE",
        &format!("/repos/{repo}/metarecords/{uuid}/fields/{live_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, got) = request(&app, "GET", &format!("/repos/{repo}/metarecords/{uuid}"), None).await;
    assert_eq!(got["fields"].as_array().unwrap().len(), 1);

    std::fs::remove_dir_all(root).unwrap();
}

// ── Reserved fields ───────────────────────────────────────────────────────────

#[tokio::test]
async fn test_reserved_fields_require_force() {
    let (app, repo, root) = app_with_repo("reserved").await;

    // Creating with an mfr_* field without force → 400.
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/metarecords"),
        Some(json!({"fields": [{"name": "mfr_size", "value": {"type": "int", "value": 10}}]})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "got: {body}");

    // Same with force → ok.
    let (status, created) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/metarecords"),
        Some(json!({
            "force": true,
            "fields": [{"name": "mfr_size", "value": {"type": "int", "value": 10}}]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "got: {created}");
    let uuid = created["uuid"].as_str().unwrap();

    // PATCH on mfr_* without force → 400; with force → ok.
    let (status, _) = request(
        &app,
        "PATCH",
        &format!("/repos/{repo}/metarecords/{uuid}"),
        Some(json!({"name": "mfr_size", "value": {"type": "int", "value": 20}})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = request(
        &app,
        "PATCH",
        &format!("/repos/{repo}/metarecords/{uuid}"),
        Some(json!({"name": "mfr_size", "value": {"type": "int", "value": 20}, "force": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Unknown mf_* name is always rejected.
    let (status, _) = request(
        &app,
        "PATCH",
        &format!("/repos/{repo}/metarecords/{uuid}"),
        Some(json!({"name": "mf_typo", "value": {"type": "bool", "value": true}, "force": true})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Deleting a reserved field row requires force in the body.
    let (_, got) = request(&app, "GET", &format!("/repos/{repo}/metarecords/{uuid}"), None).await;
    let field_id = got["fields"][0]["id"].as_i64().unwrap();
    let (status, _) = request(
        &app,
        "DELETE",
        &format!("/repos/{repo}/metarecords/{uuid}/fields/{field_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = request(
        &app,
        "DELETE",
        &format!("/repos/{repo}/metarecords/{uuid}/fields/{field_id}"),
        Some(json!({"force": true})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    std::fs::remove_dir_all(root).unwrap();
}

// ── TreeRef validation through the API ────────────────────────────────────────

#[tokio::test]
async fn test_tree_ref_validation_is_bad_request() {
    let (app, repo, root) = app_with_repo("treeref").await;
    let parent = Uuid::new_v4().as_simple().to_string();
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/metarecords"),
        Some(json!({"fields": [{"name": "parent",
            "value": {"type": "tree_ref", "value": {"parent": parent, "name": "x"}}}]})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "got: {body}");
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_tree_resolve_endpoint() {
    let (app, repo, root_dir) = app_with_repo("treeresolve").await;
    // A custom (non-reserved) TreeRef field: the endpoint is general, mfr_path
    // is only the default. The repo already owns the mfr_path root.
    let treeref = |parent: Option<&str>, name: &str| {
        json!([{"name": "cat",
            "value": {"type": "tree_ref", "value": {"parent": parent, "name": name}}}])
    };
    let uuid = |m: Value| m["uuid"].as_str().unwrap().to_string();
    let all = uuid(create_metarecord(&app, &repo, treeref(None, "all")).await);
    let music = uuid(create_metarecord(&app, &repo, treeref(Some(&all), "music")).await);
    let jazz = uuid(create_metarecord(&app, &repo, treeref(Some(&music), "jazz")).await);

    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/tree/resolve"),
        Some(json!({"field": "cat", "uuids": [jazz, music]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "got: {body}");
    assert_eq!(body[&jazz], json!(["all/music/jazz"]));
    assert_eq!(body[&music], json!(["all/music"]));

    // `field` defaults to mfr_path, which `jazz` does not carry → empty array.
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/tree/resolve"),
        Some(json!({"uuids": [jazz]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body[&jazz], json!([]));

    std::fs::remove_dir_all(root_dir).unwrap();
}

#[tokio::test]
async fn test_batch_get_metarecords() {
    let (app, repo, root_dir) = app_with_repo("batchget").await;
    let uuid = |m: Value| m["uuid"].as_str().unwrap().to_string();
    let a = uuid(
        create_metarecord(&app, &repo, json!([{"name": "rating", "value": {"type": "int", "value": 5}}]))
            .await,
    );
    let b = uuid(
        create_metarecord(&app, &repo, json!([{"name": "genre", "value": {"type": "string", "value": "jazz"}}]))
            .await,
    );
    let missing = Uuid::new_v4().as_simple().to_string();

    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/metarecords/batch"),
        Some(json!({"uuids": [a, b, missing]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "got: {body}");
    assert_eq!(body[&a]["uuid"], json!(a));
    assert_eq!(body[&b]["fields"][0]["name"], json!("genre"));
    assert!(body.get(&missing).is_none(), "unknown uuid is omitted");

    std::fs::remove_dir_all(root_dir).unwrap();
}

// ── Listing and pagination ────────────────────────────────────────────────────

#[tokio::test]
async fn test_list_metadata_plain_and_paginated() {
    let (app, repo, root) = app_with_repo("page").await;
    for i in 0..5 {
        create_metarecord(&app, &repo, json!([{"name": "i", "value": {"type": "int", "value": i}}]))
            .await;
    }

    // Without limit: plain array (5 entries + the filesystem root entry).
    let (status, body) = request(&app, "GET", &format!("/repos/{repo}/metarecords"), None).await;
    assert_eq!(status, StatusCode::OK);
    let all: Vec<String> =
        body.as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
    assert_eq!(all.len(), 6);
    let mut sorted = all.clone();
    sorted.sort();
    assert_eq!(all, sorted, "must be sorted by UUID");

    // Paginated: pages of 4, then 2, then done.
    let (status, page1) =
        request(&app, "GET", &format!("/repos/{repo}/metarecords?limit=4"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page1["results"].as_array().unwrap().len(), 4);
    let cursor = page1["next_cursor"].as_str().expect("next_cursor expected").to_string();

    let (status, page2) =
        request(&app, "GET", &format!("/repos/{repo}/metarecords?limit=4&cursor={cursor}"), None)
            .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page2["results"].as_array().unwrap().len(), 2);
    assert!(page2["next_cursor"].is_null());

    let combined: Vec<String> = page1["results"]
        .as_array()
        .unwrap()
        .iter()
        .chain(page2["results"].as_array().unwrap())
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(combined, all);

    // Invalid cursor → 400.
    let (status, _) =
        request(&app, "GET", &format!("/repos/{repo}/metarecords?limit=4&cursor=zzz"), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    std::fs::remove_dir_all(root).unwrap();
}
