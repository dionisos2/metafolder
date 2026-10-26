//! `mf:duplicate scan` (doc "duplicates panel"): the shell builtin that starts a
//! duplicate scan on the active repo, polls its task and reports the outcome in
//! the status bar and the message log. Tests run a stub daemon on an ephemeral
//! port.

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::Json;
use metafolder_gui::daemon_proxy::DaemonProxy;
use metafolder_gui::duplicates;
use metafolder_gui::events;
use metafolder_gui::notifier::RecordingNotifier;
use metafolder_gui::reconcile::Timings;
use metafolder_gui::state::GuiState;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone)]
struct Stub {
    /// Status and body served by POST /repos/:repo/duplicates/scan.
    start: (u16, Value),
    /// Bodies served by successive GET /repos/:repo/tasks/:id; the last repeats.
    polls: Arc<Mutex<Vec<Value>>>,
    /// Bodies of the received scan requests.
    scans: Arc<Mutex<Vec<Value>>>,
    /// How many times the task was polled.
    poll_count: Arc<Mutex<usize>>,
}

async fn spawn_stub(start: (u16, Value), polls: Vec<Value>) -> (String, Stub) {
    let stub = Stub {
        start,
        polls: Arc::new(Mutex::new(polls)),
        scans: Arc::new(Mutex::new(Vec::new())),
        poll_count: Arc::new(Mutex::new(0)),
    };
    let router = axum::Router::new()
        .route(
            "/repos/{repo}/duplicates/scan",
            post(|State(stub): State<Stub>, Json(body): Json<Value>| async move {
                stub.scans.lock().unwrap().push(body);
                let (status, body) = stub.start.clone();
                (StatusCode::from_u16(status).unwrap(), Json(body))
            }),
        )
        .route(
            "/repos/{repo}/tasks/{id}",
            get(|State(stub): State<Stub>| async move {
                *stub.poll_count.lock().unwrap() += 1;
                let mut polls = stub.polls.lock().unwrap();
                let body = if polls.len() > 1 { polls.remove(0) } else { polls[0].clone() };
                Json(body)
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

struct Ctx {
    gui: Arc<GuiState>,
    daemon: Arc<DaemonProxy>,
    notifier: Arc<RecordingNotifier>,
    ws: String,
}

fn setup(url: &str) -> Ctx {
    let notifier = Arc::new(RecordingNotifier::new());
    let gui = Arc::new(GuiState::new(notifier.clone()));
    let ws = gui.create_workspace(Some("cafe".into()));
    Ctx { gui, daemon: Arc::new(DaemonProxy::new(url.to_string())), notifier, ws }
}

fn timings() -> Timings {
    Timings { poll: Duration::from_millis(1), error_ms: 111, done_ms: 222 }
}

impl Ctx {
    async fn run(&self, min_size: Option<u64>, rehash: bool) -> Result<(), String> {
        duplicates::run(
            self.gui.clone(),
            self.daemon.clone(),
            self.ws.clone(),
            min_size,
            rehash,
            timings(),
        )
        .await
    }

    /// `(text, kind, timeout_ms)` of every status posted, in order.
    fn statuses(&self) -> Vec<(String, String, Value)> {
        self.notifier
            .payloads(events::STATUS_MESSAGE)
            .into_iter()
            .map(|p| {
                (
                    p["text"].as_str().unwrap().to_string(),
                    p["kind"].as_str().unwrap().to_string(),
                    p["timeout_ms"].clone(),
                )
            })
            .collect()
    }

    fn last_status(&self) -> (String, String, Value) {
        self.statuses().pop().expect("a status was posted")
    }
}

fn accepted() -> (u16, Value) {
    (202, json!({"task_id": "t1"}))
}

#[tokio::test]
async fn test_a_finished_scan_reports_its_summary_and_its_detail() {
    let result = json!({"groups": 2, "files": 5, "reclaimable": 2048});
    let (url, stub) = spawn_stub(
        accepted(),
        vec![json!({"status": "running"}), json!({"status": "done", "result": result})],
    )
    .await;
    let ctx = setup(&url);

    ctx.run(None, false).await.unwrap();

    assert_eq!(*stub.poll_count.lock().unwrap(), 2, "it polls until the task ends");
    let statuses = ctx.statuses();
    assert_eq!(statuses[0].1, "busy", "a busy status while the scan runs");
    assert_eq!(
        ctx.last_status(),
        ("Duplicates: 2 groups, 5 files, 2.0K reclaimable.".into(), "info".into(), json!(222))
    );
    let messages = ctx.gui.messages(&ctx.ws).unwrap();
    let detail = &messages.last().unwrap().text;
    assert_eq!(
        serde_json::from_str::<Value>(detail).unwrap(),
        result,
        "the full result lands in the message log"
    );
}

#[tokio::test]
async fn test_the_options_are_forwarded_and_min_size_only_when_given() {
    let done = json!({"status": "done", "result": {}});
    let (url, stub) = spawn_stub(accepted(), vec![done]).await;
    let ctx = setup(&url);

    ctx.run(Some(4096), true).await.unwrap();
    ctx.run(None, false).await.unwrap();

    assert_eq!(
        stub.scans.lock().unwrap().as_slice(),
        [json!({"rehash": true, "min_size": 4096}), json!({"rehash": false})]
    );
}

#[tokio::test]
async fn test_a_daemon_without_the_endpoint_gets_a_clear_message() {
    let (url, _stub) = spawn_stub((404, json!({"error": "not found"})), vec![json!({})]).await;
    let ctx = setup(&url);

    let error = ctx.run(None, false).await.unwrap_err();

    assert_eq!(error, "this daemon does not support duplicate detection");
    assert_eq!(ctx.last_status(), (error, "error".into(), json!(111)));
}

#[tokio::test]
async fn test_a_refused_scan_reports_the_daemon_error() {
    let (url, stub) =
        spawn_stub((400, json!({"error": "min_size must be positive"})), vec![json!({})]).await;
    let ctx = setup(&url);

    let error = ctx.run(None, false).await.unwrap_err();

    assert_eq!(error, "min_size must be positive");
    assert_eq!(ctx.last_status().1, "error");
    assert_eq!(*stub.poll_count.lock().unwrap(), 0, "no task to poll");
}

#[tokio::test]
async fn test_a_refusal_without_an_error_body_names_the_status() {
    let (url, _stub) = spawn_stub((500, json!({})), vec![json!({})]).await;
    let ctx = setup(&url);

    let error = ctx.run(None, false).await.unwrap_err();

    assert_eq!(error, "duplicate scan failed (500)");
}

#[tokio::test]
async fn test_an_answer_without_a_task_id_is_an_error() {
    let (url, _stub) = spawn_stub((202, json!({})), vec![json!({})]).await;
    let ctx = setup(&url);

    let error = ctx.run(None, false).await.unwrap_err();

    assert!(error.contains("missing task_id"), "error: {error}");
}

#[tokio::test]
async fn test_a_failed_task_reports_its_error() {
    let (url, _stub) =
        spawn_stub(accepted(), vec![json!({"status": "failed", "error": "disk on fire"})]).await;
    let ctx = setup(&url);

    let error = ctx.run(None, false).await.unwrap_err();

    assert_eq!(error, "disk on fire");
    assert_eq!(ctx.last_status(), (error, "error".into(), json!(111)));
}

#[tokio::test]
async fn test_a_failed_task_without_an_error_says_the_scan_failed() {
    let (url, _stub) = spawn_stub(accepted(), vec![json!({"status": "failed"})]).await;
    let ctx = setup(&url);

    assert_eq!(ctx.run(None, false).await.unwrap_err(), "duplicate scan failed");
}

#[tokio::test]
async fn test_a_cancelled_scan_is_not_a_failure() {
    let (url, _stub) = spawn_stub(accepted(), vec![json!({"status": "cancelled"})]).await;
    let ctx = setup(&url);

    ctx.run(None, false).await.unwrap();

    assert_eq!(
        ctx.last_status(),
        ("Duplicate scan cancelled; what it found is kept.".into(), "info".into(), json!(222))
    );
}

#[tokio::test]
async fn test_an_unreachable_daemon_is_reported_in_the_status_bar() {
    // A port nothing listens on: bind one, then let it go.
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let ctx = setup(&format!("http://127.0.0.1:{port}"));

    let error = ctx.run(None, false).await.unwrap_err();

    assert_eq!(ctx.last_status(), (error, "error".into(), json!(111)));
}

#[tokio::test]
async fn test_without_an_active_repo_nothing_is_asked() {
    let (url, stub) = spawn_stub(accepted(), vec![json!({})]).await;
    let ctx = setup(&url);
    let ws = ctx.gui.create_workspace(None);

    let error = duplicates::run(ctx.gui.clone(), ctx.daemon.clone(), ws, None, false, timings())
        .await
        .unwrap_err();

    assert!(error.contains("repository"), "error: {error}");
    assert!(stub.scans.lock().unwrap().is_empty());
}
