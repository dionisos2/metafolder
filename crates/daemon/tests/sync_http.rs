//! Integration tests for the cross-repo sync HTTP primitives (spec-sync "HTTP
//! API"): two repositories loaded on one daemon, driven through the Axum router
//! with `oneshot`. Covers canonical pair ordering, link CRUD, the `status`
//! truth table, batched commit + snapshot round-trip, and endpoint-coupled
//! deletion. (Matching is now CLI-side by TreeRef identity — see the CLI
//! `sync plan` tests — so there is no daemon matcher to exercise here.)

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use metafolder_daemon::routes;
use metafolder_daemon::state::AppState;
use serde_json::{json, Value};
use tower::util::ServiceExt;
use uuid::Uuid;

mod common;
use common::TempDir;

fn temp_dir(prefix: &str) -> TempDir {
    TempDir::new(&format!("sync_{prefix}"))
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

/// Initialises a repository in a temp dir; returns (repo uuid, root path).
async fn init_repo(app: &Router, prefix: &str) -> (String, TempDir) {
    let root = temp_dir(prefix);
    let (status, body) =
        request(app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    (body["repo_uuid"].as_str().unwrap().to_string(), root)
}

/// Two repos on one app, returned in **canonical** order (`a < b`).
async fn two_repos(app: &Router) -> (String, TempDir, String, TempDir) {
    let (x, xr) = init_repo(app, "x").await;
    let (y, yr) = init_repo(app, "y").await;
    if x < y {
        (x, xr, y, yr)
    } else {
        (y, yr, x, xr)
    }
}

async fn create(app: &Router, repo: &str, fields: Value) -> Value {
    let (status, body) = request(
        app,
        "POST",
        &format!("/repos/{repo}/metarecords"),
        Some(json!({"fields": fields, "force": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create failed: {body}");
    body
}

// ── Pair ordering and link CRUD ──────────────────────────────────────────────

#[tokio::test]
async fn test_self_pair_is_rejected() {
    let app = app();
    let (a, _ar) = init_repo(&app, "self").await;
    let (status, _) = request(&app, "GET", &format!("/sync/{a}/{a}/links"), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_create_link_canonical_roles_and_list() {
    let app = app();
    let (a, _ar, b, _br) = two_repos(&app).await;
    let ra = create(&app, &a, json!([])).await["uuid"].as_str().unwrap().to_string();
    let rb = create(&app, &b, json!([])).await["uuid"].as_str().unwrap().to_string();

    // Address the pair in *non-canonical* URL order; roles must still resolve
    // by canonical repo order.
    let (status, link) = request(
        &app,
        "POST",
        &format!("/sync/{b}/{a}/links"),
        Some(json!({"record_a": ra, "record_b": rb})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create link failed: {link}");
    assert_eq!(link["record_a"], ra);
    assert_eq!(link["record_b"], rb);
    assert!(link["version_a"].is_null(), "fresh link has no version");

    let (status, body) = request(&app, "GET", &format!("/sync/{a}/{b}/links"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["repo_a"], a);
    assert_eq!(body["repo_b"], b);
    assert_eq!(body["links"].as_array().unwrap().len(), 1);
    assert_eq!(body["links"][0]["uuid"], link["uuid"]);
}

#[tokio::test]
async fn test_duplicate_endpoint_is_conflict() {
    let app = app();
    let (a, _ar, b, _br) = two_repos(&app).await;
    let ra = create(&app, &a, json!([])).await["uuid"].as_str().unwrap().to_string();
    let rb = create(&app, &b, json!([])).await["uuid"].as_str().unwrap().to_string();
    let rb2 = create(&app, &b, json!([])).await["uuid"].as_str().unwrap().to_string();

    let (status, _) = request(
        &app,
        "POST",
        &format!("/sync/{a}/{b}/links"),
        Some(json!({"record_a": ra, "record_b": rb})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Re-using record_a in a second link must hit the UNIQUE constraint.
    let (status, _) = request(
        &app,
        "POST",
        &format!("/sync/{a}/{b}/links"),
        Some(json!({"record_a": ra, "record_b": rb2})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn test_create_link_missing_record_is_not_found() {
    let app = app();
    let (a, _ar, b, _br) = two_repos(&app).await;
    let ra = create(&app, &a, json!([])).await["uuid"].as_str().unwrap().to_string();
    let ghost = Uuid::new_v4().as_simple().to_string();
    let (status, _) = request(
        &app,
        "POST",
        &format!("/sync/{a}/{b}/links"),
        Some(json!({"record_a": ra, "record_b": ghost})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ── status truth table + commit ──────────────────────────────────────────────

async fn make_link(app: &Router, a: &str, b: &str, ra: &str, rb: &str) -> String {
    let (status, link) = request(
        app,
        "POST",
        &format!("/sync/{a}/{b}/links"),
        Some(json!({"record_a": ra, "record_b": rb})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create link failed: {link}");
    link["uuid"].as_str().unwrap().to_string()
}

async fn status_of(app: &Router, a: &str, b: &str, link: &str) -> Value {
    let (status, body) = request(app, "GET", &format!("/sync/{a}/{b}/status"), None).await;
    assert_eq!(status, StatusCode::OK, "status failed: {body}");
    body["links"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["uuid"] == link)
        .cloned()
        .unwrap_or(Value::Null)
}

#[tokio::test]
async fn test_status_never_synced_then_in_sync_then_ahead() {
    let app = app();
    let (a, _ar, b, _br) = two_repos(&app).await;
    let ra_m = create(&app, &a, json!([])).await;
    let rb_m = create(&app, &b, json!([])).await;
    let ra = ra_m["uuid"].as_str().unwrap().to_string();
    let rb = rb_m["uuid"].as_str().unwrap().to_string();
    let va = ra_m["version"].as_u64().unwrap();
    let vb = rb_m["version"].as_u64().unwrap();

    let link = make_link(&app, &a, &b, &ra, &rb).await;
    assert_eq!(status_of(&app, &a, &b, &link).await["state"], "never_synced");

    // Commit at the two current versions, with a snapshot field.
    let (status, body) = request(
        &app,
        "POST",
        &format!("/sync/{a}/{b}/links/commit"),
        Some(json!({"commits": [{
            "link": link,
            "version_a": va,
            "version_b": vb,
            "snapshot": [{"name": "tag", "value": {"type": "string", "value": "hi"}}],
        }]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "commit failed: {body}");
    assert_eq!(body["committed"], 1);
    assert_eq!(status_of(&app, &a, &b, &link).await["state"], "in_sync");

    // A write on side A bumps its version → ahead_a.
    let (status, _) = request(
        &app,
        "PUT",
        &format!("/repos/{a}/metarecords/{ra}/fields/tag"),
        Some(json!({"value": {"type": "string", "value": "changed"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(status_of(&app, &a, &b, &link).await["state"], "ahead_a");

    // The stored snapshot survives and is readable on the link detail.
    let (status, detail) = request(&app, "GET", &format!("/sync/{a}/{b}/links/{link}"), None).await;
    assert_eq!(status, StatusCode::OK);
    let snap = detail["snapshot"].as_array().unwrap();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0]["name"], "tag");
    assert_eq!(snap[0]["value"]["value"], "hi");
    // Endpoint metarecords are inlined.
    assert_eq!(detail["record_a"]["uuid"], ra);
    assert_eq!(detail["record_b"]["uuid"], rb);
}

#[tokio::test]
async fn test_status_missing_endpoint_takes_precedence() {
    let app = app();
    let (a, _ar, b, _br) = two_repos(&app).await;
    let ra = create(&app, &a, json!([])).await["uuid"].as_str().unwrap().to_string();
    let rb = create(&app, &b, json!([])).await["uuid"].as_str().unwrap().to_string();
    let link = make_link(&app, &a, &b, &ra, &rb).await;

    // Delete side B's endpoint out from under the link.
    let (status, _) = request(&app, "DELETE", &format!("/repos/{b}/metarecords/{rb}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(status_of(&app, &a, &b, &link).await["state"], "missing_b");
}

// ── delete with coupled endpoint ─────────────────────────────────────────────

#[tokio::test]
async fn test_delete_link_with_endpoint_deletes_record_first() {
    let app = app();
    let (a, _ar, b, _br) = two_repos(&app).await;
    let ra = create(&app, &a, json!([])).await["uuid"].as_str().unwrap().to_string();
    let rb = create(&app, &b, json!([])).await["uuid"].as_str().unwrap().to_string();
    let link = make_link(&app, &a, &b, &ra, &rb).await;

    let (status, _) =
        request(&app, "DELETE", &format!("/sync/{a}/{b}/links/{link}?with_endpoint=b"), None).await;
    assert_eq!(status, StatusCode::OK);

    // The link is gone …
    let (status, _) = request(&app, "GET", &format!("/sync/{a}/{b}/links/{link}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // … and so is record B.
    let (status, _) = request(&app, "GET", &format!("/repos/{b}/metarecords/{rb}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Record A is untouched.
    let (status, _) = request(&app, "GET", &format!("/repos/{a}/metarecords/{ra}"), None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn test_mf_sync_endpoint() {
    let app = app();
    let (a, _ar) = init_repo(&app, "mfsync").await;
    let root = root_uuid_of(&app, &a).await;

    // A directory marked mf_sync = external, and a file under it.
    let dir = create(
        &app,
        &a,
        json!([
            {"name": "mfr_path", "value": {"type": "tree_ref", "value": {"parent": root, "name": "ext"}}},
            {"name": "mf_sync", "value": {"type": "string", "value": "external"}}
        ]),
    )
    .await["uuid"]
        .as_str()
        .unwrap()
        .to_string();
    let file = create(
        &app,
        &a,
        json!([{"name": "mfr_path", "value": {"type": "tree_ref", "value": {"parent": dir, "name": "f.txt"}}}]),
    )
    .await["uuid"]
        .as_str()
        .unwrap()
        .to_string();

    // The file inherits external; a bare record (no mfr_path) is internal.
    let (status, body) =
        request(&app, "GET", &format!("/repos/{a}/metarecords/{file}/mf-sync"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["mf_sync"], "external");

    let bare =
        create(&app, &a, json!([{"name": "tag", "value": {"type": "string", "value": "x"}}])).await
            ["uuid"]
            .as_str()
            .unwrap()
            .to_string();
    let (_, body) =
        request(&app, "GET", &format!("/repos/{a}/metarecords/{bare}/mf-sync"), None).await;
    assert_eq!(body["mf_sync"], "internal");
}

/// The mfr_path forest root uuid of a repo.
async fn root_uuid_of(app: &Router, repo: &str) -> String {
    let (_, body) =
        request(app, "GET", &format!("/repos/{repo}/tree/roots?field=mfr_path"), None).await;
    body[0]["uuid"].as_str().unwrap().to_string()
}
