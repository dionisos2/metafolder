//! Store maintenance over HTTP (doc "Storage"): `check`, `reindex`, backups by
//! request and when due, and `restore`.

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

fn temp_dir(prefix: &str) -> TempDir {
    TempDir::new(&format!("kvserve_{prefix}"))
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

async fn setup(prefix: &str) -> (Router, String, TempDir, std::sync::Arc<AppState>) {
    let state = std::sync::Arc::new(AppState::new());
    let app = routes::build(state.clone());
    let root = temp_dir(prefix);
    let (status, body) =
        request(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    let repo = body["repo_uuid"].as_str().unwrap().to_string();
    (app, repo, root, state)
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

/// `POST /repos/:repo/check` reports what no longer holds together (nothing,
/// here) and `POST /repos/:repo/reindex` derives again.
#[tokio::test]
async fn check_and_reindex() {
    let (app, repo, _root, _state) = setup("check").await;
    let tag = json!([{"name": "tag", "value": {"type": "string", "value": "kept"}}]);
    let kept = create(&app, &repo, tag).await;

    let (status, body) = request(&app, "POST", &format!("/repos/{repo}/check"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"problems": []}));
    let (status, body) = request(&app, "POST", &format!("/repos/{repo}/reindex"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let query = json!({"query": {"type": "eq", "field": "tag",
                                 "value": {"type": "string", "value": "kept"}}});
    let (status, body) = request(&app, "POST", &format!("/repos/{repo}/query"), Some(query)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!([kept]), "answers after a reindex");
}

/// `POST /repos/:repo/backup` takes a verified backup — by default under
/// `internal/backups/`, or into a new directory — and the automatic backups
/// of every loaded repository are taken when due.
#[tokio::test]
async fn backups_by_request_and_when_due() {
    let (app, repo, root, state) = setup("backup").await;
    let (status, body) = request(&app, "POST", &format!("/repos/{repo}/backup"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let path = std::path::PathBuf::from(body["path"].as_str().unwrap());
    assert!(path.starts_with(root.path().join(".metafolder/internal/backups")), "{body}");
    assert!(path.join("backup.json").exists());

    let elsewhere = root.path().join("elsewhere");
    let to = json!({"to": elsewhere.to_str().unwrap()});
    let (status, body) =
        request(&app, "POST", &format!("/repos/{repo}/backup"), Some(to.clone())).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(elsewhere.join("config.json").exists());
    let (status, _) = request(&app, "POST", &format!("/repos/{repo}/backup"), Some(to)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "an existing directory is not overwritten");

    state.run_auto_backups();
    assert!(root.path().join(".metafolder/internal/backups/auto/backup.json").exists());
}

/// Initialises a repository with a `kept` note, takes a backup of it, then
/// writes a `lost` note.
async fn backed_up(app: &Router, root: &TempDir) -> String {
    let (status, body) =
        request(app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let repo = body["repo_uuid"].as_str().unwrap().to_string();
    create(app, &repo, json!([{"name": "note", "value": {"type": "string", "value": "kept"}}]))
        .await;
    let (status, body) = request(app, "POST", &format!("/repos/{repo}/backup"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    create(app, &repo, json!([{"name": "note", "value": {"type": "string", "value": "lost"}}]))
        .await;
    repo
}

/// The notes a loaded repository answers, sorted.
async fn notes(app: &Router, repo: &str) -> Vec<String> {
    let query = json!({"query": {"type": "is_present", "field": "note"}, "select": ["note"]});
    let (status, body) = request(app, "POST", &format!("/repos/{repo}/query"), Some(query)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let mut out: Vec<String> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["fields"][0]["value"]["value"].as_str().unwrap().to_string())
        .collect();
    out.sort();
    out
}

/// `POST /repos/:repo/restore` restores a loaded repository from its most
/// recent backup — unloaded, restored, loaded back — and a client of the
/// change feed sees the head move back.
#[tokio::test]
async fn a_loaded_repository_is_restored_and_answers() {
    let state = std::sync::Arc::new(AppState::new());
    let app = routes::build(state.clone());
    let root = temp_dir("restore");
    let repo = backed_up(&app, &root).await;
    assert_eq!(notes(&app, &repo).await, ["kept", "lost"]);
    let (_, since) = request(&app, "GET", &format!("/repos/{repo}/log/since"), None).await;
    let head = since["head"].as_i64().unwrap();

    let (status, body) = request(&app, "POST", &format!("/repos/{repo}/restore"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["repo_uuid"], json!(repo));
    assert!(body["backup"]["created_at_ms"].as_i64().is_some(), "{body}");
    assert!(body["old_store"].as_str().unwrap().contains("pre-restore-"), "{body}");
    assert_eq!(notes(&app, &repo).await, ["kept"]);

    let (_, since) =
        request(&app, "GET", &format!("/repos/{repo}/log/since?op={head}"), None).await;
    assert_ne!(since["head"].as_i64(), Some(head), "{since}");
    assert_eq!(since["operations"], json!([]), "{since}");
}

/// `POST /repos/restore` restores a repository named by its path — here one
/// whose store is gone, so it could not be loaded — and loads it.
#[tokio::test]
async fn an_unloadable_repository_is_restored_by_its_path() {
    let state = std::sync::Arc::new(AppState::new());
    let app = routes::build(state.clone());
    let root = temp_dir("restore_path");
    let repo = backed_up(&app, &root).await;
    let (status, body) = request(&app, "POST", &format!("/repos/{repo}/unload"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    std::fs::remove_dir_all(root.join(".metafolder/internal/kv")).unwrap();
    let (status, _) =
        request(&app, "POST", "/repos/load", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_ne!(status, StatusCode::OK, "no store: no load");

    let (status, body) =
        request(&app, "POST", "/repos/restore", Some(json!({"root": root.to_str().unwrap()})))
            .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["repo_uuid"], json!(repo));
    assert_eq!(body["old_store"], Value::Null, "{body}");
    assert_eq!(notes(&app, &repo).await, ["kept"]);
}

/// The path form of a loaded repository takes the loaded route.
#[tokio::test]
async fn a_loaded_repository_is_restored_by_its_path_too() {
    let state = std::sync::Arc::new(AppState::new());
    let app = routes::build(state.clone());
    let root = temp_dir("restore_loaded_path");
    let repo = backed_up(&app, &root).await;
    let (status, body) =
        request(&app, "POST", "/repos/restore", Some(json!({"root": root.to_str().unwrap()})))
            .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(notes(&app, &repo).await, ["kept"]);
}

#[tokio::test]
async fn a_restore_names_its_errors() {
    let state = std::sync::Arc::new(AppState::new());
    let app = routes::build(state.clone());
    let root = temp_dir("restore_errors");
    let (status, body) =
        request(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let repo = body["repo_uuid"].as_str().unwrap().to_string();
    let (status, body) = request(&app, "POST", &format!("/repos/{repo}/restore"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "no backup yet: {body}");
    let not_a_backup = root.join("nothing");
    std::fs::create_dir_all(&not_a_backup).unwrap();
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/restore"),
        Some(json!({"from": not_a_backup.to_str().unwrap()})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(notes(&app, &repo).await, Vec::<String>::new(), "still loaded and answering");
}
