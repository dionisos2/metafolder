//! A directory subtree that leaves the repository must give its inotify watches
//! back.
//!
//! This is its own test binary on purpose: it asserts on the *process-wide*
//! number of inotify watches, which every other watcher in the same binary
//! would perturb. Cargo runs each integration test in its own process, so
//! alone here the count belongs to this repository's watcher only.

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use metafolder_daemon::routes;
use metafolder_daemon::state::AppState;
use serde_json::{json, Value};
use tower::util::ServiceExt;

mod common;
use common::TempDir;

async fn request(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let builder = Request::builder().method(method).uri(uri);
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
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

/// Every tracked path in the repository, repo-root-relative (the root is `""`).
async fn tracked_paths(app: &Router, repo: &str) -> Vec<String> {
    let (status, body) = request(
        app,
        "POST",
        &format!("/repos/{repo}/query/fields/resolve-tree"),
        Some(json!({
            "query": {"type": "is_present", "field": "mfr_path"},
            "field": "mfr_path",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "resolve-tree failed: {body}");
    let mut out: Vec<String> = body
        .as_object()
        .expect("a uuid → paths map")
        .values()
        .filter_map(|paths| paths.as_array())
        .flatten()
        .filter_map(|p| p.as_str())
        .map(|p| p.trim_start_matches('/').to_string())
        .collect();
    out.sort();
    out
}

async fn wait_for_paths(app: &Router, repo: &str, expected: &[&str]) {
    let mut want: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
    want.sort();
    let mut last = Vec::new();
    for _ in 0..100 {
        last = tracked_paths(app, repo).await;
        if last == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("tracked paths never became {want:?}; last seen {last:?}");
}

async fn watched_repo(prefix: &str) -> (Router, String, TempDir) {
    let app = routes::build(std::sync::Arc::new(AppState::new()));
    let root = TempDir::new(prefix);
    let (status, body) =
        request(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    let repo = body["repo_uuid"].as_str().unwrap().to_string();

    let (_, roots) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": {"type": "is_present", "field": "mf_watch"}})),
    )
    .await;
    let root_uuid = roots[0].as_str().unwrap().to_string();
    let (status, body) = request(
        &app,
        "PUT",
        &format!("/repos/{repo}/metarecords/{root_uuid}/fields/mf_watch"),
        Some(json!({"value": {"type": "bool", "value": true}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "enabling mf_watch failed: {body}");
    (app, repo, root)
}

/// How many inotify watches this process holds, across every inotify instance:
/// the kernel exposes them as `inotify wd:` lines in the fd's `fdinfo`. This is
/// the only place the leak is visible — the daemon's own bookkeeping forgets
/// the directory either way.
fn kernel_watches() -> usize {
    let mut total = 0;
    for entry in std::fs::read_dir("/proc/self/fd").expect("read /proc/self/fd") {
        let entry = entry.expect("an fd entry");
        match std::fs::read_link(entry.path()) {
            Ok(target) if target.to_string_lossy() == "anon_inode:inotify" => {}
            _ => continue,
        }
        let info = std::path::Path::new("/proc/self/fdinfo").join(entry.file_name());
        let Ok(text) = std::fs::read_to_string(info) else {
            continue; // The fd was closed between the two reads.
        };
        total += text.lines().filter(|l| l.starts_with("inotify wd:")).count();
    }
    total
}

/// Moving a directory *out* of the repository releases the watches on it and on
/// every directory below it.
///
/// Nothing in this crate unwatches them: the watcher's `maintain_watches` only
/// *forgets* a departure, because notify's inotify backend removes the watch
/// (recursively) when it sees the `MOVED_FROM`. That is the whole reason the
/// leak this test guards against does not exist — and it is a property of the
/// dependency, not of the kernel, which keeps an inotify watch alive across a
/// rename. A notify upgrade that dropped it would leak one watch per departed
/// directory for the lifetime of the repository, silently, while events in the
/// directory's new home outside the repository kept arriving under its old
/// path.
#[tokio::test(flavor = "multi_thread")]
async fn test_directory_moved_out_of_the_repository_releases_its_watches() {
    let (app, repo, root) = watched_repo("watchleak").await;
    let outside = TempDir::new("watchleak-outside");
    let baseline = kernel_watches();

    std::fs::create_dir_all(root.join("A/B/C")).unwrap();
    std::fs::write(root.join("A/B/C/one.txt"), b"1").unwrap();
    tokio::time::timeout(
        Duration::from_secs(20),
        wait_for_paths(&app, &repo, &["", "A", "A/B", "A/B/C", "A/B/C/one.txt"]),
    )
    .await
    .expect("the new directory and its file are tracked");

    let deep = kernel_watches();
    assert_eq!(deep, baseline + 3, "A, A/B and A/B/C each take one watch");

    std::fs::rename(root.join("A"), outside.join("A")).unwrap();
    tokio::time::timeout(Duration::from_secs(20), wait_for_paths(&app, &repo, &[""]))
        .await
        .expect("the departed subtree is orphaned");

    let mut after = deep;
    for _ in 0..100 {
        after = kernel_watches();
        if after == baseline {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the departed subtree kept its watches: {baseline} → {deep} → {after}");
}
