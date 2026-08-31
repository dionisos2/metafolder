//! HTTP-level tests for the ignore/eligibility introspection endpoints
//! (spec-file-tracking "Eligibility explain" / "Effective ignore set").

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

/// A repository whose root is watched and carries one ignore pattern, with a
/// `work` directory tracked and given its own (replacing) pattern set.
async fn setup() -> (Router, String) {
    let app = routes::build(std::sync::Arc::new(AppState::new()));
    let root = TempDir::new("ignhttp");
    std::fs::create_dir_all(root.join("work")).unwrap();
    let (status, body) =
        request(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    let repo = body["repo_uuid"].as_str().unwrap().to_string();

    // Root: watched, with a `.git` ignore pattern.
    let root_uuid = tree_root(&app, &repo).await;
    put_field(&app, &repo, &root_uuid, "mf_watch", json!([{"type": "bool", "value": true}])).await;
    put_field(
        &app,
        &repo,
        &root_uuid,
        "mf_ignore",
        json!([{"type": "string", "value": r"\.git(/.*)?$"}]),
    )
    .await;

    // /work: tracked, with its own pattern set (which replaces the root's).
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/track"),
        Some(json!({"path": root.join("work").to_str().unwrap()})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "track failed: {body}");
    let work = body["uuid"].as_str().unwrap().to_string();
    // `track` writes `mf_watch = false`; drop it so /work inherits the root's
    // watch and only contributes its own ignore set (the common shape).
    let (status, body) =
        request(&app, "DELETE", &format!("/repos/{repo}/metarecords/{work}/fields/mf_watch"), None)
            .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "unset mf_watch failed: {body}");
    put_field(
        &app,
        &repo,
        &work,
        "mf_ignore",
        json!([{"type": "string", "value": r"target(/.*)?$"}]),
    )
    .await;
    (app, repo)
}

async fn tree_root(app: &Router, repo: &str) -> String {
    let (status, body) =
        request(app, "GET", &format!("/repos/{repo}/tree/roots?field=mfr_path"), None).await;
    assert_eq!(status, StatusCode::OK, "tree/roots failed: {body}");
    body.as_array().unwrap().iter().find(|r| r["name"] == "").unwrap()["uuid"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn put_field(app: &Router, repo: &str, uuid: &str, name: &str, values: Value) {
    let (status, body) = request(
        app,
        "PUT",
        &format!("/repos/{repo}/metarecords/{uuid}/fields/{name}"),
        Some(json!({ "values": values })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "PUT {name} failed: {body}");
}

#[tokio::test]
async fn test_eligibility_explains_each_path() {
    let (app, repo) = setup().await;
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/eligibility"),
        Some(
            json!({"paths": ["/notes.txt", "/.git/config", "/work/target/debug", "/work/.git/x"]}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "eligibility failed: {body}");
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 4, "one result per requested path, in order");

    assert_eq!(results[0]["path"], "/notes.txt");
    assert_eq!(results[0]["eligible"], true);
    assert_eq!(results[0]["reason"], "tracked");
    assert_eq!(results[0]["watch_scope"], "");
    assert_eq!(results[0]["pattern"], Value::Null);

    assert_eq!(results[1]["eligible"], false);
    assert_eq!(results[1]["reason"], "ignored");
    assert_eq!(results[1]["pattern"], r"\.git(/.*)?$");
    assert_eq!(results[1]["ignore_source"], "");

    assert_eq!(results[2]["eligible"], false);
    assert_eq!(results[2]["reason"], "ignored");
    assert_eq!(results[2]["ignore_source"], "/work", "the nearest set wins, not the root's");
    assert_eq!(results[2]["pattern"], r"target(/.*)?$");

    assert_eq!(results[3]["eligible"], true, "the root's pattern is not merged in below /work");
}

#[tokio::test]
async fn test_eligibility_rejects_an_oversized_batch() {
    let (app, repo) = setup().await;
    let paths: Vec<String> = (0..1001).map(|i| format!("/f{i}")).collect();
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/eligibility"),
        Some(json!({ "paths": paths })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn test_eligibility_rejects_a_path_without_leading_slash() {
    let (app, repo) = setup().await;
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/eligibility"),
        Some(json!({"paths": ["work/target"]})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn test_effective_ignore_reports_source_and_directness() {
    let (app, repo) = setup().await;

    // A directory with its own set.
    let (status, body) =
        request(&app, "GET", &format!("/repos/{repo}/ignore/effective?path=/work"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["source"], "/work");
    assert_eq!(body["direct"], true);
    assert_eq!(body["patterns"], json!([r"target(/.*)?$"]));
    assert!(body["source_uuid"].as_str().is_some());

    // A directory inheriting one: the write trap the GUI warns about.
    let (status, body) =
        request(&app, "GET", &format!("/repos/{repo}/ignore/effective?path=/work/live"), None)
            .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["source"], "/work");
    assert_eq!(body["direct"], false);
    assert_eq!(body["patterns"], json!([r"target(/.*)?$"]));

    // The repository root itself (empty path).
    let (status, body) =
        request(&app, "GET", &format!("/repos/{repo}/ignore/effective?path="), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["source"], "");
    assert_eq!(body["direct"], true);
    assert_eq!(body["patterns"], json!([r"\.git(/.*)?$"]));
}
