//! The GUI's work context (doc "The GUI's work context"): a label the user
//! opens on a repository, carried by every later request to that repository
//! (`X-Metafolder-Label`) until it is closed, and remembered in the
//! repository's `.metafolder/gui/context` so a restart can offer it back.
//!
//! A stub daemon on an ephemeral port serves the repository listing and the
//! label routes, and records the label each write was sent in.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use metafolder_core::context::LABEL_HEADER;
use metafolder_gui::daemon_proxy::DaemonProxy;
use metafolder_gui::work_context::{self, Pending, WorkContext};
use serde_json::{json, Value};

mod common;

#[derive(Default)]
struct Stub {
    /// The labels that exist; a prune removes one.
    labels: HashSet<i64>,
    next: i64,
    /// The label header of each label creation, in order.
    created_in: Vec<Option<String>>,
    /// The label header of each write, in order.
    writes_in: Vec<Option<String>>,
}

type Shared = Arc<Mutex<Stub>>;

fn label_of(headers: &HeaderMap) -> Option<String> {
    headers.get(LABEL_HEADER).and_then(|v| v.to_str().ok()).map(str::to_string)
}

/// The stub daemon, serving one repository `repo` whose `.metafolder/` is
/// `metafolder`.
async fn spawn_stub(repo: &str, metafolder: PathBuf) -> (String, Shared) {
    let stub: Shared = Arc::default();
    let internal = metafolder.join("internal").to_str().unwrap().to_string();
    let listing = json!([{"repo_uuid": repo, "name": "r", "root": "/r", "internal_dir": internal}]);
    let (create, read, write) = (stub.clone(), stub.clone(), stub.clone());
    let router = Router::new()
        .route("/repos", get(move || async move { Json(listing) }))
        .route(
            "/repos/:repo/labels",
            post(move |headers: HeaderMap, Json(body): Json<Value>| async move {
                let mut stub = create.lock().unwrap();
                stub.created_in.push(label_of(&headers));
                stub.next += 1;
                let id = stub.next;
                stub.labels.insert(id);
                Json(json!({"id": id, "name": body["name"], "parent": null}))
            }),
        )
        .route(
            "/repos/:repo/labels/:id",
            get(move |Path((_, id)): Path<(String, i64)>| async move {
                if read.lock().unwrap().labels.contains(&id) {
                    (StatusCode::OK, Json(json!({"id": id})))
                } else {
                    (StatusCode::NOT_FOUND, Json(json!({"error": format!("no label {id}")})))
                }
            }),
        )
        .route(
            "/repos/:repo/metarecords",
            post(move |headers: HeaderMap| async move {
                let mut stub = write.lock().unwrap();
                let label = label_of(&headers);
                stub.writes_in.push(label.clone());
                match label.map(|l| l.parse::<i64>().unwrap()) {
                    Some(id) if !stub.labels.contains(&id) => {
                        (StatusCode::BAD_REQUEST, Json(json!({"error": format!("no label {id}")})))
                    }
                    _ => (StatusCode::OK, Json(json!({}))),
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (format!("http://127.0.0.1:{port}"), stub)
}

/// A repository of its own (the contexts are the process's, and the tests run
/// in parallel), its `.metafolder/`, the guard holding it, and a stub serving it.
async fn setup(prefix: &str) -> (String, PathBuf, common::TempDir, DaemonProxy, Shared) {
    let repo = uuid::Uuid::new_v4().simple().to_string();
    let root = common::TempDir::new(prefix);
    let metafolder = root.join(".metafolder");
    std::fs::create_dir_all(&metafolder).unwrap();
    let (url, stub) = spawn_stub(&repo, metafolder.clone()).await;
    (repo, metafolder, root, DaemonProxy::new(url), stub)
}

fn file(metafolder: &std::path::Path) -> PathBuf {
    metafolder.join("gui").join("context")
}

async fn write(proxy: &DaemonProxy, repo: &str) -> u16 {
    proxy.request("POST", &format!("/repos/{repo}/metarecords"), None).await.unwrap().status
}

#[tokio::test]
async fn a_context_set_is_carried_by_every_write_to_its_repository_until_unset() {
    let (repo, metafolder, _root, proxy, stub) = setup("work_context_set").await;

    let opened = work_context::set(&proxy, &repo, "sorting photos", None).await.unwrap();
    assert_eq!(opened, WorkContext { label: 1, name: "sorting photos".into(), description: None });
    assert!(file(&metafolder).exists(), "remembered in .metafolder/gui/context");
    assert_eq!(work_context::active(&repo), Some(opened.clone()));

    assert_eq!(write(&proxy, &repo).await, 200);
    // Another repository's requests carry nothing: a label id means nothing there.
    let other = uuid::Uuid::new_v4().simple().to_string();
    let _ = proxy.request("POST", &format!("/repos/{other}/metarecords"), None).await;

    // One at a time: setting another closes the first, and the new label is
    // not opened inside it.
    let second = work_context::set(&proxy, &repo, "tagging", Some("the 2024 trip")).await.unwrap();
    assert_eq!(second.label, 2);
    assert_eq!(second.description.as_deref(), Some("the 2024 trip"), "the description is kept");
    assert_eq!(work_context::read_file(&metafolder).unwrap(), Some(second.clone()));
    assert_eq!(write(&proxy, &repo).await, 200);

    let closed = work_context::unset(&proxy, &repo).await.unwrap();
    assert_eq!(closed, Some(second));
    assert!(!file(&metafolder).exists(), "closing forgets the file");
    assert_eq!(write(&proxy, &repo).await, 200);

    let stub = stub.lock().unwrap();
    assert_eq!(stub.created_in, [None, None], "a context is opened at the root");
    assert_eq!(
        stub.writes_in,
        [Some("1".to_string()), None, Some("2".to_string()), None],
        "each write in the context open on its repository at the time — none for \
         another repository's, none once it is closed"
    );
}

#[tokio::test]
async fn a_remembered_context_is_offered_back_and_resumed() {
    let (repo, metafolder, _root, proxy, stub) = setup("work_context_resume").await;
    work_context::set(&proxy, &repo, "sorting photos", None).await.unwrap();
    // A restart: the GUI's memory is gone, the file is not.
    work_context::forget(&repo);
    assert_eq!(work_context::active(&repo), None);
    assert!(file(&metafolder).exists());

    let expected = WorkContext { label: 1, name: "sorting photos".into(), description: None };
    assert_eq!(
        work_context::pending(&proxy, &repo).await.unwrap(),
        Pending::Open(expected.clone())
    );
    assert_eq!(work_context::resume(&proxy, &repo).await.unwrap(), Some(expected.clone()));
    assert_eq!(
        work_context::pending(&proxy, &repo).await.unwrap(),
        Pending::None,
        "already active"
    );
    assert_eq!(write(&proxy, &repo).await, 200);
    assert_eq!(stub.lock().unwrap().writes_in, [Some("1".to_string())]);

    // Ending it at the question: the file goes, nothing is offered again.
    work_context::forget(&repo);
    assert_eq!(work_context::unset(&proxy, &repo).await.unwrap(), Some(expected));
    assert!(!file(&metafolder).exists());
    assert_eq!(work_context::pending(&proxy, &repo).await.unwrap(), Pending::None);
}

#[tokio::test]
async fn a_remembered_context_whose_label_was_pruned_is_dropped() {
    let (repo, metafolder, _root, proxy, stub) = setup("work_context_gone").await;
    work_context::set(&proxy, &repo, "old work", None).await.unwrap();
    work_context::forget(&repo);
    stub.lock().unwrap().labels.clear();

    let gone = WorkContext { label: 1, name: "old work".into(), description: None };
    assert_eq!(work_context::pending(&proxy, &repo).await.unwrap(), Pending::Gone(gone));
    assert!(!file(&metafolder).exists(), "nothing to offer next time");
}

#[tokio::test]
async fn a_write_whose_context_was_pruned_meanwhile_is_made_outside_it() {
    let (repo, metafolder, _root, proxy, stub) = setup("work_context_pruned").await;
    work_context::set(&proxy, &repo, "work", None).await.unwrap();
    // `mf log prune` from a terminal takes the label away during the session.
    stub.lock().unwrap().labels.clear();

    assert_eq!(write(&proxy, &repo).await, 200, "the write is not lost");
    assert_eq!(work_context::active(&repo), None, "the context is closed");
    assert!(!file(&metafolder).exists());
    assert_eq!(stub.lock().unwrap().writes_in, [Some("1".to_string()), None]);
}

#[tokio::test]
async fn every_change_of_a_context_is_signalled() {
    let (repo, _metafolder, _root, proxy, _stub) = setup("work_context_signal").await;
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = count.clone();
    work_context::on_change(move || {
        seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    });
    let changes = || count.load(std::sync::atomic::Ordering::SeqCst);
    // Other tests change contexts meanwhile: only "it grew" can be asserted.
    let before = changes();
    work_context::set(&proxy, &repo, "a", None).await.unwrap();
    let after_set = changes();
    assert!(after_set > before, "set is signalled");
    work_context::unset(&proxy, &repo).await.unwrap();
    assert!(changes() > after_set, "unset is signalled");
}

#[test]
fn a_file_without_a_description_is_still_read() {
    let root = common::TempDir::new("work_context_old_file");
    std::fs::create_dir_all(root.join("gui")).unwrap();
    std::fs::write(root.join("gui/context"), r#"{"label": 3, "name": "n"}"#).unwrap();
    let expected = WorkContext { label: 3, name: "n".into(), description: None };
    assert_eq!(work_context::read_file(&root).unwrap(), Some(expected));
}

#[test]
fn the_file_holds_the_label_and_its_name() {
    let root = common::TempDir::new("work_context_file");
    let context = WorkContext { label: 42, name: "a name".into(), description: Some("why".into()) };
    work_context::write_file(&root, &context).unwrap();
    assert_eq!(work_context::read_file(&root).unwrap(), Some(context));
    std::fs::write(root.join("gui/context"), "not json").unwrap();
    assert!(work_context::read_file(&root).is_err(), "a malformed file is an error, not a context");
    let empty = common::TempDir::new("work_context_none");
    assert_eq!(work_context::read_file(&empty).unwrap(), None);
}
