//! The GUI's trashing (spec-trash "What trashing does, in order"): the
//! file-manager's delete operates on a raw filesystem path, but a path the
//! repository tracks still has to take its metarecords with it — otherwise the
//! file vanishes, the watcher finds a metarecord pointing at nothing, and the
//! orphan the redesign set out to abolish is back. Tests run a stub daemon on
//! an ephemeral port.

mod common;

use axum::extract::State;
use axum::routing::{get, post};
use axum::Json;
use common::TempDir;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct Stub {
    /// Bodies of the received POST /repos/:repo/metarecords/trash calls.
    deletes: Arc<Mutex<Vec<Value>>>,
    /// Whether a metarecord tracks the queried path.
    tracked: bool,
}

const UUID: &str = "11111111111111111111111111111111";

async fn spawn_stub(tracked: bool) -> (String, Stub) {
    let stub = Stub { deletes: Arc::new(Mutex::new(Vec::new())), tracked };
    let router = axum::Router::new()
        .route(
            "/repos/:repo/query",
            post(|State(stub): State<Stub>, Json(body): Json<Value>| async move {
                // Two questions reach this route: "what tracks this path"
                // (`eq`) and "what lives under it" (`follows_transitive`).
                let hits = match body["query"]["type"].as_str() {
                    Some("eq") if stub.tracked => json!([UUID]),
                    _ => json!([]),
                };
                Json(json!({"results": hits, "next_cursor": null}))
            }),
        )
        .route(
            "/repos/:repo/metarecords/:uuid",
            get(|| async move {
                Json(json!({
                    "uuid": UUID,
                    "version": 7,
                    "fields": [{
                        "name": "mfr_path",
                        "value": {"type": "tree_ref", "value": {"parent": null, "name": "f.txt"}}
                    }],
                }))
            }),
        )
        .route(
            "/repos/:repo/metarecords/trash",
            post(|State(stub): State<Stub>, Json(body): Json<Value>| async move {
                stub.deletes.lock().unwrap().push(body);
                Json(json!({"deleted": 1}))
            }),
        )
        .with_state(stub.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("http://{addr}"), stub)
}

/// A repo root holding `f.txt`, with its `.metafolder/internal` alongside.
fn repo_with_a_file(dir: &TempDir) -> (std::path::PathBuf, String) {
    let root = dir.path().to_path_buf();
    let internal = root.join(".metafolder/internal");
    std::fs::create_dir_all(&internal).unwrap();
    std::fs::write(root.join("f.txt"), b"data").unwrap();
    (root, internal.to_string_lossy().into_owned())
}

#[tokio::test]
async fn trashing_a_tracked_path_deletes_its_metarecords_before_the_bytes_move() {
    let (base, stub) = spawn_stub(true).await;
    let dir = TempDir::new("gui_trash_tracked");
    let (root, internal) = repo_with_a_file(&dir);
    let file = root.join("f.txt");
    let (r, f) = (root.clone(), file.clone());

    let name = tokio::task::spawn_blocking(move || {
        metafolder_gui::trash::trash_tracked(&base, "repo", &internal, &f, Some(&r), None)
    })
    .await
    .unwrap()
    .expect("the trashing succeeds");
    assert_eq!(name, "f.txt");

    let deletes = stub.deletes.lock().unwrap();
    assert_eq!(deletes.len(), 1, "the metarecords go through the daemon's trash endpoint");
    assert_eq!(deletes[0]["uuids"], json!([UUID]), "the tracked record is the one deleted");
    assert!(!root.join("f.txt").exists(), "and the bytes have moved");
}

#[tokio::test]
async fn trashing_an_untracked_path_moves_only_the_bytes() {
    let (base, stub) = spawn_stub(false).await;
    let dir = TempDir::new("gui_trash_untracked");
    let (root, internal) = repo_with_a_file(&dir);
    let file = root.join("f.txt");
    let (r, f) = (root.clone(), file.clone());

    tokio::task::spawn_blocking(move || {
        metafolder_gui::trash::trash_tracked(&base, "repo", &internal, &f, Some(&r), None)
    })
    .await
    .unwrap()
    .expect("an untracked file still trashes");

    assert!(stub.deletes.lock().unwrap().is_empty(), "nothing to delete, nothing asked");
    assert!(!root.join("f.txt").exists(), "the bytes moved all the same");
}
