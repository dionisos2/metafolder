//! `log:undo` / `log:redo` (doc "Log commands in the GUI"): shell builtins
//! reverting the active repo's newest change, or its newest undo, through the
//! daemon's revert API (`core::navigation`). Tests run a stub daemon on an
//! ephemeral port.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Json;
use metafolder_gui::commands::StatusTimeouts;
use metafolder_gui::daemon_proxy::DaemonProxy;
use metafolder_gui::events;
use metafolder_gui::notifier::RecordingNotifier;
use metafolder_gui::state::GuiState;
use metafolder_gui::undo;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct Stub {
    /// Body served by GET /repos/:repo/log.
    log: Value,
    /// Bodies of the received POST /repos/:repo/revert calls.
    reverts: Arc<Mutex<Vec<Value>>>,
    /// Body served by GET /repos/:repo/revert/plan.
    revert_plan: Arc<Mutex<Value>>,
    /// Whether POST /repos/:repo/revert refuses (500).
    revert_refused: Arc<Mutex<bool>>,
}

async fn spawn_stub(log: Value) -> (String, Stub) {
    let stub = Stub {
        log,
        reverts: Arc::new(Mutex::new(Vec::new())),
        revert_plan: Arc::new(Mutex::new(json!({"requires_lock": false,
                                                "operations": [{"id": 9}]}))),
        revert_refused: Arc::new(Mutex::new(false)),
    };
    let router = axum::Router::new()
        // Where the repository is: the revert locates the trash-bin.
        .route(
            "/repos/:repo",
            get(|| async {
                Json(json!({"root": "/nowhere", "internal_dir": "/nowhere/.metafolder/internal"}))
            }),
        )
        .route(
            "/repos/:repo/log",
            get(|State(stub): State<Stub>| async move { Json(stub.log.clone()) }),
        )
        .route(
            "/repos/:repo/revert/plan",
            get(|State(stub): State<Stub>| async move {
                Json(stub.revert_plan.lock().unwrap().clone())
            }),
        )
        .route(
            "/repos/:repo/revert",
            post(|State(stub): State<Stub>, Json(body): Json<Value>| async move {
                stub.reverts.lock().unwrap().push(body);
                if *stub.revert_refused.lock().unwrap() {
                    let refusal = Json(json!({"error": "the repository is locked"}));
                    return (StatusCode::INTERNAL_SERVER_ERROR, refusal).into_response();
                }
                Json(json!({"label": 42, "reverted_operations": [9]})).into_response()
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

fn op(id: i64, label_id: i64) -> Value {
    json!({"id": id, "label_id": label_id, "timestamp": id, "op_type": "set_field",
           "entity_uuid": "00", "field_name": "x", "origin": null})
}

/// One of the watcher's.
fn watcher_op(id: i64, label_id: i64) -> Value {
    let mut value = op(id, label_id);
    value["origin"] = json!("watcher");
    value
}

/// The same, undoing `reverts`.
fn revert_op(id: i64, label_id: i64, reverts: i64) -> Value {
    let mut value = op(id, label_id);
    value["reverts_op_id"] = json!(reverts);
    value
}

fn timeouts() -> StatusTimeouts {
    StatusTimeouts { message_ms: 222, error_ms: 111 }
}

fn setup(url: &str) -> (Arc<GuiState>, Arc<DaemonProxy>, String, Arc<RecordingNotifier>) {
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

/// The root metarecord (label 1, the log's first operation), then two of the
/// user's writes: label 2 = ops 2-3, label 3 = op 4.
fn user_log() -> Value {
    json!({
        "head": 4,
        "first": 1,
        "operations": [op(1, 1), op(2, 2), op(3, 2), op(4, 3)],
    })
}

/// The user's change (rev 2), then one of the watcher's (rev 3).
fn log_with_the_watcher_on_top() -> Value {
    json!({
        "head": 3,
        "first": 1,
        "operations": [op(1, 1), op(2, 2), watcher_op(3, 3)],
    })
}

#[tokio::test]
async fn test_undo_reverts_the_newest_change_and_marks_dirty() {
    let (url, stub) = spawn_stub(user_log()).await;
    let (gui, daemon, ws, notifier) = setup(&url);

    undo::navigate(gui.clone(), daemon, ws.clone(), false, None, timeouts()).await.unwrap();

    assert_eq!(
        stub.reverts.lock().unwrap().as_slice(),
        [json!({"target": {"label_id": 3, "only": "changes"}})]
    );
    assert_ne!(gui.get_var(&ws, "metarecords:dirty").unwrap(), Value::Null);
    let (text, kind, _) = last_status(&notifier);
    assert!(text.starts_with("Undid label 3"), "status: {text}");
    assert_eq!(kind, "info");
}

#[tokio::test]
async fn test_undo_under_the_watcher_reverts_the_users_change() {
    let (url, stub) = spawn_stub(log_with_the_watcher_on_top()).await;
    let (gui, daemon, ws, notifier) = setup(&url);

    undo::navigate(gui, daemon, ws, false, None, timeouts()).await.unwrap();

    assert_eq!(
        stub.reverts.lock().unwrap().as_slice(),
        [json!({"target": {"label_id": 2, "only": "changes"}})]
    );
    assert_eq!(
        last_status(&notifier),
        (
            "Undid label 2 (1 operations) — the daemon's own operations were left in place.".into(),
            "info".into(),
            json!(222)
        )
    );
}

/// A later change wrote the same cell: nothing blocks the undo (doc "Reverting
/// onto the current state") — it reverts onto the current state.
#[tokio::test]
async fn test_an_undo_over_a_cell_changed_since_still_reverts() {
    let (url, stub) = spawn_stub(log_with_the_watcher_on_top()).await;
    *stub.revert_plan.lock().unwrap() = json!({
        "requires_lock": false,
        "operations": [{"id": 2, "changed_since": {"op_id": 3, "label_id": 7}}],
    });
    let (gui, daemon, ws, _notifier) = setup(&url);

    undo::navigate(gui, daemon, ws, false, None, timeouts()).await.unwrap();

    assert_eq!(stub.reverts.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn test_undo_with_nothing_to_undo_says_so() {
    let log = json!({"head": 1, "first": 1, "operations": [op(1, 1)]});
    let (url, stub) = spawn_stub(log).await;
    let (gui, daemon, ws, notifier) = setup(&url);

    undo::navigate(gui, daemon, ws, false, None, timeouts()).await.unwrap();

    assert!(stub.reverts.lock().unwrap().is_empty());
    assert_eq!(last_status(&notifier), ("Nothing to undo.".into(), "info".into(), json!(222)));
}

/// The case the history tree lost (doc "Why the log stops being a tree"): the
/// user's change (rev 2), its undo (rev 3), then the watcher (rev 4). Redo
/// steps over the watcher and reverts the undo.
#[tokio::test]
async fn test_redo_reverts_the_undo_across_the_watchers_work() {
    let log = json!({
        "head": 4,
        "first": 1,
        "operations": [op(1, 1), op(2, 2), revert_op(3, 3, 2), watcher_op(4, 4)],
    });
    let (url, stub) = spawn_stub(log).await;
    let (gui, daemon, ws, notifier) = setup(&url);

    undo::navigate(gui.clone(), daemon, ws.clone(), true, None, timeouts()).await.unwrap();

    assert_eq!(
        stub.reverts.lock().unwrap().as_slice(),
        [json!({"target": {"label_id": 3, "only": "undos"}})]
    );
    assert_ne!(gui.get_var(&ws, "metarecords:dirty").unwrap(), Value::Null);
    let text = last_status(&notifier).0;
    assert!(text.starts_with("Redid label 3"), "status: {text}");
}

#[tokio::test]
async fn test_redo_does_nothing_when_your_newest_change_is_not_an_undo() {
    let (url, stub) = spawn_stub(user_log()).await;
    let (gui, daemon, ws, notifier) = setup(&url);

    undo::navigate(gui.clone(), daemon, ws.clone(), true, None, timeouts()).await.unwrap();

    assert!(stub.reverts.lock().unwrap().is_empty());
    assert_eq!(last_status(&notifier).0, "Nothing to redo.");
    assert_eq!(gui.get_var(&ws, "metarecords:dirty").unwrap(), Value::Null);
}

/// A revert the daemon refuses is reported on the status bar, not swallowed.
#[tokio::test]
async fn test_a_refused_revert_is_reported() {
    let (url, stub) = spawn_stub(user_log()).await;
    *stub.revert_refused.lock().unwrap() = true;
    let (gui, daemon, ws, notifier) = setup(&url);

    undo::navigate(gui.clone(), daemon, ws.clone(), false, None, timeouts()).await.unwrap();

    let (text, kind, timeout) = last_status(&notifier);
    assert!(text.contains("the repository is locked"), "status: {text}");
    assert_eq!((kind.as_str(), timeout), ("error", json!(111)));
    assert_eq!(gui.get_var(&ws, "metarecords:dirty").unwrap(), Value::Null);
}

#[tokio::test]
async fn test_no_active_repo_is_an_error() {
    let (url, _stub) = spawn_stub(user_log()).await;
    let gui = Arc::new(GuiState::new(Arc::new(RecordingNotifier::new())));
    let ws = gui.create_workspace(None);
    let daemon = Arc::new(DaemonProxy::new(url));

    assert!(undo::navigate(gui, daemon, ws, false, None, Default::default()).await.is_err());
}

/// A change written in several labels — a command inside a context — is not
/// undone without saying which: the status names them (the shell asks first).
#[tokio::test]
async fn test_undo_of_a_change_in_several_labels_names_them() {
    let log = json!({
        "head": 2,
        "first": 1,
        "operations": [op(1, 1), op(2, 3)],
        "labels": [{"id": 3, "name": "mf tag add x", "parent": 2},
                   {"id": 2, "name": "tag run", "parent": null}],
    });
    let (url, stub) = spawn_stub(log).await;
    let (gui, daemon, ws, notifier) = setup(&url);

    undo::navigate(gui.clone(), daemon.clone(), ws.clone(), false, None, timeouts()).await.unwrap();
    assert!(stub.reverts.lock().unwrap().is_empty());
    let text = last_status(&notifier).0;
    assert!(text.contains("3  mf tag add x") && text.contains("2  tag run"), "status: {text}");

    // Named, it is undone.
    undo::navigate(gui, daemon, ws, false, Some(2), timeouts()).await.unwrap();
    assert_eq!(
        stub.reverts.lock().unwrap().as_slice(),
        [json!({"target": {"label_id": 2, "only": "changes"}})]
    );
}
