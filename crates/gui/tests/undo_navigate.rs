//! `log:undo` / `log:redo` (spec-gui "Event log"): shell builtins
//! navigating the active repo's event log through the daemon rollback
//! API. Tests run a stub daemon on an ephemeral port.

use axum::extract::State;
use axum::routing::{get, post};
use axum::Json;
use metafolder_gui::daemon_proxy::DaemonProxy;
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
}

async fn spawn_stub(log: Value) -> (String, Stub) {
    let stub = Stub {
        log,
        rollbacks: Arc::new(Mutex::new(Vec::new())),
    };
    let router = axum::Router::new()
        .route(
            "/repos/:repo/log",
            get(|State(stub): State<Stub>| async move { Json(stub.log.clone()) }),
        )
        .route(
            "/repos/:repo/rollback",
            post(|State(stub): State<Stub>, Json(body): Json<Value>| async move {
                stub.rollbacks.lock().unwrap().push(body);
                Json(json!({
                    "previous_head": 4,
                    "new_head": 2,
                    "operations_unapplied": 2,
                    "operations_applied": 0,
                }))
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

#[tokio::test]
async fn test_no_active_repo_is_an_error() {
    let (url, _stub) = spawn_stub(linear_log(json!(4))).await;
    let gui = Arc::new(GuiState::new(Arc::new(RecordingNotifier::new())));
    let ws = gui.create_workspace(None);
    let daemon = Arc::new(DaemonProxy::new(url));

    assert!(undo::navigate(gui, daemon, ws, false, Default::default()).await.is_err());
}
