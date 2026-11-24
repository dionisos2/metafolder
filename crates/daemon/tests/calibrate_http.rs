//! `POST /repos/:repo/calibrate-threshold` (doc "Calibrating the mass-write
//! thresholds"): a task measuring each kind of write on a throwaway store
//! beside the repository's, its thresholds written into `config.json` when
//! asked. Nothing here looks at a duration: what the thresholds are depends on
//! the machine, only their shape is held.

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

async fn setup(prefix: &str) -> (Router, String, TempDir) {
    let app = routes::build(std::sync::Arc::new(AppState::new(common::settings())));
    let root = TempDir::new(&format!("calhttp_{prefix}"));
    common::seed_config(&root);
    let (status, body) =
        request(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    let repo = body["repo_uuid"].as_str().unwrap().to_string();
    (app, repo, root)
}

async fn await_task(app: &Router, repo: &str, task_id: &str) -> Value {
    for _ in 0..6000 {
        let (status, task) =
            request(app, "GET", &format!("/repos/{repo}/tasks/{task_id}"), None).await;
        assert_eq!(status, StatusCode::OK, "task read failed: {task}");
        match task["status"].as_str() {
            Some("done") | Some("failed") | Some("cancelled") => return task,
            _ => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
        }
    }
    panic!("the calibration never reached a terminal state");
}

async fn calibrate(app: &Router, repo: &str, body: Value) -> Value {
    let (status, started) =
        request(app, "POST", &format!("/repos/{repo}/calibrate-threshold"), Some(body)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{started}");
    let task = await_task(app, repo, started["task_id"].as_str().unwrap()).await;
    assert_eq!(task["status"], "done", "{task}");
    task["result"].clone()
}

const KINDS: [&str; 4] = ["add", "remove", "set", "delete"];

#[tokio::test]
async fn a_calibration_measures_each_kind_and_leaves_nothing_behind() {
    let (app, repo, root) = setup("measure").await;
    let result = calibrate(&app, &repo, json!({"records": 200})).await;

    assert_eq!(result["records"], 200);
    assert_eq!(result["written"], false);
    let kinds = result["kinds"].as_array().unwrap();
    let names: Vec<&str> = kinds.iter().map(|k| k["kind"].as_str().unwrap()).collect();
    assert_eq!(names, KINDS);
    for k in kinds {
        let samples = k["samples"].as_array().unwrap();
        assert!(!samples.is_empty(), "{k}");
        assert_eq!(samples[0]["records"], 1, "the sweep starts at one record");
        assert!(
            samples[0]["each_ms"].as_f64().is_some() && samples[0]["mass_ms"].as_f64().is_some()
        );
        assert_eq!(result["thresholds"][k["kind"].as_str().unwrap()], k["threshold"]);
    }
    // The throwaway store is gone; the repository's configuration untouched.
    let internal = root.join(".metafolder/internal");
    let left: Vec<_> = std::fs::read_dir(&internal)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("calibrate"))
        .collect();
    assert!(left.is_empty(), "the throwaway store is removed");
    let config: Value = serde_json::from_str(
        &std::fs::read_to_string(root.join(".metafolder/config.json")).unwrap(),
    )
    .unwrap();
    assert!(config.get("mass_write_threshold").is_none(), "{config}");
}

#[tokio::test]
async fn a_calibration_asked_to_write_sets_the_repository_s_thresholds() {
    let (app, repo, root) = setup("write").await;
    let result = calibrate(&app, &repo, json!({"records": 200, "write": true})).await;
    assert_eq!(result["written"], true);

    let config: Value = serde_json::from_str(
        &std::fs::read_to_string(root.join(".metafolder/config.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(config["mass_write_threshold"], result["thresholds"], "{config}");
    // In force at once, without a reload.
    let (status, info) = request(&app, "GET", &format!("/repos/{repo}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(info["mass_write_thresholds"], result["thresholds"], "{info}");

    // A rename afterwards keeps them: config.json is read back before it is
    // written.
    let (status, body) =
        request(&app, "PATCH", &format!("/repos/{repo}"), Some(json!({"name": "renamed"}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let config: Value = serde_json::from_str(
        &std::fs::read_to_string(root.join(".metafolder/config.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(config["mass_write_threshold"], result["thresholds"], "{config}");
    assert_eq!(config["name"], "renamed");
}

#[tokio::test]
async fn a_store_too_small_is_refused() {
    let (app, repo, _root) = setup("small").await;
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/calibrate-threshold"),
        Some(json!({"records": 10})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}
