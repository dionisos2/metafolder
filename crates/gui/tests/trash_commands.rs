//! The GUI's trashing (doc "What trashing does"): the
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
    /// Where the repository is, for `GET /repos/:repo`.
    root: String,
}

const UUID: &str = "11111111111111111111111111111111";

async fn spawn_stub(tracked: bool) -> (String, Stub) {
    spawn_stub_at(tracked, String::new()).await
}

async fn spawn_stub_at(tracked: bool, root: String) -> (String, Stub) {
    let stub = Stub { deletes: Arc::new(Mutex::new(Vec::new())), tracked, root };
    let router = axum::Router::new()
        .route(
            "/repos/:repo",
            get(|State(stub): State<Stub>| async move {
                Json(json!({
                    "root": stub.root,
                    "internal_dir": format!("{}/.metafolder/internal", stub.root),
                }))
            }),
        )
        .route(
            "/repos/:repo/metarecords/:uuid/fields/mfr_path/resolve-tree",
            // The selected record's file, while it is tracked.
            get(|State(stub): State<Stub>| async move {
                let paths = if stub.tracked { json!(["/f.txt"]) } else { json!([]) };
                Json(json!({"paths": paths}))
            }),
        )
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

/// Runs a blocking trash worker off the async runtime the stub daemon needs.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f).await.unwrap()
}

#[tokio::test]
async fn trashing_the_selected_metarecord_trashes_its_file_and_the_record() {
    let dir = TempDir::new("gui_trash_selected");
    let (root, _internal) = repo_with_a_file(&dir);
    let (base, stub) = spawn_stub_at(true, root.to_string_lossy().into_owned()).await;

    let name = blocking(move || {
        metafolder_gui::trash::trash_selected_blocking(base, UUID.into(), "repo".into())
    })
    .await
    .expect("the selected record's file trashes");

    assert_eq!(name, "f.txt");
    assert_eq!(stub.deletes.lock().unwrap()[0]["uuids"], json!([UUID]));
    assert!(!root.join("f.txt").exists());
}

#[tokio::test]
async fn a_selected_metarecord_without_a_file_is_refused() {
    let dir = TempDir::new("gui_trash_selected_gone");
    let (root, _internal) = repo_with_a_file(&dir);
    let (base, stub) = spawn_stub_at(false, root.to_string_lossy().into_owned()).await;

    let error = blocking(move || {
        metafolder_gui::trash::trash_selected_blocking(base, UUID.into(), "repo".into())
    })
    .await
    .unwrap_err();

    assert!(error.contains("no file"), "error: {error}");
    assert!(stub.deletes.lock().unwrap().is_empty());
    assert!(root.join("f.txt").exists(), "nothing moved");
}

/// Trashes the untracked `f.txt` of `root` and returns its trash entry id.
async fn trash_untracked(base: &str, root: &std::path::Path) -> String {
    let (base, r) = (base.to_string(), root.to_path_buf());
    let internal = root.join(".metafolder/internal");
    let dir = metafolder_core::trash::TrashDir::new(internal.join("trash"));
    blocking(move || {
        let internal = internal.to_string_lossy().into_owned();
        metafolder_gui::trash::trash_tracked(
            &base,
            "repo",
            &internal,
            &r.join("f.txt"),
            Some(&r),
            None,
        )
    })
    .await
    .unwrap();
    dir.entries().unwrap().pop().unwrap().id
}

#[tokio::test]
async fn restoring_an_entry_puts_the_file_back_where_it_was() {
    let dir = TempDir::new("gui_trash_restore");
    let (root, _internal) = repo_with_a_file(&dir);
    let (base, _stub) = spawn_stub_at(false, root.to_string_lossy().into_owned()).await;
    let id = trash_untracked(&base, &root).await;
    assert!(!root.join("f.txt").exists());

    let restored =
        blocking(move || metafolder_gui::trash::restore_blocking(base, "repo".into(), id))
            .await
            .expect("the restore succeeds");

    assert_eq!(restored, root.join("f.txt").display().to_string());
    assert_eq!(std::fs::read(root.join("f.txt")).unwrap(), b"data");
}

#[tokio::test]
async fn a_restore_onto_an_occupied_path_is_refused_and_moves_nothing() {
    let dir = TempDir::new("gui_trash_restore_occupied");
    let (root, _internal) = repo_with_a_file(&dir);
    let (base, _stub) = spawn_stub_at(false, root.to_string_lossy().into_owned()).await;
    let id = trash_untracked(&base, &root).await;
    std::fs::write(root.join("f.txt"), b"newer").unwrap();

    let result =
        blocking(move || metafolder_gui::trash::restore_blocking(base, "repo".into(), id)).await;

    assert!(result.is_err(), "the restore is refused: {result:?}");
    assert_eq!(std::fs::read(root.join("f.txt")).unwrap(), b"newer", "the newer file stays");
}
