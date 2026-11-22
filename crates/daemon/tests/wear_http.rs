//! `GET /repos/:repo/wear`: how worn the store is (doc "Storage wear").

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

/// The shipped settings, every write to many records in bulk: what these
/// tests look at is what a mass write leaves pending, on a handful of records
/// (doc "Mass writes in the store").
fn mass_settings() -> metafolder_daemon::daemon_config::DaemonSettings {
    metafolder_daemon::daemon_config::DaemonSettings {
        mass_write_threshold: 0,
        ..common::settings()
    }
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

#[tokio::test]
async fn the_wear_of_a_new_repository_is_nothing_pending() {
    let app = routes::build(std::sync::Arc::new(AppState::new(common::settings())));
    let root = TempDir::new("wear_http");
    common::seed_config(&root);
    let (status, body) =
        request(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    let repo = body["repo_uuid"].as_str().unwrap().to_string();

    let (status, wear) = request(&app, "GET", &format!("/repos/{repo}/wear"), None).await;
    assert_eq!(status, StatusCode::OK, "{wear}");
    assert_eq!(wear["pending_writes"], json!(0));
    assert_eq!(wear["index_writes"], json!(0));
    assert_eq!(wear["pending_bytes"], json!(0));
    assert!(wear["dead_ids"].is_u64() && wear["dead_fraction"].is_f64(), "{wear}");

    let unknown = "0".repeat(32);
    let (status, _) = request(&app, "GET", &format!("/repos/{unknown}/wear"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// A trashing of many records is one deletion of many records (doc "Mass
/// operations in the log"): which of them have a duplicate group is asked once,
/// and only those leave it one by one.
#[tokio::test]
async fn a_trashing_is_a_deletion_of_many_records() {
    let app = routes::build(std::sync::Arc::new(AppState::new(mass_settings())));
    let root = TempDir::new("wear_http_trash");
    common::seed_config(&root);
    let (status, body) =
        request(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    let repo = body["repo_uuid"].as_str().unwrap().to_string();

    let mut uuids = Vec::new();
    for _ in 0..4 {
        let (status, record) = request(
            &app,
            "POST",
            &format!("/repos/{repo}/metarecords"),
            Some(
                json!({"fields": [{"name": "rating", "value": {"type": "string", "value": "x"}}]}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{record}");
        uuids.push(record["uuid"].as_str().unwrap().to_string());
    }
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/metarecords/trash"),
        Some(json!({"uuids": uuids, "force": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["deleted"], json!(4));

    let (_, wear) = request(&app, "GET", &format!("/repos/{repo}/wear"), None).await;
    assert_eq!(wear["pending_writes"], json!(1), "one deletion waiting for its fold: {wear}");

    let (status, _) =
        request(&app, "GET", &format!("/repos/{repo}/metarecords/{}", uuids[0]), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // A uuid that is no metarecord is still refused.
    let unknown = "0".repeat(32);
    let (status, _) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/metarecords/trash"),
        Some(json!({"uuids": [unknown], "force": false})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// `mf repo check` folds what is pending first, so that it holds everything to
/// the rows, the histograms the pending deletions leave counted included.
#[tokio::test]
async fn a_check_folds_what_is_pending_and_holds_the_histograms() {
    let app = routes::build(std::sync::Arc::new(AppState::new(mass_settings())));
    let root = TempDir::new("wear_http_check");
    common::seed_config(&root);
    let (status, body) =
        request(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    let repo = body["repo_uuid"].as_str().unwrap().to_string();

    let mut uuids = Vec::new();
    for i in 0..6 {
        let (status, record) = request(
            &app,
            "POST",
            &format!("/repos/{repo}/metarecords"),
            Some(json!({"fields": [{"name": "rank", "value": {"type": "int", "value": i}}]})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{record}");
        uuids.push(record["uuid"].as_str().unwrap().to_string());
    }
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/metarecords/trash"),
        Some(json!({"uuids": &uuids[..4], "force": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, wear) = request(&app, "GET", &format!("/repos/{repo}/wear"), None).await;
    assert_eq!(wear["pending_writes"], json!(1), "{wear}");

    let (status, checked) = request(&app, "POST", &format!("/repos/{repo}/check"), None).await;
    assert_eq!(status, StatusCode::OK, "{checked}");
    assert_eq!(checked["problems"], json!([]), "{checked}");
    let (_, wear) = request(&app, "GET", &format!("/repos/{repo}/wear"), None).await;
    assert_eq!(wear["pending_writes"], json!(0), "the check folded it: {wear}");
}

/// `POST /repos/:repo/fold` folds every list pending now, one list at a time.
#[tokio::test]
async fn an_explicit_fold_writes_the_pending_lists_in() {
    let app = routes::build(std::sync::Arc::new(AppState::new(mass_settings())));
    let root = TempDir::new("wear_http_fold");
    common::seed_config(&root);
    let (status, body) =
        request(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    let repo = body["repo_uuid"].as_str().unwrap().to_string();

    let mut uuids = Vec::new();
    for i in 0..5 {
        let (status, record) = request(
            &app,
            "POST",
            &format!("/repos/{repo}/metarecords"),
            Some(json!({"fields": [{"name": "rank", "value": {"type": "int", "value": i}}]})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{record}");
        uuids.push(record["uuid"].as_str().unwrap().to_string());
    }
    let (status, _) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/metarecords/trash"),
        Some(json!({"uuids": &uuids[..3], "force": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, wear) = request(&app, "GET", &format!("/repos/{repo}/wear"), None).await;
    assert_eq!(wear["pending_writes"], json!(1), "{wear}");

    let (status, folded) = request(&app, "POST", &format!("/repos/{repo}/fold"), None).await;
    assert_eq!(status, StatusCode::OK, "{folded}");
    // The list, and the history of the records the writes logged (doc "Log
    // storage").
    assert_eq!(folded["folded"], json!(2), "one list and the log: {folded}");
    let (_, wear) = request(&app, "GET", &format!("/repos/{repo}/wear"), None).await;
    assert_eq!(wear["pending_writes"], json!(0), "{wear}");
    // Nothing left: nothing folded.
    let (_, folded) = request(&app, "POST", &format!("/repos/{repo}/fold"), None).await;
    assert_eq!(folded["folded"], json!(0), "{folded}");
}
