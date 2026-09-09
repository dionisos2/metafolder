//! The slow-operation log (spec-slow-log.org): what the daemon records when an
//! operation crosses the threshold, and the endpoint that serves it back.
//!
//! Slowness is produced deliberately rather than waited for: a thread holds the
//! repository's connection lock while the request runs, which is the real shape
//! of the problem the log exists for (a query that is fine, behind something
//! that is not).

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use metafolder_core::slowlog;
use metafolder_daemon::daemon_config::DaemonSettings;
use metafolder_daemon::routes;
use metafolder_daemon::state::AppState;
use serde_json::{json, Value};
use tower::util::ServiceExt;

mod common;
use common::TempDir;

/// How long the lock holder keeps the repository busy. Long enough that the
/// measured wait cannot be confused with scheduling noise.
const HELD: Duration = Duration::from_millis(200);

struct Fixture {
    app: Router,
    state: Arc<AppState>,
    repo: String,
    _root: TempDir,
}

async fn fixture(prefix: &str, threshold_ms: u64) -> Fixture {
    let settings =
        DaemonSettings { slow_operation_threshold_ms: threshold_ms, ..Default::default() };
    let state = Arc::new(AppState::new().with_settings(settings));
    let app = routes::build(state.clone());
    let root = TempDir::new(&format!("slowlog_{prefix}"));
    let (status, body) =
        request(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()})), &[])
            .await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    let repo = body["repo_uuid"].as_str().unwrap().to_string();
    Fixture { app, state, repo, _root: root }
}

async fn request(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = match body {
        Some(v) => builder
            .header("content-type", "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

impl Fixture {
    /// Runs one request while another thread holds the repository's connection,
    /// so the operation is reliably slow.
    async fn while_busy(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> (StatusCode, Value) {
        let repo_state = self.state.ready_repo(self.repo.parse().unwrap()).unwrap();
        let holder = std::thread::spawn(move || {
            let _busy = repo_state.conn.lock().unwrap();
            std::thread::sleep(HELD);
        });
        // Let the holder take the lock before the request reaches for it.
        std::thread::sleep(Duration::from_millis(20));
        let out = request(&self.app, method, uri, body, headers).await;
        holder.join().unwrap();
        out
    }

    fn entries(&self) -> Vec<slowlog::Entry> {
        let repo_state = self.state.ready_repo(self.repo.parse().unwrap()).unwrap();
        slowlog::read(&slowlog::slow_dir(&repo_state.internal_dir()), 100, None).0
    }
}

fn phase<'e>(entry: &'e slowlog::Entry, name: &str) -> Option<&'e slowlog::Phase> {
    entry.phases.iter().find(|p| p.name == name)
}

fn context<'e>(entry: &'e slowlog::Entry, key: &str) -> Option<&'e str> {
    entry.context.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

#[tokio::test]
async fn test_a_query_that_waited_for_the_database_says_so() {
    // The distinction the log exists to make: this query was not expensive,
    // it was behind something else holding the repository.
    let f = fixture("query_wait", 1).await;
    let (status, _) = f
        .while_busy(
            "POST",
            &format!("/repos/{}/query", f.repo),
            Some(json!({"query": {"type": "is_present", "field": "mfr_path"}})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let entries = f.entries();
    assert_eq!(entries.len(), 1, "one slow operation, one entry: {entries:?}");
    let entry = &entries[0];
    assert_eq!(entry.op, "POST /repos/:repo/query", "the route, not the concrete URL");
    assert_eq!(entry.source, "daemon");
    let wait = phase(entry, "wait:conn").expect("the wait must be named: {entry:?}");
    assert!(wait.ms >= 100, "the wait is where the time went: {wait:?}");
    assert!(entry.ms >= wait.ms, "the total covers the wait");
    assert!(context(entry, "query").is_some(), "the query is what a reader asks for next");
}

#[tokio::test]
async fn test_a_quick_operation_is_not_logged() {
    // The log's value is that everything in it is worth reading.
    let f = fixture("quick", slowlog::DEFAULT_THRESHOLD_MS).await;
    let (status, _) = request(
        &f.app,
        "POST",
        &format!("/repos/{}/query", f.repo),
        Some(json!({"query": {"type": "is_present", "field": "mfr_path"}})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(f.entries().is_empty());
}

#[tokio::test]
async fn test_logging_can_be_turned_off() {
    let f = fixture("off", 0).await;
    let (status, _) = f
        .while_busy(
            "POST",
            &format!("/repos/{}/query", f.repo),
            Some(json!({"query": {"type": "is_present", "field": "mfr_path"}})),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(f.entries().is_empty(), "a threshold of 0 records nothing");
}

#[tokio::test]
async fn test_the_client_says_what_the_user_asked_for() {
    // The daemon receives the query IR; only the client knows the text the user
    // typed, and the pair of entries is what lines the two logs up.
    let f = fixture("headers", 1).await;
    let (status, _) = f
        .while_busy(
            "POST",
            &format!("/repos/{}/query", f.repo),
            Some(json!({"query": {"type": "is_present", "field": "mfr_path"}})),
            &[("x-metafolder-op-id", "op-42"), ("x-metafolder-context", "mfr_path ->* \"/2024\"")],
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let entries = f.entries();
    assert_eq!(entries[0].op_id.as_deref(), Some("op-42"));
    assert_eq!(context(&entries[0], "client"), Some("mfr_path ->* \"/2024\""));
}

#[tokio::test]
async fn test_a_field_write_reports_the_phases_of_a_write() {
    // A write's time can go somewhere a query's never does — the commit's
    // fsync, the tree-cache settle — so those are named too.
    let f = fixture("write", 1).await;
    let (status, body) = request(
        &f.app,
        "POST",
        &format!("/repos/{}/metarecords", f.repo),
        Some(json!({"fields": [{"name": "title", "value": {"type": "string", "value": "x"}}]})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, _) = f
        .while_busy(
            "POST",
            &format!("/repos/{}/query/fields/set", f.repo),
            Some(json!({
                "query": {"type": "is_present", "field": "title"},
                "name": "rating",
                "value": {"type": "int", "value": 5}
            })),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let entry = f
        .entries()
        .into_iter()
        .find(|e| e.op.ends_with("/query/fields/set"))
        .expect("the write must be logged");
    assert!(phase(&entry, "wait:conn").is_some(), "{entry:?}");
    assert!(phase(&entry, "commit").is_some(), "the WAL fsync lands here: {entry:?}");
    assert_eq!(context(&entry, "updated"), Some("1"));
    assert_eq!(context(&entry, "field"), Some("rating"));
}

#[tokio::test]
async fn test_the_endpoint_serves_the_log_newest_first_and_can_empty_it() {
    let f = fixture("endpoint", 1).await;
    let repo_state = f.state.ready_repo(f.repo.parse().unwrap()).unwrap();
    let dir = slowlog::slow_dir(&repo_state.internal_dir());
    // Both sources: the daemon reads the GUI's file too, it just never writes it.
    slowlog::Sink::new(&dir, "daemon").append(&slowlog::Entry::new("daemon", "old", 1000, 3000));
    slowlog::Sink::new(&dir, "gui").append(&slowlog::Entry::new("gui", "new", 2000, 5000));

    let (status, body) =
        request(&f.app, "GET", &format!("/repos/{}/slow", f.repo), None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    let ops: Vec<&str> =
        body["entries"].as_array().unwrap().iter().map(|e| e["op"].as_str().unwrap()).collect();
    assert_eq!(ops, vec!["new", "old"]);
    assert_eq!(body["truncated"], json!(false));

    let (status, body) =
        request(&f.app, "GET", &format!("/repos/{}/slow?limit=1", f.repo), None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["entries"].as_array().unwrap().len(), 1);
    assert_eq!(body["truncated"], json!(true), "the limit cut it short");

    let (status, body) =
        request(&f.app, "GET", &format!("/repos/{}/slow?since_ms=2000", f.repo), None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["entries"].as_array().unwrap().len(), 1, "since keeps the boundary");

    let (status, body) =
        request(&f.app, "DELETE", &format!("/repos/{}/slow", f.repo), None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["cleared"], json!(2));
    let (_, body) = request(&f.app, "GET", &format!("/repos/{}/slow", f.repo), None, &[]).await;
    assert!(body["entries"].as_array().unwrap().is_empty());
}

/// The flush is not served over HTTP, so it names itself — and it is the
/// operation most likely to be *holding* the repository when a query complains
/// about waiting for one.
#[test]
fn test_a_slow_watcher_flush_names_itself() {
    use metafolder_core::metarecord::Value;
    use metafolder_daemon::executor::{self, FsEvent};
    use metafolder_daemon::log::Writer;
    use metafolder_daemon::state::RepoState;
    use metafolder_daemon::{db, repo};

    let root = TempDir::new("slowlog_flush");
    let opened = repo::init_repository(&root, None, None, false).unwrap();
    let settings = DaemonSettings { slow_operation_threshold_ms: 1, ..Default::default() };
    let repo_state = Arc::new(RepoState::from_opened_with(opened, &settings));
    {
        let mut conn = repo_state.conn.lock().unwrap();
        let root_uuid = db::find_tree_child(&conn, "mfr_path", None, "").unwrap().unwrap();
        let mut w = Writer::begin(&mut conn, None).unwrap();
        w.set_field(root_uuid, "mf_watch", Value::Bool(true)).unwrap();
        w.commit().unwrap();
    }
    std::fs::write(root.join("a.txt"), b"hello").unwrap();
    executor::enqueue(&repo_state, FsEvent::Create("/a.txt".into()), None);

    let busy = repo_state.clone();
    let holder = std::thread::spawn(move || {
        let _held = busy.conn.lock().unwrap();
        std::thread::sleep(HELD);
    });
    std::thread::sleep(Duration::from_millis(20));
    executor::flush_pending(&repo_state).unwrap();
    holder.join().unwrap();

    let entries = slowlog::read(&slowlog::slow_dir(&repo_state.internal_dir()), 100, None).0;
    let entry = entries
        .iter()
        .find(|e| e.op == "watcher.flush")
        .unwrap_or_else(|| panic!("the flush must be logged: {entries:?}"));
    assert!(phase(entry, "wait:conn").is_some_and(|p| p.ms >= 100), "{entry:?}");
    assert!(phase(entry, "watcher.apply").is_some(), "{entry:?}");
    assert_eq!(context(entry, "events"), Some("1"));
}
