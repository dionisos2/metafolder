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
    assert_eq!(body["status"], "ok");
    assert!(body["version"].is_string());
    assert_eq!(body["repos"], json!(0));
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
async fn test_loading_a_duplicate_name_is_a_conflict() {
    let app = app();
    // Two distinct repositories sharing one explicit name.
    let root_a = temp_dir("dupname_a");
    let root_b = temp_dir("dupname_b");
    let (status, _) = request(
        &app,
        "POST",
        "/repos/init",
        Some(json!({"root": root_a.to_str().unwrap(), "name": "Shared"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // A second repo with the same name must be rejected (names are unique among
    // loaded repos, so `mf -n <name>` resolves to one UUID).
    let (status, body) = request(
        &app,
        "POST",
        "/repos/init",
        Some(json!({"root": root_b.to_str().unwrap(), "name": "Shared"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "duplicate name should be 409: {body}");
    assert!(body["error"].as_str().unwrap().contains("Shared"));

    // Only the first repo is loaded.
    let (_, repos) = request(&app, "GET", "/repos", None).await;
    assert_eq!(repos.as_array().unwrap().len(), 1);

    std::fs::remove_dir_all(root_a).unwrap();
    std::fs::remove_dir_all(root_b).unwrap();
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
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{bogus}/query"),
        Some(json!({"query": {"type": "is_present", "field": "x"}})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].is_string());
}

// ── MetaRecord CRUD ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_field_by_id_get_rename_and_delete() {
    let (app, repo, root) = app_with_repo("field_by_id").await;
    let created = create_metarecord(
        &app,
        &repo,
        json!([{"name": "rating", "value": {"type": "int", "value": 5}}]),
    )
    .await;
    let id = created["fields"][0]["id"].as_i64().unwrap();

    // GET one row by its id.
    let (status, row) = request(&app, "GET", &format!("/repos/{repo}/fields/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(row["name"].as_str().unwrap(), "rating");
    assert_eq!(row["value"]["value"].as_i64().unwrap(), 5);

    // PATCH renames and revalues in place, keeping the id.
    let (status, updated) = request(
        &app,
        "PATCH",
        &format!("/repos/{repo}/fields/{id}"),
        Some(json!({"name": "score", "value": {"type": "int", "value": 9}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "rename failed: {updated}");
    let fields = updated["fields"].as_array().unwrap();
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0]["id"].as_i64().unwrap(), id, "row id preserved on rename");
    assert_eq!(fields[0]["name"].as_str().unwrap(), "score");
    assert_eq!(fields[0]["value"]["value"].as_i64().unwrap(), 9);

    // A value whose type clashes with the new name's type is rejected.
    let (status, _) = request(
        &app,
        "PATCH",
        &format!("/repos/{repo}/fields/{id}"),
        Some(json!({"value": {"type": "string", "value": "nope"}})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "type must be validated against the name");

    // DELETE by id.
    let (status, _) = request(&app, "DELETE", &format!("/repos/{repo}/fields/{id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = request(&app, "GET", &format!("/repos/{repo}/fields/{id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // An unknown id is 404.
    let (status, _) = request(&app, "GET", &format!("/repos/{repo}/fields/999999"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    std::fs::remove_dir_all(root).unwrap();
}

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
        "PUT",
        &format!("/repos/{repo}/metarecords/{uuid}/fields/tag"),
        Some(json!({"value": {"type": "string", "value": "blues"}})),
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
        "PUT",
        &format!("/repos/{repo}/metarecords/{bogus}/fields/tag"),
        Some(json!({"value": {"type": "string", "value": "x"}})),
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

    // PATCH replaces that single row by id, keeping its id.
    let (status, updated) = request(
        &app,
        "PATCH",
        &format!("/repos/{repo}/fields/{live_id}"),
        Some(json!({"value": {"type": "string", "value": "studio"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "patch failed: {updated}");
    let replaced =
        updated["fields"].as_array().unwrap().iter().find(|f| f["id"] == live_id).unwrap();
    assert_eq!(replaced["value"]["value"], "studio");

    // An unknown field id → 404 (the id is repo-global, no metarecord scope).
    let (status, _) = request(
        &app,
        "PATCH",
        &format!("/repos/{repo}/fields/999999"),
        Some(json!({"value": {"type": "int", "value": 2}})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // DELETE the row.
    let (status, _) = request(
        &app,
        "DELETE",
        &format!("/repos/{repo}/fields/{live_id}"),
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
        "PUT",
        &format!("/repos/{repo}/metarecords/{uuid}/fields/mfr_size"),
        Some(json!({"value": {"type": "int", "value": 20}})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = request(
        &app,
        "PUT",
        &format!("/repos/{repo}/metarecords/{uuid}/fields/mfr_size"),
        Some(json!({"value": {"type": "int", "value": 20}, "force": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Unknown mf_* name is always rejected.
    let (status, _) = request(
        &app,
        "PUT",
        &format!("/repos/{repo}/metarecords/{uuid}/fields/mf_typo"),
        Some(json!({"value": {"type": "bool", "value": true}, "force": true})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Deleting a reserved field row requires force in the body.
    let (_, got) = request(&app, "GET", &format!("/repos/{repo}/metarecords/{uuid}"), None).await;
    let field_id = got["fields"][0]["id"].as_i64().unwrap();
    let (status, _) = request(
        &app,
        "DELETE",
        &format!("/repos/{repo}/fields/{field_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = request(
        &app,
        "DELETE",
        &format!("/repos/{repo}/fields/{field_id}"),
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
        &format!("/repos/{repo}/query/fields/resolve-tree"),
        Some(json!({"query": {"type": "uuid_in", "uuids": [jazz, music]}, "field": "cat"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "got: {body}");
    assert_eq!(body[&jazz], json!(["all/music/jazz"]));
    assert_eq!(body[&music], json!(["all/music"]));

    // `field` defaults to mfr_path, which `jazz` does not carry → empty array.
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query/fields/resolve-tree"),
        Some(json!({"query": {"type": "uuid_in", "uuids": [jazz]}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body[&jazz], json!([]));

    std::fs::remove_dir_all(root_dir).unwrap();
}

#[tokio::test]
async fn test_read_a_named_set_via_uuid_in() {
    let (app, repo, root_dir) = app_with_repo("uuidinread").await;
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

    // Reading several metarecords by name = a uuid_in query (no batch endpoint).
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": {"type": "uuid_in", "uuids": [a, b, missing]}, "select": "*"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "got: {body}");
    let objs = body.as_array().unwrap();
    assert_eq!(objs.len(), 2, "unknown uuid is omitted");
    let by_uuid = |u: &str| objs.iter().find(|o| o["uuid"] == json!(u)).unwrap();
    assert_eq!(by_uuid(&a)["uuid"], json!(a));
    assert_eq!(by_uuid(&b)["fields"][0]["name"], json!("genre"));

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

    // "List all" is a match-all query (is_unknown on a never-used field matches
    // the whole universe). Without limit: plain array (5 + the root entry).
    let all_q = json!({"type": "is_unknown", "field": "__never__"});
    let query = |body: Value| {
        let app = app.clone();
        let repo = repo.clone();
        async move { request(&app, "POST", &format!("/repos/{repo}/query"), Some(body)).await }
    };
    let (status, body) = query(json!({"query": all_q})).await;
    assert_eq!(status, StatusCode::OK);
    let all: Vec<String> =
        body.as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
    assert_eq!(all.len(), 6);

    // Paginated: pages of 4, then 2, then done.
    let (status, page1) = query(json!({"query": all_q, "limit": 4})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page1["results"].as_array().unwrap().len(), 4);
    let cursor = page1["next_cursor"].as_str().expect("next_cursor expected").to_string();

    let (status, page2) = query(json!({"query": all_q, "limit": 4, "cursor": cursor})).await;
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
    assert_eq!(combined.len(), 6);
    let _ = &combined;

    // Invalid cursor → 400.
    let (status, _) = query(json!({"query": all_q, "limit": 4, "cursor": "zzz"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    std::fs::remove_dir_all(root).unwrap();
}

// ── One value type per field name + retype ─────────────────────────────────────

#[tokio::test]
async fn test_conflicting_value_type_rejected() {
    let (app, repo, root) = app_with_repo("typeconflict").await;
    create_metarecord(&app, &repo, json!([{"name": "rating", "value": {"type": "int", "value": 5}}]))
        .await;
    // A String write to the now-Int field is rejected.
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/metarecords"),
        Some(json!({"fields": [{"name": "rating", "value": {"type": "string", "value": "x"}}]})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_retype_endpoint_converts_and_relocks() {
    let (app, repo, root) = app_with_repo("retype").await;
    let m = create_metarecord(
        &app,
        &repo,
        json!([{"name": "rating", "value": {"type": "int", "value": 5}}]),
    )
    .await;
    let uuid = m["uuid"].as_str().unwrap().to_string();

    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/retype"),
        Some(json!({"name": "rating", "to": "string"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["converted"], 1);
    assert_eq!(body["fallback_count"], 0);

    // The value now reads back as a string.
    let (_, got) = request(&app, "GET", &format!("/repos/{repo}/metarecords/{uuid}"), None).await;
    let rating = got["fields"].as_array().unwrap().iter().find(|f| f["name"] == "rating").unwrap();
    assert_eq!(rating["value"]["type"], "string");
    assert_eq!(rating["value"]["value"], "5");

    // The field is String repo-wide now: an Int write is rejected.
    let (status, _) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/metarecords"),
        Some(json!({"fields": [{"name": "rating", "value": {"type": "int", "value": 1}}]})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_retype_endpoint_to_tree_ref() {
    let (app, repo, root) = app_with_repo("retypetree").await;
    // A string field holding a root path "/tags" becomes a TreeRef root node.
    let m = create_metarecord(
        &app,
        &repo,
        json!([{"name": "cat", "value": {"type": "string", "value": "/tags"}}]),
    )
    .await;
    let uuid = m["uuid"].as_str().unwrap().to_string();

    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/retype"),
        Some(json!({"name": "cat", "to": "tree_ref"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["converted"], 1);
    assert_eq!(body["fallback_count"], 0);

    let (_, got) = request(&app, "GET", &format!("/repos/{repo}/metarecords/{uuid}"), None).await;
    let cat = got["fields"].as_array().unwrap().iter().find(|f| f["name"] == "cat").unwrap();
    assert_eq!(cat["value"]["type"], "tree_ref");
    assert_eq!(cat["value"]["value"]["name"], "tags");
    assert_eq!(cat["value"]["value"]["parent"], serde_json::Value::Null);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_retype_rejects_reserved_and_unknown_type() {
    let (app, repo, root) = app_with_repo("retyperej").await;
    // Reserved field: rejected unconditionally.
    let (status, _) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/retype"),
        Some(json!({"name": "mfr_size", "to": "string"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // Unknown target type: rejected (the reference types are now valid targets,
    // so an unknown name must be one that maps to no value type at all).
    let (status, _) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/retype"),
        Some(json!({"name": "anything", "to": "bogus"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn unload_removes_repo_and_releases_the_lock() {
    let (app, repo, root) = app_with_repo("unload").await;

    // It is loaded and listed.
    let (_, before) = request(&app, "GET", "/repos", None).await;
    assert_eq!(before.as_array().unwrap().len(), 1);

    // Unload it: 200 with the repo uuid.
    let (status, body) = request(&app, "POST", &format!("/repos/{repo}/unload"), None).await;
    assert_eq!(status, StatusCode::OK, "unload failed: {body}");
    assert_eq!(body["repo_uuid"].as_str().unwrap(), repo);

    // No longer listed; repo-scoped calls now 404.
    let (_, after) = request(&app, "GET", "/repos", None).await;
    assert!(after.as_array().unwrap().is_empty(), "still listed: {after}");
    let (status, _) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": {"type": "is_present", "field": "x"}})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Unloading again is a 404 (it is gone).
    let (status, _) = request(&app, "POST", &format!("/repos/{repo}/unload"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The exclusive SQLite lock was released: the same root loads again.
    let (status, body) =
        request(&app, "POST", "/repos/load", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "reload failed: {body}");
    assert_eq!(body["repo_uuid"].as_str().unwrap(), repo);

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn unload_unknown_repo_is_404() {
    let app = app();
    let ghost = Uuid::new_v4().as_simple().to_string();
    let (status, _) = request(&app, "POST", &format!("/repos/{ghost}/unload"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ── New resource endpoints (target API) ───────────────────────────────────────

#[tokio::test]
async fn test_repo_info_and_rename() {
    let (app, repo, root) = app_with_repo("repoinfo").await;

    // GET one repo's info.
    let (status, info) = request(&app, "GET", &format!("/repos/{repo}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(info["repo_uuid"].as_str().unwrap(), repo);
    let original = info["name"].as_str().unwrap().to_string();

    // PATCH renames it; GET reflects the new name.
    let (status, renamed) = request(
        &app,
        "PATCH",
        &format!("/repos/{repo}"),
        Some(json!({"name": "fresh-name"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "rename failed: {renamed}");
    assert_eq!(renamed["name"], "fresh-name");
    assert_ne!(renamed["name"].as_str().unwrap(), original);

    // A second repo cannot take the same name (409).
    let other = temp_dir("repoinfo_other");
    let (_, ob) =
        request(&app, "POST", "/repos/init", Some(json!({"root": other.to_str().unwrap()}))).await;
    let other_repo = ob["repo_uuid"].as_str().unwrap().to_string();
    let (status, _) = request(
        &app,
        "PATCH",
        &format!("/repos/{other_repo}"),
        Some(json!({"name": "fresh-name"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Unknown repo → 404.
    let ghost = Uuid::new_v4().as_simple().to_string();
    let (status, _) = request(&app, "GET", &format!("/repos/{ghost}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_dir_all(other).unwrap();
}

#[tokio::test]
async fn test_record_field_by_name_get_set_unset() {
    let (app, repo, root) = app_with_repo("fieldbyname").await;
    let created = create_metarecord(
        &app,
        &repo,
        json!([{"name": "tag", "value": {"type": "string", "value": "jazz"}}]),
    )
    .await;
    let uuid = created["uuid"].as_str().unwrap();

    // GET the field's values.
    let (status, got) =
        request(&app, "GET", &format!("/repos/{repo}/metarecords/{uuid}/fields/tag"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got["values"].as_array().unwrap().len(), 1);
    assert_eq!(got["values"][0]["value"], "jazz");

    // PUT (set) with multiple values replaces all rows.
    let (status, _) = request(
        &app,
        "PUT",
        &format!("/repos/{repo}/metarecords/{uuid}/fields/tag"),
        Some(json!({"values": [{"type": "string", "value": "a"}, {"type": "string", "value": "b"}]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, got) =
        request(&app, "GET", &format!("/repos/{repo}/metarecords/{uuid}/fields/tag"), None).await;
    assert_eq!(got["values"].as_array().unwrap().len(), 2);

    // DELETE (unset) removes the field entirely.
    let (status, _) = request(
        &app,
        "DELETE",
        &format!("/repos/{repo}/metarecords/{uuid}/fields/tag"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, got) =
        request(&app, "GET", &format!("/repos/{repo}/metarecords/{uuid}/fields/tag"), None).await;
    assert_eq!(got["values"].as_array().unwrap().len(), 0, "field is now unknown");

    std::fs::remove_dir_all(root).unwrap();
}

// ── Field-name enumeration (GET /repos/:repo/fields) ──────────────────────────

/// Collects the response into a set of "name:type" strings for order-free
/// membership assertions.
fn field_pairs(body: &Value) -> std::collections::HashSet<String> {
    body.as_array()
        .unwrap()
        .iter()
        .map(|e| format!("{}:{}", e["name"].as_str().unwrap(), e["type"].as_str().unwrap()))
        .collect()
}

#[tokio::test]
async fn test_tree_roots_lists_forest_roots() {
    let (app, repo, root) = app_with_repo("treeroots").await;
    let treeref = |parent: Option<&str>, name: &str| {
        json!([{"name": "cat",
            "value": {"type": "tree_ref", "value": {"parent": parent, "name": name}}}])
    };
    let id = |m: Value| m["uuid"].as_str().unwrap().to_string();

    let music = id(create_metarecord(&app, &repo, treeref(None, "music")).await);
    let _books = id(create_metarecord(&app, &repo, treeref(None, "books")).await);
    // A child of `music` — must NOT be reported as a root.
    create_metarecord(&app, &repo, treeref(Some(&music), "rock")).await;

    let (status, body) =
        request(&app, "GET", &format!("/repos/{repo}/tree/roots?field=cat"), None).await;
    assert_eq!(status, StatusCode::OK, "got: {body}");
    let names: Vec<&str> = body.as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["books", "music"], "roots, sorted by name; no children");
    // The reported root carries its uuid.
    let music_entry = body.as_array().unwrap().iter().find(|e| e["name"] == "music").unwrap();
    assert_eq!(music_entry["uuid"].as_str().unwrap(), music);

    // mfr_path: the single init-time root metarecord, named "".
    let (status, body) =
        request(&app, "GET", &format!("/repos/{repo}/tree/roots?field=mfr_path"), None).await;
    assert_eq!(status, StatusCode::OK, "got: {body}");
    assert_eq!(body.as_array().unwrap().len(), 1, "one mfr_path root: {body}");
    assert_eq!(body[0]["name"], "");

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_list_fields_distinct_names_and_types() {
    let (app, repo, root) = app_with_repo("listfields").await;

    // A few metarecords with assorted field types. `tag` (ref) appears on two
    // metarecords — it must be reported once. `note` is set to Nothing — an
    // explicit absence, which must be excluded from the enumeration.
    let target = create_metarecord(&app, &repo, json!([])).await["uuid"]
        .as_str()
        .unwrap()
        .to_string();
    create_metarecord(
        &app,
        &repo,
        json!([
            {"name": "tag", "value": {"type": "ref", "value": target}},
            {"name": "rating", "value": {"type": "int", "value": 5}},
            {"name": "genre", "value": {"type": "string", "value": "jazz"}},
        ]),
    )
    .await;
    create_metarecord(
        &app,
        &repo,
        json!([
            {"name": "tag", "value": {"type": "ref", "value": target}},
            {"name": "category", "value": {"type": "tree_ref", "value": {"parent": null, "name": "music"}}},
            {"name": "note", "value": {"type": "nothing"}},
        ]),
    )
    .await;

    // Unfiltered: every distinct (name, type) of the repo's metarecords.
    let (status, body) = request(&app, "GET", &format!("/repos/{repo}/fields"), None).await;
    assert_eq!(status, StatusCode::OK, "got: {body}");
    let pairs = field_pairs(&body);
    for expected in [
        "tag:ref",
        "rating:int",
        "genre:string",
        "category:tree_ref",
        // The init-time root metarecord contributes these.
        "mfr_path:tree_ref",
        "mfr_type:string",
        "mf_watch:bool",
        "mf_ignore:string",
    ] {
        assert!(pairs.contains(expected), "missing {expected} in {pairs:?}");
    }
    // `tag` reported exactly once despite two rows; Nothing rows excluded.
    assert_eq!(body.as_array().unwrap().iter().filter(|e| e["name"] == "tag").count(), 1);
    assert!(!pairs.iter().any(|p| p.starts_with("note:")), "Nothing field must be excluded");

    // Filtered by type: only TreeRef field names.
    let (status, body) =
        request(&app, "GET", &format!("/repos/{repo}/fields?type=tree_ref"), None).await;
    assert_eq!(status, StatusCode::OK, "got: {body}");
    let pairs = field_pairs(&body);
    assert!(pairs.contains("category:tree_ref"));
    assert!(pairs.contains("mfr_path:tree_ref"));
    assert!(body.as_array().unwrap().iter().all(|e| e["type"] == "tree_ref"), "got: {body}");

    // Filtered by type: only Ref field names.
    let (status, body) =
        request(&app, "GET", &format!("/repos/{repo}/fields?type=ref"), None).await;
    assert_eq!(status, StatusCode::OK, "got: {body}");
    assert_eq!(field_pairs(&body), std::collections::HashSet::from(["tag:ref".to_string()]));

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_direct_resolve_tree_one_record() {
    let (app, repo, root) = app_with_repo("resolveone").await;
    let treeref = |parent: Option<&str>, name: &str| {
        json!([{"name": "cat",
            "value": {"type": "tree_ref", "value": {"parent": parent, "name": name}}}])
    };
    let id = |m: Value| m["uuid"].as_str().unwrap().to_string();
    let all = id(create_metarecord(&app, &repo, treeref(None, "all")).await);
    let music = id(create_metarecord(&app, &repo, treeref(Some(&all), "music")).await);

    let (status, body) = request(
        &app,
        "GET",
        &format!("/repos/{repo}/metarecords/{music}/fields/cat/resolve-tree"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "got: {body}");
    assert_eq!(body["paths"], json!(["all/music"]));

    std::fs::remove_dir_all(root).unwrap();
}
