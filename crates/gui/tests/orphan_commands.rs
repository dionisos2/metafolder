//! `orphan:detect` / `orphan:delete` (doc "Orphans in the GUI"): the shell builtins
//! that mark the active repo's orphaned metarecords and delete the marked set.
//! Tests run a stub daemon on an ephemeral port.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Json;
use metafolder_gui::commands::StatusTimeouts;
use metafolder_gui::daemon_proxy::DaemonProxy;
use metafolder_gui::events;
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
    /// When set, every route answers this status and body instead.
    refusal: Option<(u16, Value)>,
}

impl Stub {
    fn answer(&self, body: Value) -> Response {
        match &self.refusal {
            Some((status, body)) => {
                (StatusCode::from_u16(*status).unwrap(), Json(body.clone())).into_response()
            }
            None => Json(body).into_response(),
        }
    }
}

async fn spawn_stub(mark: Value, marked_count: u64) -> (String, Stub) {
    spawn_stub_with(mark, marked_count, None).await
}

async fn spawn_refusing_stub(status: u16, body: Value) -> (String, Stub) {
    spawn_stub_with(json!({}), 0, Some((status, body))).await
}

async fn spawn_stub_with(
    mark: Value,
    marked_count: u64,
    refusal: Option<(u16, Value)>,
) -> (String, Stub) {
    let stub = Stub { mark, marked_count, deletes: Arc::new(Mutex::new(Vec::new())), refusal };
    let router = axum::Router::new()
        .route(
            "/repos/{repo}/orphans/mark",
            post(|State(stub): State<Stub>| async move { stub.answer(stub.mark.clone()) }),
        )
        .route(
            "/repos/{repo}/query",
            post(|State(stub): State<Stub>, Json(_body): Json<Value>| async move {
                stub.answer(json!({"results": [], "next_cursor": null, "total": stub.marked_count}))
            }),
        )
        .route(
            "/repos/{repo}/query/delete",
            post(|State(stub): State<Stub>, Json(body): Json<Value>| async move {
                stub.deletes.lock().unwrap().push(body);
                stub.answer(json!({"deleted": stub.marked_count}))
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

fn timeouts() -> StatusTimeouts {
    StatusTimeouts { message_ms: 222, error_ms: 111 }
}

/// Like [`setup`], keeping the notifier to read the statuses back.
fn setup_watched(url: &str) -> (Arc<GuiState>, Arc<DaemonProxy>, String, Arc<RecordingNotifier>) {
    let notifier = Arc::new(RecordingNotifier::new());
    let gui = Arc::new(GuiState::new(notifier.clone()));
    let ws = gui.create_workspace(Some("cafe".into()));
    (gui, Arc::new(DaemonProxy::new(url.to_string())), ws, notifier)
}

/// `(text, kind, timeout_ms)` of the last status posted.
fn last_status(notifier: &RecordingNotifier) -> (String, String, Value) {
    let p = notifier.payloads(events::STATUS_MESSAGE).pop().expect("a status was posted");
    (
        p["text"].as_str().unwrap().into(),
        p["kind"].as_str().unwrap().into(),
        p["timeout_ms"].clone(),
    )
}

#[tokio::test]
async fn test_detect_summarises_what_it_marked() {
    let (url, _stub) = spawn_stub(json!({"orphans": 3, "marked": 2, "unmarked": 1}), 3).await;
    let (gui, daemon, ws, notifier) = setup_watched(&url);

    orphan::detect(gui.clone(), daemon, ws.clone(), timeouts()).await.unwrap();

    let summary = "3 orphaned metarecords marked orphan = true (2 new, 1 no longer orphaned).";
    assert_eq!(last_status(&notifier), (summary.into(), "info".into(), json!(222)));
    let messages = gui.messages(&ws).unwrap();
    assert_eq!(messages.last().unwrap().text, summary, "kept in the message log too");
}

#[tokio::test]
async fn test_detect_finding_nothing_says_so() {
    let (url, _stub) = spawn_stub(json!({"orphans": 0, "marked": 0, "unmarked": 4}), 0).await;
    let (gui, daemon, ws, notifier) = setup_watched(&url);

    assert_eq!(orphan::detect(gui, daemon, ws, timeouts()).await.unwrap(), 0);

    assert_eq!(last_status(&notifier).0, "No orphans — every tracked metarecord has its file.");
}

#[tokio::test]
async fn test_detect_against_a_daemon_without_the_endpoint() {
    let (url, _stub) = spawn_refusing_stub(404, json!({"error": "not found"})).await;
    let (gui, daemon, ws, notifier) = setup_watched(&url);

    let error = orphan::detect(gui, daemon, ws, timeouts()).await.unwrap_err();

    assert_eq!(error, "this daemon does not support orphan marking");
    assert_eq!(last_status(&notifier), (error, "error".into(), json!(111)));
}

#[tokio::test]
async fn test_detect_refused_reports_the_daemon_error_or_a_default() {
    let (url, _stub) = spawn_refusing_stub(503, json!({"error": "repository loading"})).await;
    let (gui, daemon, ws, _) = setup_watched(&url);
    assert_eq!(
        orphan::detect(gui, daemon, ws, timeouts()).await.unwrap_err(),
        "repository loading"
    );

    let (url, _stub) = spawn_refusing_stub(500, json!({})).await;
    let (gui, daemon, ws, _) = setup_watched(&url);
    assert_eq!(
        orphan::detect(gui, daemon, ws, timeouts()).await.unwrap_err(),
        "orphan detection failed"
    );
}

#[tokio::test]
async fn test_detect_with_the_daemon_down_reports_it() {
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let (gui, daemon, ws, notifier) = setup_watched(&format!("http://127.0.0.1:{port}"));

    let error = orphan::detect(gui, daemon, ws, timeouts()).await.unwrap_err();

    assert_eq!(last_status(&notifier), (error, "error".into(), json!(111)));
}

#[tokio::test]
async fn test_count_refused_reports_the_daemon_error() {
    let (url, _stub) = spawn_refusing_stub(400, json!({"error": "bad query"})).await;
    let (gui, daemon, ws) = setup(&url);
    assert_eq!(orphan::count(gui, daemon, ws).await.unwrap_err(), "bad query");

    let (url, _stub) = spawn_refusing_stub(500, json!({})).await;
    let (gui, daemon, ws) = setup(&url);
    assert_eq!(
        orphan::count(gui, daemon, ws).await.unwrap_err(),
        "counting the marked metarecords failed"
    );
}

#[tokio::test]
async fn test_delete_summarises_and_names_the_undo() {
    let (url, _stub) = spawn_stub(json!({}), 1).await;
    let (gui, daemon, ws, notifier) = setup_watched(&url);

    orphan::delete(gui, daemon, ws, timeouts()).await.unwrap();

    assert_eq!(
        last_status(&notifier),
        ("Deleted 1 orphaned metarecord — undo takes them back.".into(), "info".into(), json!(222))
    );
}

#[tokio::test]
async fn test_delete_refused_leaves_the_list_alone() {
    let (url, _stub) = spawn_refusing_stub(400, json!({"error": "referenced elsewhere"})).await;
    let (gui, daemon, ws, notifier) = setup_watched(&url);

    let error = orphan::delete(gui.clone(), daemon, ws.clone(), timeouts()).await.unwrap_err();

    assert_eq!(error, "referenced elsewhere");
    assert_eq!(last_status(&notifier).1, "error");
    assert_eq!(gui.get_var(&ws, "metarecords:dirty").unwrap(), Value::Null, "nothing changed");

    let (url, _stub) = spawn_refusing_stub(500, json!({})).await;
    let (gui, daemon, ws, _) = setup_watched(&url);
    assert_eq!(
        orphan::delete(gui, daemon, ws, timeouts()).await.unwrap_err(),
        "deleting the marked metarecords failed"
    );
}
