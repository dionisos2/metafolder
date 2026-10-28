//! Repositories the daemon knows by name without loading them (doc "Known
//! repositories"): `[[known]]` in the daemon configuration, `GET
//! /repos/known`, and `POST /repos/load {"name"}`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use metafolder_daemon::daemon_config::{self, LoadEntry};
use metafolder_daemon::repo::{self, RepoLocator};
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

/// A repository on disk, not loaded, named `name`.
fn on_disk_repo(dir: &Path, name: &str) -> PathBuf {
    let root = dir.join(name);
    std::fs::create_dir_all(&root).unwrap();
    common::seed_config(&root);
    let opened = repo::init_repository(&root, None, Some(name), false).unwrap();
    drop(opened); // release the store's lock
    root
}

fn app_with_config(dir: &Path, contents: &str) -> Router {
    let path = dir.join("config.toml");
    std::fs::write(&path, contents).unwrap();
    routes::build(Arc::new(AppState::new().with_config_file(path)))
}

#[test]
fn test_known_entries_parse_like_load_entries() {
    let dir = TempDir::new("known_parse");
    let path = dir.join("config.toml");
    std::fs::write(
        &path,
        r#"
[[known]]
root = "/data/music"

[[known]]
metafolder = "/ssd/meta/photos/.metafolder"
in-memory = true
"#,
    )
    .unwrap();
    let config = daemon_config::read_config(&path).unwrap();
    assert!(config.load.is_empty(), "a known repository is not loaded at startup");
    assert_eq!(
        config.known,
        vec![
            LoadEntry::from(RepoLocator::Root(PathBuf::from("/data/music"))),
            LoadEntry {
                locator: RepoLocator::Metafolder(PathBuf::from("/ssd/meta/photos/.metafolder")),
                in_memory: true,
            },
        ]
    );
}

#[test]
fn test_a_known_entry_with_both_locators_is_an_error() {
    let dir = TempDir::new("known_both");
    let path = dir.join("config.toml");
    std::fs::write(&path, "[[known]]\nroot = \"/a\"\nmetafolder = \"/b\"\n").unwrap();
    let err = format!("{:#}", daemon_config::read_config(&path).unwrap_err());
    assert!(err.contains("'known'"), "{err}");
}

#[tokio::test]
async fn test_a_known_repository_is_listed_and_loaded_by_its_name() {
    let dir = TempDir::new("known_load");
    let root = on_disk_repo(&dir, "music");
    let app = app_with_config(&dir, &format!("[[known]]\nroot = {:?}\n", root));

    let (status, body) = request(&app, "GET", "/repos/known", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let known = body.as_array().unwrap();
    assert_eq!(known.len(), 1, "{body}");
    assert_eq!(known[0]["name"], "music", "{body}");
    assert_eq!(known[0]["loaded"], false, "{body}");
    assert_eq!(known[0]["startup"], false, "{body}");
    assert_eq!(known[0]["root"], root.canonicalize().unwrap().to_str().unwrap(), "{body}");
    // Known is not loaded: the ordinary listing does not have it.
    let (_, repos) = request(&app, "GET", "/repos", None).await;
    assert_eq!(repos, json!([]));

    let (status, body) = request(&app, "POST", "/repos/load", Some(json!({"name": "music"}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let uuid = body["repo_uuid"].as_str().unwrap().to_string();
    assert_eq!(known[0]["repo_uuid"], uuid.as_str());

    let (_, body) = request(&app, "GET", "/repos/known", None).await;
    assert_eq!(body[0]["loaded"], true, "{body}");
}

#[tokio::test]
async fn test_the_repositories_loaded_at_startup_are_known_too() {
    // Unloaded, a [[load]] repository is loaded again by its name.
    let dir = TempDir::new("known_startup");
    let root = on_disk_repo(&dir, "photos");
    let app = app_with_config(&dir, &format!("[[load]]\nroot = {:?}\n", root));
    let (_, body) = request(&app, "GET", "/repos/known", None).await;
    assert_eq!(body[0]["name"], "photos", "{body}");
    assert_eq!(body[0]["startup"], true, "{body}");
    let (status, body) =
        request(&app, "POST", "/repos/load", Some(json!({"name": "photos"}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn test_the_configuration_is_read_again_at_each_lookup() {
    // An entry added while the daemon runs is known at once: no restart.
    let dir = TempDir::new("known_reread");
    let root = on_disk_repo(&dir, "books");
    let app = app_with_config(&dir, "");
    let (status, body) = request(&app, "POST", "/repos/load", Some(json!({"name": "books"}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(body["error"].as_str().unwrap().contains("books"), "{body}");

    std::fs::write(dir.join("config.toml"), format!("[[known]]\nroot = {:?}\n", root)).unwrap();
    let (status, body) = request(&app, "POST", "/repos/load", Some(json!({"name": "books"}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn test_a_known_entry_that_cannot_be_read_is_listed_with_its_error() {
    // An unplugged drive: listed, with why, and the others still answer.
    let dir = TempDir::new("known_missing");
    let root = on_disk_repo(&dir, "music");
    let missing = dir.join("absent");
    let app = app_with_config(
        &dir,
        &format!("[[known]]\nroot = {:?}\n\n[[known]]\nroot = {:?}\n", missing, root),
    );
    let (status, body) = request(&app, "GET", "/repos/known", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let known = body.as_array().unwrap();
    assert_eq!(known.len(), 2, "{body}");
    assert!(known[0]["name"].is_null(), "{body}");
    assert_eq!(known[0]["root"], missing.to_str().unwrap(), "{body}");
    assert!(known[0]["error"].is_string(), "{body}");
    assert_eq!(known[1]["name"], "music", "{body}");
    assert!(known[1]["error"].is_null(), "{body}");
    let (status, body) = request(&app, "POST", "/repos/load", Some(json!({"name": "music"}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn test_a_known_entry_listed_twice_is_one_repository() {
    // In [[load]] and in [[known]], or once by root and once by metafolder.
    let dir = TempDir::new("known_dup");
    let root = on_disk_repo(&dir, "music");
    let app = app_with_config(
        &dir,
        &format!(
            "[[load]]\nroot = {:?}\n\n[[known]]\nmetafolder = {:?}\n",
            root,
            root.join(".metafolder")
        ),
    );
    let (_, body) = request(&app, "GET", "/repos/known", None).await;
    assert_eq!(body.as_array().unwrap().len(), 1, "{body}");
    assert_eq!(body[0]["startup"], true, "{body}");
}

#[tokio::test]
async fn test_two_known_repositories_with_one_name_are_refused() {
    let dir = TempDir::new("known_ambiguous");
    let a = on_disk_repo(&dir.join("a"), "music");
    let b = on_disk_repo(&dir.join("b"), "music");
    let app =
        app_with_config(&dir, &format!("[[known]]\nroot = {:?}\n\n[[known]]\nroot = {:?}\n", a, b));
    let (status, body) = request(&app, "POST", "/repos/load", Some(json!({"name": "music"}))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let error = body["error"].as_str().unwrap();
    assert!(error.contains(a.to_str().unwrap()) && error.contains(b.to_str().unwrap()), "{error}");
}

#[tokio::test]
async fn test_a_known_entry_in_memory_loads_in_memory() {
    let dir = TempDir::new("known_memory");
    let root = on_disk_repo(&dir, "music");
    let path = dir.join("config.toml");
    std::fs::write(&path, format!("[[known]]\nroot = {:?}\nin-memory = true\n", root)).unwrap();
    let state = Arc::new(AppState::new().with_config_file(path));
    let app = routes::build(state.clone());
    let (_, body) = request(&app, "GET", "/repos/known", None).await;
    assert_eq!(body[0]["in_memory"], true, "{body}");
    let (status, body) = request(&app, "POST", "/repos/load", Some(json!({"name": "music"}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let uuid: uuid::Uuid = body["repo_uuid"].as_str().unwrap().parse().unwrap();
    assert!(state.repo(uuid).unwrap().held_in_memory());
}

#[tokio::test]
async fn test_a_load_names_exactly_one_of_root_metafolder_or_name() {
    let dir = TempDir::new("known_exclusive");
    let root = on_disk_repo(&dir, "music");
    let app = app_with_config(&dir, &format!("[[known]]\nroot = {:?}\n", root));
    let (status, body) = request(
        &app,
        "POST",
        "/repos/load",
        Some(json!({"name": "music", "root": root.to_str().unwrap()})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn test_without_a_configuration_file_nothing_is_known() {
    let app = routes::build(Arc::new(AppState::new()));
    let (status, body) = request(&app, "GET", "/repos/known", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!([]));
}
