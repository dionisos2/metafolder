//! `orphan:detect` / `orphan:delete` (spec-gui "Orphans"): the shell builtins
//! that mark the active repo's orphaned metarecords and delete the marked set.
//! Tests run a stub daemon on an ephemeral port.

use axum::extract::State;
use axum::routing::post;
use axum::Json;
use metafolder_gui::daemon_proxy::DaemonProxy;
use metafolder_gui::notifier::RecordingNotifier;
use metafolder_gui::orphan;
use metafolder_gui::state::GuiState;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct Stub {
    /// Body served by POST /repos/:repo/orphans/mark.
    mark: Value,
    /// How many metarecords `POST /query` reports (`total`).
    marked_count: u64,
    /// Bodies of the received POST /repos/:repo/query/delete calls.
    deletes: Arc<Mutex<Vec<Value>>>,
}

async fn spawn_stub(mark: Value, marked_count: u64) -> (String, Stub) {
    let stub = Stub { mark, marked_count, deletes: Arc::new(Mutex::new(Vec::new())) };
    let router = axum::Router::new()
        .route(
            "/repos/:repo/orphans/mark",
            post(|State(stub): State<Stub>| async move { Json(stub.mark.clone()) }),
        )
        .route(
            "/repos/:repo/query",
            post(|State(stub): State<Stub>, Json(_body): Json<Value>| async move {
                Json(json!({"results": [], "next_cursor": null, "total": stub.marked_count}))
            }),
        )
        .route(
            "/repos/:repo/query/delete",
            post(|State(stub): State<Stub>, Json(body): Json<Value>| async move {
                stub.deletes.lock().unwrap().push(body);
                Json(json!({"deleted": stub.marked_count}))
            }),
        )
        .with_state(stub.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (format!("http://127.0.0.1:{port}"), stub)
}

fn setup(url: &str) -> (Arc<GuiState>, Arc<DaemonProxy>, String) {
    let gui = Arc::new(GuiState::new(Arc::new(RecordingNotifier::new())));
    let ws = gui.create_workspace(Some("cafe".into()));
    (gui, Arc::new(DaemonProxy::new(url.to_string())), ws)
}

#[tokio::test]
async fn test_detect_reports_the_counts_and_marks_dirty() {
    let (url, _stub) = spawn_stub(json!({"orphans": 3, "marked": 2, "unmarked": 1}), 3).await;
    let (gui, daemon, ws) = setup(&url);

    let total = orphan::detect(gui.clone(), daemon, ws.clone(), Default::default()).await.unwrap();

    assert_eq!(total, 3, "the caller needs the count to confirm a deletion");
    // Panels refresh through the metarecords:dirty workspace variable.
    assert_ne!(gui.get_var(&ws, "metarecords:dirty").unwrap(), Value::Null);
}

#[tokio::test]
async fn test_detect_without_an_active_repo_is_an_error() {
    let (url, _stub) = spawn_stub(json!({"orphans": 0, "marked": 0, "unmarked": 0}), 0).await;
    let gui = Arc::new(GuiState::new(Arc::new(RecordingNotifier::new())));
    let ws = gui.create_workspace(None);
    let daemon = Arc::new(DaemonProxy::new(url));

    let error = orphan::detect(gui, daemon, ws, Default::default()).await.unwrap_err();
    assert!(error.contains("repository"), "error: {error}");
}

#[tokio::test]
async fn test_count_asks_for_the_marked_set() {
    let (url, _stub) = spawn_stub(json!({}), 7).await;
    let (gui, daemon, ws) = setup(&url);

    assert_eq!(orphan::count(gui, daemon, ws).await.unwrap(), 7);
}

#[tokio::test]
async fn test_delete_deletes_exactly_the_marked_set() {
    let (url, stub) = spawn_stub(json!({}), 2).await;
    let (gui, daemon, ws) = setup(&url);

    let deleted =
        orphan::delete(gui.clone(), daemon, ws.clone(), Default::default()).await.unwrap();

    assert_eq!(deleted, 2);
    let deletes = stub.deletes.lock().unwrap();
    assert_eq!(
        deletes.as_slice(),
        [json!({"query": {"type": "eq", "field": "orphan",
                          "value": {"type": "bool", "value": true}}})],
        "the deletion is a plain query over the marker, not a uuid list"
    );
    assert_ne!(gui.get_var(&ws, "metarecords:dirty").unwrap(), Value::Null);
}
