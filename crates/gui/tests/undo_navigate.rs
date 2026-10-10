//! `log:undo` / `log:redo` (doc "Revision commands in the GUI"): shell builtins
//! navigating the active repo's event log through the daemon rollback
//! API — in one atomic call when no step touches a file, through the
//! coordinated protocol otherwise (`core::navigation`). Tests run a stub
//! daemon on an ephemeral port.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
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
    /// Bodies of the received POST /repos/:repo/rollback calls.
    rollbacks: Arc<Mutex<Vec<Value>>>,
    /// Bodies of the received POST /repos/:repo/revert calls.
    reverts: Arc<Mutex<Vec<Value>>>,
    /// What the plan summary says about the steps with a file action.
    filesystem_steps: u64,
    /// Bodies of the received POST /repos/:repo/rollback/start calls.
    starts: Arc<Mutex<Vec<Value>>>,
    /// Body served by GET /repos/:repo/revert/plan.
    revert_plan: Arc<Mutex<Value>>,
    /// Whether POST /repos/:repo/rollback refuses (500).
    rollback_refused: Arc<Mutex<bool>>,
}

async fn spawn_stub(log: Value) -> (String, Stub) {
    spawn_stub_with(log, 0).await
}

async fn spawn_stub_with(log: Value, filesystem_steps: u64) -> (String, Stub) {
    let stub = Stub {
        log,
        rollbacks: Arc::new(Mutex::new(Vec::new())),
        reverts: Arc::new(Mutex::new(Vec::new())),
        filesystem_steps,
        starts: Arc::new(Mutex::new(Vec::new())),
        revert_plan: Arc::new(Mutex::new(json!({"revertable": true, "requires_lock": false,
                                                "operations": [{"id": 9}], "blocked": [],
                                                "dependents": []}))),
        rollback_refused: Arc::new(Mutex::new(false)),
    };
    let router = axum::Router::new()
        // Where the repository is: the navigation locates the trash-bin.
        .route(
            "/repos/:repo",
            get(|| async {
                Json(json!({"root": "/nowhere", "internal_dir": "/nowhere/.metafolder/internal"}))
            }),
        )
        .route(
            "/repos/:repo/rollback/plan/summary",
            get(|State(stub): State<Stub>| async move {
                Json(json!({"total_operations": 2, "by_type": {"set_field": 2},
                            "revisions_affected": 1,
                            "filesystem_steps": stub.filesystem_steps}))
            }),
        )
        .route(
            "/repos/:repo/rollback/start",
            post(|State(stub): State<Stub>, Json(body): Json<Value>| async move {
                stub.starts.lock().unwrap().push(body);
                Json(json!({"op": {"id": 4, "op_type": "set_field", "entity_uuid": "00",
                                   "filesystem": null}, "remaining": 1}))
            }),
        )
        .route("/repos/:repo/rollback/step", post(|| async { Json(json!({"op": null})) }))
        .route(
            "/repos/:repo/log",
            get(|State(stub): State<Stub>| async move { Json(stub.log.clone()) }),
        )
        .route(
            "/repos/:repo/rollback",
            post(|State(stub): State<Stub>, Json(body): Json<Value>| async move {
                stub.rollbacks.lock().unwrap().push(body);
                if *stub.rollback_refused.lock().unwrap() {
                    let refusal = Json(json!({"error": "the repository is locked"}));
                    return (StatusCode::INTERNAL_SERVER_ERROR, refusal).into_response();
                }
                Json(json!({
                    "previous_head": 4,
                    "new_head": 2,
                    "operations_unapplied": 2,
                    "operations_applied": 0,
                }))
                .into_response()
            }),
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
                Json(json!({"revision": 42, "reverted_operations": [9]}))
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

fn op(id: i64, parent_id: Option<i64>, rev_id: i64) -> Value {
    json!({"id": id, "parent_id": parent_id, "rev_id": rev_id, "seq": 0,
           "op_type": "set_field", "entity_uuid": "00", "field_name": "x"})
}

/// The same, undoing `reverts`.
fn revert_op(id: i64, parent_id: Option<i64>, rev_id: i64, reverts: i64) -> Value {
    let mut value = op(id, parent_id, rev_id);
    value["reverts_op_id"] = json!(reverts);
    value
}

fn rev(id: i64, origin: Option<&str>) -> Value {
    json!({"id": id, "timestamp": id, "label": null, "origin": origin})
}

fn setup(url: &str) -> (Arc<GuiState>, Arc<DaemonProxy>, String) {
    let gui = Arc::new(GuiState::new(Arc::new(RecordingNotifier::new())));
    let ws = gui.create_workspace(Some("cafe".into()));
    (gui, Arc::new(DaemonProxy::new(url.to_string())), ws)
}

// Two revisions: rev 1 = ops 1-2, rev 2 = ops 3-4 (a linear chain).
fn linear_log(head: Value) -> Value {
    json!({
        "head": head,
        "operations": [op(1, None, 1), op(2, Some(1), 1), op(3, Some(2), 2), op(4, Some(3), 2)],
        "revisions": [{"id": 1, "timestamp": 0, "label": null},
                      {"id": 2, "timestamp": 1, "label": null}],
    })
}

#[tokio::test]
async fn test_undo_posts_prev_revision_and_marks_dirty() {
    let (url, stub) = spawn_stub(linear_log(json!(4))).await;
    let (gui, daemon, ws) = setup(&url);

    undo::navigate(gui.clone(), daemon, ws.clone(), false, Default::default()).await.unwrap();

    let rollbacks = stub.rollbacks.lock().unwrap();
    assert_eq!(rollbacks.as_slice(), [json!({"target": {"prev_revision": true}})]);
    // Panels refresh through the metarecords:dirty workspace variable.
    assert_ne!(gui.get_var(&ws, "metarecords:dirty").unwrap(), Value::Null);
}

/// A navigation with something to do on disk — a file to move, one to bring
/// back from the trash-bin or send back there — goes through the coordinated
/// protocol, step by step, instead of the metadata-only shortcut.
#[tokio::test]
async fn test_undo_touching_files_navigates_step_by_step() {
    let (url, stub) = spawn_stub_with(linear_log(json!(4)), 1).await;
    let (gui, daemon, ws) = setup(&url);

    undo::navigate(gui.clone(), daemon, ws.clone(), false, Default::default()).await.unwrap();

    assert!(stub.rollbacks.lock().unwrap().is_empty(), "not the metadata-only shortcut");
    assert_eq!(
        stub.starts.lock().unwrap().as_slice(),
        [json!({"target": {"prev_revision": true}})]
    );
    assert_ne!(gui.get_var(&ws, "metarecords:dirty").unwrap(), Value::Null);
}

#[tokio::test]
async fn test_redo_targets_the_last_op_of_heads_child_revision() {
    // HEAD on op 2 (end of rev 1): redo must re-apply all of rev 2 (op 4).
    let (url, stub) = spawn_stub(linear_log(json!(2))).await;
    let (gui, daemon, ws) = setup(&url);

    undo::navigate(gui.clone(), daemon, ws.clone(), true, Default::default()).await.unwrap();

    let rollbacks = stub.rollbacks.lock().unwrap();
    assert_eq!(rollbacks.as_slice(), [json!({"target": {"id": 4}})]);
    assert_ne!(gui.get_var(&ws, "metarecords:dirty").unwrap(), Value::Null);
}

#[tokio::test]
async fn test_redo_at_the_tip_does_nothing() {
    let (url, stub) = spawn_stub(linear_log(json!(4))).await;
    let (gui, daemon, ws) = setup(&url);

    undo::navigate(gui.clone(), daemon, ws, true, Default::default()).await.unwrap();

    assert!(stub.rollbacks.lock().unwrap().is_empty());
}

#[tokio::test]
async fn test_redo_follows_the_most_recent_branch() {
    // HEAD on op 2; two children: op 3 (rev 2, old branch) and op 5
    // (rev 3, created after a rollback). Redo follows the newest.
    let log = json!({
        "head": 2,
        "operations": [op(1, None, 1), op(2, Some(1), 1),
                       op(3, Some(2), 2), op(4, Some(3), 2),
                       op(5, Some(2), 3), op(6, Some(5), 3)],
        "revisions": [{"id": 1, "timestamp": 0, "label": null},
                      {"id": 2, "timestamp": 1, "label": null},
                      {"id": 3, "timestamp": 2, "label": null}],
    });
    let (url, stub) = spawn_stub(log).await;
    let (gui, daemon, ws) = setup(&url);

    undo::navigate(gui.clone(), daemon, ws, true, Default::default()).await.unwrap();

    let rollbacks = stub.rollbacks.lock().unwrap();
    assert_eq!(rollbacks.as_slice(), [json!({"target": {"id": 6}})]);
}

/// Redo when the last thing written is an undo and nothing has happened since:
/// HEAD steps back over the revert, which removes it as cleanly as it was made.
#[tokio::test]
async fn test_redo_rolls_back_over_the_undo_at_head() {
    let log = json!({
        "head": 3,
        "operations": [op(1, None, 1), op(2, Some(1), 2), revert_op(3, Some(2), 3, 2)],
        "revisions": [rev(1, None), rev(2, None), rev(3, None)],
    });
    let (url, stub) = spawn_stub(log).await;
    let (gui, daemon, ws) = setup(&url);

    undo::navigate(gui.clone(), daemon, ws, true, Default::default()).await.unwrap();

    assert_eq!(
        stub.rollbacks.lock().unwrap().as_slice(),
        [json!({"target": {"prev_revision": true}})]
    );
    assert!(stub.reverts.lock().unwrap().is_empty());
}

/// The case redo exists for: an undo, then the watcher wrote. HEAD cannot be
/// rewound over the watcher's revision, so the undo is undone in place.
#[tokio::test]
async fn test_redo_reverts_the_undo_when_the_watcher_has_written_since() {
    let log = json!({
        "head": 4,
        "operations": [
            op(1, None, 1),
            op(2, Some(1), 2),
            revert_op(3, Some(2), 3, 2),
            op(4, Some(3), 4),
        ],
        "revisions": [rev(1, None), rev(2, None), rev(3, None), rev(4, Some("watcher"))],
    });
    let (url, stub) = spawn_stub(log).await;
    let (gui, daemon, ws) = setup(&url);

    undo::navigate(gui.clone(), daemon, ws.clone(), true, Default::default()).await.unwrap();

    assert!(stub.rollbacks.lock().unwrap().is_empty(), "HEAD must not move");
    assert_eq!(
        stub.reverts.lock().unwrap().as_slice(),
        [json!({"target": {"rev_id": 3}, "with_dependents": false})]
    );
    assert_ne!(gui.get_var(&ws, "metarecords:dirty").unwrap(), Value::Null);
}

/// A plain change on top of an undo is not something to redo.
#[tokio::test]
async fn test_redo_does_nothing_when_your_newest_change_is_not_an_undo() {
    let log = json!({
        "head": 4,
        "operations": [
            op(1, None, 1),
            op(2, Some(1), 2),
            revert_op(3, Some(2), 3, 2),
            op(4, Some(3), 4),
        ],
        "revisions": [rev(1, None), rev(2, None), rev(3, None), rev(4, None)],
    });
    let (url, stub) = spawn_stub(log).await;
    let (gui, daemon, ws) = setup(&url);

    undo::navigate(gui.clone(), daemon, ws, true, Default::default()).await.unwrap();

    assert!(stub.rollbacks.lock().unwrap().is_empty());
    assert!(stub.reverts.lock().unwrap().is_empty());
}

#[tokio::test]
async fn test_no_active_repo_is_an_error() {
    let (url, _stub) = spawn_stub(linear_log(json!(4))).await;
    let gui = Arc::new(GuiState::new(Arc::new(RecordingNotifier::new())));
    let ws = gui.create_workspace(None);
    let daemon = Arc::new(DaemonProxy::new(url));

    assert!(undo::navigate(gui, daemon, ws, false, Default::default()).await.is_err());
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

/// The user's change (rev 2), then one of the watcher's (rev 3): an undo has to
/// revert rev 2 in place.
fn log_with_the_watcher_on_top() -> Value {
    json!({
        "head": 3,
        "operations": [op(1, None, 1), op(2, Some(1), 2), op(3, Some(2), 3)],
        "revisions": [rev(1, None), rev(2, None), rev(3, Some("watcher"))],
    })
}

#[tokio::test]
async fn test_undo_reports_the_rolled_back_revision() {
    let (url, _stub) = spawn_stub(linear_log(json!(4))).await;
    let (gui, daemon, ws, notifier) = setup_watched(&url);

    undo::navigate(gui, daemon, ws, false, timeouts()).await.unwrap();

    let (text, kind, timeout) = last_status(&notifier);
    assert!(text.starts_with("Undo: revision 2 rolled back ("), "status: {text}");
    assert_eq!((kind.as_str(), timeout), ("info", json!(222)));
}

#[tokio::test]
async fn test_undo_with_nothing_to_undo_says_so() {
    let log = json!({"head": null, "operations": [], "revisions": []});
    let (url, stub) = spawn_stub(log).await;
    let (gui, daemon, ws, notifier) = setup_watched(&url);

    undo::navigate(gui, daemon, ws, false, timeouts()).await.unwrap();

    assert_eq!(last_status(&notifier), ("Nothing to undo.".into(), "info".into(), json!(222)));
    assert!(stub.rollbacks.lock().unwrap().is_empty());
}

#[tokio::test]
async fn test_redo_with_nothing_to_redo_says_so() {
    let (url, _stub) = spawn_stub(linear_log(json!(4))).await;
    let (gui, daemon, ws, notifier) = setup_watched(&url);

    undo::navigate(gui, daemon, ws, true, timeouts()).await.unwrap();

    assert_eq!(last_status(&notifier).0, "Nothing to redo.");
}

#[tokio::test]
async fn test_redo_reports_the_revision_it_re_applied() {
    let (url, _stub) = spawn_stub(linear_log(json!(2))).await;
    let (gui, daemon, ws, notifier) = setup_watched(&url);

    undo::navigate(gui, daemon, ws, true, timeouts()).await.unwrap();

    let text = last_status(&notifier).0;
    assert!(text.starts_with("Redo: revision 2 re-applied ("), "status: {text}");
}

#[tokio::test]
async fn test_a_refused_rollback_is_reported_and_fails_the_command() {
    let (url, stub) = spawn_stub(linear_log(json!(4))).await;
    *stub.rollback_refused.lock().unwrap() = true;
    let (gui, daemon, ws, notifier) = setup_watched(&url);

    let error =
        undo::navigate(gui.clone(), daemon, ws.clone(), false, timeouts()).await.unwrap_err();

    assert!(error.contains("locked"), "error: {error}");
    assert_eq!(last_status(&notifier), (error, "error".into(), json!(111)));
    assert_eq!(gui.get_var(&ws, "metarecords:dirty").unwrap(), Value::Null, "nothing changed");
}

#[tokio::test]
async fn test_undo_under_the_watcher_reverts_and_reports_the_new_revision() {
    let (url, stub) = spawn_stub(log_with_the_watcher_on_top()).await;
    let (gui, daemon, ws, notifier) = setup_watched(&url);

    undo::navigate(gui, daemon, ws, false, timeouts()).await.unwrap();

    assert!(stub.rollbacks.lock().unwrap().is_empty(), "HEAD must not move");
    assert_eq!(stub.reverts.lock().unwrap().len(), 1);
    assert_eq!(
        last_status(&notifier),
        (
            "Revision 2 reverted as revision 42 (1 operations) — the daemon's own revisions \
             were left in place."
                .into(),
            "info".into(),
            json!(222)
        )
    );
}

/// A later change overwrote what the undone revision wrote: the GUI does not
/// pull that change in behind the user's back. It says what blocks and where
/// the way through is, and the command itself does not fail.
#[tokio::test]
async fn test_a_blocked_undo_names_the_blocker_and_the_way_through() {
    let (url, stub) = spawn_stub(log_with_the_watcher_on_top()).await;
    *stub.revert_plan.lock().unwrap() = json!({
        "revertable": false, "requires_lock": false, "operations": [{"id": 2}],
        "blocked": [{"op_id": 2, "rev_id": 7}], "dependents": [],
    });
    let (gui, daemon, ws, notifier) = setup_watched(&url);

    undo::navigate(gui.clone(), daemon, ws.clone(), false, timeouts()).await.unwrap();

    assert!(stub.reverts.lock().unwrap().is_empty(), "nothing is forced");
    assert_eq!(
        last_status(&notifier),
        (
            "Cannot take revision 2 back: a later change by revision 7 overwrote what it wrote. \
             Open the log panel and use log:revert with-dependents to undo both."
                .into(),
            "error".into(),
            json!(111)
        )
    );
    assert_eq!(gui.get_var(&ws, "metarecords:dirty").unwrap(), Value::Null);
}
