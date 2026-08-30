//! The daemon HTTP proxy: panels and the shell reach the daemon through
//! the Rust backend (the WebView cannot, for CORS reasons). Tests run a
//! stub daemon on an ephemeral port.

use axum::extract::State;
use axum::routing::{any, get};
use axum::Json;
use metafolder_gui::daemon_proxy::DaemonProxy;
use metafolder_gui::events;
use metafolder_gui::notifier::RecordingNotifier;
use metafolder_gui::state::GuiState;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Recorded {
    calls: Arc<Mutex<Vec<(String, String, Value)>>>,
}

/// Stub daemon: metarecords every request; /health answers ok; /fail answers
/// a daemon-style error.
async fn spawn_stub() -> (String, Recorded) {
    let recorded = Recorded::default();
    let router = axum::Router::new()
        .route(
            "/health",
            get(|| async {
                Json(json!({"status": "ok", "api_version": metafolder_core::API_VERSION}))
            }),
        )
        .route(
            "/fail",
            any(|| async {
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    Json(json!({"error": "bad request"})),
                )
            }),
        )
        .fallback(any(
            |State(recorded): State<Recorded>, request: axum::extract::Request| async move {
                let method = request.method().to_string();
                let path = request
                    .uri()
                    .path_and_query()
                    .map(|p| p.to_string())
                    .unwrap_or_default();
                let bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                recorded.calls.lock().unwrap().push((method, path, body));
                Json(json!({"echo": true}))
            },
        ))
        .with_state(recorded.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (format!("http://127.0.0.1:{port}"), recorded)
}

fn gui_with_notifier() -> (Arc<RecordingNotifier>, Arc<GuiState>) {
    let notifier = Arc::new(RecordingNotifier::new());
    (notifier.clone(), Arc::new(GuiState::new(notifier)))
}

#[tokio::test]
async fn test_request_passthrough() {
    let (url, recorded) = spawn_stub().await;
    let proxy = DaemonProxy::new(url);

    let response = proxy
        .request(
            "POST",
            "/repos/abc/query?limit=10",
            Some(json!({"query": {"type": "and", "clauses": []}})),
        )
        .await
        .unwrap();

    assert_eq!(response.status, 200);
    assert_eq!(response.body, json!({"echo": true}));

    let calls = recorded.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    let (method, path, body) = &calls[0];
    assert_eq!(method, "POST");
    assert_eq!(path, "/repos/abc/query?limit=10");
    assert_eq!(body, &json!({"query": {"type": "and", "clauses": []}}));
}

#[tokio::test]
async fn test_daemon_errors_pass_through_with_status() {
    let (url, _) = spawn_stub().await;
    let proxy = DaemonProxy::new(url);

    // Daemon-level errors are not transport errors: the panel needs the
    // status code and the {"error": ...} body.
    let response = proxy.request("POST", "/fail", None).await.unwrap();
    assert_eq!(response.status, 400);
    assert_eq!(response.body, json!({"error": "bad request"}));
}

#[tokio::test]
async fn test_transport_error_is_err() {
    let proxy = DaemonProxy::new("http://127.0.0.1:1".into());
    assert!(proxy.request("GET", "/repos", None).await.is_err());
}

#[tokio::test]
async fn test_health_transitions_emit_events() {
    let (url, _) = spawn_stub().await;
    let (notifier, gui) = gui_with_notifier();
    let proxy = DaemonProxy::new(url.clone());

    // First check: connected.
    assert!(proxy.check_health(&gui).await);
    // Same state again: no duplicate event.
    assert!(proxy.check_health(&gui).await);
    // Unreachable daemon: disconnected event.
    proxy.set_url("http://127.0.0.1:1".into());
    assert!(!proxy.check_health(&gui).await);
    // Back to the live stub: connected event.
    proxy.set_url(url);
    assert!(proxy.check_health(&gui).await);

    let payloads = notifier.payloads(events::DAEMON_HEALTH_CHANGED);
    assert_eq!(
        payloads,
        vec![
            json!({
                "connected": true, "compatible": true,
                "daemon_api_version": metafolder_core::API_VERSION,
                "gui_api_version": metafolder_core::API_VERSION,
            }),
            json!({
                "connected": false, "compatible": false,
                "daemon_api_version": Value::Null,
                "gui_api_version": metafolder_core::API_VERSION,
            }),
            json!({
                "connected": true, "compatible": true,
                "daemon_api_version": metafolder_core::API_VERSION,
                "gui_api_version": metafolder_core::API_VERSION,
            }),
        ]
    );
}

/// A health-only stub reporting the given `/health` body (used to exercise the
/// version-compatibility verdict).
async fn spawn_health_stub(body: Value) -> String {
    let router = axum::Router::new()
        .route("/health", get(move || {
            let body = body.clone();
            async move { Json(body) }
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://127.0.0.1:{port}")
}

#[tokio::test]
async fn test_incompatible_daemon_flagged() {
    // A reachable daemon that reports a wire-protocol version we do not speak is
    // connected but not compatible, so the shell can warn (spec-gui) instead of
    // silently serving requests the two sides may disagree about.
    let url = spawn_health_stub(json!({
        "status": "ok",
        "api_version": metafolder_core::API_VERSION + 1,
    }))
    .await;
    let (notifier, gui) = gui_with_notifier();
    let proxy = DaemonProxy::new(url);

    // Still reachable (the probe succeeded), just incompatible.
    assert!(proxy.check_health(&gui).await);
    let payloads = notifier.payloads(events::DAEMON_HEALTH_CHANGED);
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0]["connected"], json!(true));
    assert_eq!(payloads[0]["compatible"], json!(false));
    assert_eq!(payloads[0]["daemon_api_version"], json!(metafolder_core::API_VERSION + 1));
    assert_eq!(payloads[0]["gui_api_version"], json!(metafolder_core::API_VERSION));
}

#[tokio::test]
async fn test_pre_versioning_daemon_is_incompatible() {
    // A daemon predating the api_version field: reachable but treated as
    // incompatible (we cannot vouch for its contract).
    let url = spawn_health_stub(json!({"status": "ok"})).await;
    let (notifier, gui) = gui_with_notifier();
    let proxy = DaemonProxy::new(url);

    assert!(proxy.check_health(&gui).await);
    let payloads = notifier.payloads(events::DAEMON_HEALTH_CHANGED);
    assert_eq!(payloads[0]["connected"], json!(true));
    assert_eq!(payloads[0]["compatible"], json!(false));
    assert_eq!(payloads[0]["daemon_api_version"], Value::Null);
}

/// Stub for the asynchronous reconcile contract (spec-tasks): POST reconcile
/// answers 202 + task id; GET the task answers a finished task with a result.
async fn spawn_reconcile_stub() -> String {
    let router = axum::Router::new()
        .route(
            "/repos/abc123/reconcile",
            axum::routing::post(|| async {
                (axum::http::StatusCode::ACCEPTED, Json(json!({"task_id": "t1"})))
            }),
        )
        .route(
            "/repos/abc123/tasks/t1",
            get(|| async {
                Json(json!({
                    "id": "t1", "repo_uuid": "abc123", "kind": "reconcile",
                    "status": "done", "phase": "mime", "done": 2, "total": 2,
                    "result": {"created": 2, "moved": 0, "candidates": []},
                    "error": null,
                }))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://127.0.0.1:{port}")
}

#[tokio::test]
async fn test_reconcile_run_posts_status_and_logs() {
    let url = spawn_reconcile_stub().await;
    let (notifier, gui) = gui_with_notifier();
    let proxy = Arc::new(DaemonProxy::new(url));

    // No active repo: refused.
    assert!(
        metafolder_gui::reconcile::run(gui.clone(), proxy.clone(), "ws-1".into(), Default::default())
            .await
            .is_err()
    );

    let ws = gui.workspace_new(Some("abc123".into()));
    notifier.clear();
    metafolder_gui::reconcile::run(gui.clone(), proxy, ws.clone(), Default::default())
        .await
        .unwrap();

    // The task is done on the first poll: initial busy status, then the summary.
    let statuses = notifier.payloads(events::STATUS_MESSAGE);
    assert_eq!(statuses.len(), 2);
    assert_eq!(statuses[0]["kind"], "busy");
    assert!(statuses[1]["text"]
        .as_str()
        .unwrap()
        .starts_with("Reconcile:"));
    // Message log: initial "Reconciling…" + summary + detail (progress polls
    // do not append to the log).
    assert_eq!(gui.messages(&ws).unwrap().len(), 3);
}

#[tokio::test]
async fn test_set_url_is_visible() {
    let proxy = DaemonProxy::new("http://127.0.0.1:7523".into());
    assert_eq!(proxy.base_url(), "http://127.0.0.1:7523");
    proxy.set_url("http://127.0.0.1:9999".into());
    assert_eq!(proxy.base_url(), "http://127.0.0.1:9999");
}
