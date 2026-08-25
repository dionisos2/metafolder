//! The first minutes of a repository, against a live watcher — and an oracle
//! for them.
//!
//! Every other watcher test states the expected outcome by hand, so it only
//! checks what its author thought to predict. Here each step is followed by a
//! *full reconcile*, which recomputes the tracking state from the filesystem
//! alone: after a settled watcher, a reconcile must find nothing to do (nothing
//! created, nothing moved) and must leave the tracked paths untouched. The
//! watcher and the reconcile are independent implementations of the same
//! question — "which files exist and where" — so any disagreement between them
//! is a bug in one of the two, whichever way it falls.
//!
//! The sequence is deliberately ordinary: create a repo on a folder that
//! already holds files (including the junk the default ignore preset excludes),
//! turn tracking on, then do what a user does in a file manager — add, rename,
//! move, nest, delete.

use std::path::PathBuf;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use metafolder_daemon::routes;
use metafolder_daemon::state::AppState;
use serde_json::{json, Value};
use tower::util::ServiceExt;
use uuid::Uuid;

async fn request(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
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

/// The shipped `default` ignore preset, expanded — what `mf repo init` and the
/// GUI's create-repo flow write to a new root (the daemon writes none itself).
fn default_ignore_patterns() -> Vec<String> {
    let toml = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../core/default-config/ignore-presets.toml"
    ))
    .expect("the shipped ignore presets");
    let presets = metafolder_core::ignore_presets::Presets::parse(&toml).expect("valid presets");
    presets.expand(&["default"]).expect("the default preset")
}

/// A repository as a user gets one: created on an existing folder, with the
/// default ignore set applied client-side, and tracking turned on.
async fn journey_repo(prefix: &str) -> (Router, String, PathBuf) {
    let app = routes::build(std::sync::Arc::new(AppState::new()));
    let root: PathBuf =
        std::env::temp_dir().join(format!("metafolder_journey_{prefix}_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();

    let (status, body) =
        request(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    let repo = body["repo_uuid"].as_str().unwrap().to_string();
    let root_uuid = root_metarecord(&app, &repo).await;

    // The ignore set, then tracking — the order the create-repo flow uses.
    let values: Vec<Value> =
        default_ignore_patterns().iter().map(|p| json!({"type": "string", "value": p})).collect();
    let (status, body) = request(
        &app,
        "PUT",
        &format!("/repos/{repo}/metarecords/{root_uuid}/fields/mf_ignore"),
        Some(json!({"values": values})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "applying the default ignores failed: {body}");

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

async fn root_metarecord(app: &Router, repo: &str) -> String {
    let (_, roots) = request(
        app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": {"type": "is_present", "field": "mf_watch"}})),
    )
    .await;
    roots[0].as_str().expect("the filesystem root metarecord").to_string()
}

/// Every tracked path, repo-root-relative without the leading slash (the root
/// itself is `""`), sorted.
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

/// Waits for the watcher to settle on `expected` (its 500 ms quiet period plus
/// the flush), then reports what it settled on.
async fn settle_on(app: &Router, repo: &str, expected: &[&str]) -> Vec<String> {
    let mut want: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
    want.sort();
    let mut last = Vec::new();
    for _ in 0..100 {
        last = tracked_paths(app, repo).await;
        if last == want {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the watcher never settled on {want:?}; last seen {last:?}");
}

/// The oracle: a full reconcile after a settled watcher must be a no-op.
/// `label` names the step, so a failure says which operation the two disagree
/// about.
async fn reconcile_agrees(app: &Router, repo: &str, label: &str) {
    let before = tracked_paths(app, repo).await;

    let (status, body) = request(app, "POST", &format!("/repos/{repo}/reconcile"), None).await;
    assert_eq!(status, StatusCode::ACCEPTED, "[{label}] reconcile start failed: {body}");
    let task_id = body["task_id"].as_str().unwrap().to_string();
    let mut task = Value::Null;
    for _ in 0..400 {
        let (_, body) =
            request(app, "GET", &format!("/repos/{repo}/tasks/{task_id}"), None).await;
        if body["status"] == "done" || body["status"] == "failed" {
            task = body;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(task["status"], "done", "[{label}] reconcile did not finish: {task}");
    let result = &task["result"];
    assert_eq!(
        result["created"], 0,
        "[{label}] the watcher missed {} file(s) a reconcile had to create — tracked: {before:?}",
        result["created"],
    );
    assert_eq!(
        result["moved"], 0,
        "[{label}] the watcher left {} file(s) at a stale path — tracked: {before:?}",
        result["moved"],
    );

    let after = tracked_paths(app, repo).await;
    assert_eq!(after, before, "[{label}] the reconcile changed the tracked paths");
}

/// Metarecords whose `mfr_path` is `Nothing` — orphans. Deleting a file leaves
/// one behind by design; nothing else may.
async fn orphan_count(app: &Router, repo: &str) -> usize {
    let (status, body) = request(
        app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": {"type": "is_absent", "field": "mfr_path"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "orphan query failed: {body}");
    body.as_array().map(|a| a.len()).unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread")]
async fn test_first_minutes_of_a_repository() {
    // A folder that already holds content when the repository is created —
    // including the build junk and VCS metadata the default preset ignores.
    let app_root = {
        let (app, repo, root) = journey_repo("firstuse").await;
        std::fs::create_dir_all(root.join("photos/2024")).unwrap();
        std::fs::write(root.join("photos/2024/a.jpg"), b"a").unwrap();
        std::fs::write(root.join("notes.txt"), b"hello").unwrap();
        std::fs::create_dir_all(root.join(".git/objects")).unwrap();
        std::fs::write(root.join(".git/HEAD"), b"ref: refs/heads/main").unwrap();
        std::fs::create_dir_all(root.join("node_modules/left-pad")).unwrap();
        std::fs::write(root.join("node_modules/left-pad/index.js"), b"//").unwrap();

        // Everything eligible is picked up; the ignored subtrees are not.
        settle_on(&app, &repo, &["", "photos", "photos/2024", "photos/2024/a.jpg", "notes.txt"])
            .await;
        reconcile_agrees(&app, &repo, "initial content").await;
        assert_eq!(orphan_count(&app, &repo).await, 0, "initial content: no orphan expected");
        (app, repo, root)
    };
    let (app, repo, root) = app_root;

    // ── Add a file, the way the file-manager does ────────────────────────────
    std::fs::write(root.join("photos/2024/b.jpg"), b"b").unwrap();
    settle_on(
        &app,
        &repo,
        &["", "photos", "photos/2024", "photos/2024/a.jpg", "photos/2024/b.jpg", "notes.txt"],
    )
    .await;
    reconcile_agrees(&app, &repo, "new file").await;
    assert_eq!(orphan_count(&app, &repo).await, 0, "new file: no orphan expected");

    // ── Rename a file ────────────────────────────────────────────────────────
    std::fs::rename(root.join("notes.txt"), root.join("notes-2024.txt")).unwrap();
    settle_on(
        &app,
        &repo,
        &[
            "",
            "photos",
            "photos/2024",
            "photos/2024/a.jpg",
            "photos/2024/b.jpg",
            "notes-2024.txt",
        ],
    )
    .await;
    reconcile_agrees(&app, &repo, "renamed file").await;
    assert_eq!(orphan_count(&app, &repo).await, 0, "renamed file: no orphan expected");

    // ── Rename a directory (its children follow) ─────────────────────────────
    std::fs::rename(root.join("photos/2024"), root.join("photos/holidays")).unwrap();
    settle_on(
        &app,
        &repo,
        &[
            "",
            "photos",
            "photos/holidays",
            "photos/holidays/a.jpg",
            "photos/holidays/b.jpg",
            "notes-2024.txt",
        ],
    )
    .await;
    reconcile_agrees(&app, &repo, "renamed directory").await;
    assert_eq!(orphan_count(&app, &repo).await, 0, "renamed directory: no orphan expected");

    // ── Move a file into another directory ───────────────────────────────────
    std::fs::create_dir(root.join("archive")).unwrap();
    std::fs::rename(root.join("photos/holidays/a.jpg"), root.join("archive/a.jpg")).unwrap();
    settle_on(
        &app,
        &repo,
        &[
            "",
            "photos",
            "photos/holidays",
            "photos/holidays/b.jpg",
            "archive",
            "archive/a.jpg",
            "notes-2024.txt",
        ],
    )
    .await;
    reconcile_agrees(&app, &repo, "moved file").await;
    assert_eq!(orphan_count(&app, &repo).await, 0, "no deletion happened yet");

    // ── Delete a file: one orphan, and the reconcile leaves it alone ─────────
    std::fs::remove_file(root.join("photos/holidays/b.jpg")).unwrap();
    settle_on(
        &app,
        &repo,
        &["", "photos", "photos/holidays", "archive", "archive/a.jpg", "notes-2024.txt"],
    )
    .await;
    reconcile_agrees(&app, &repo, "deleted file").await;
    assert_eq!(orphan_count(&app, &repo).await, 1, "the deleted file's record is preserved");

    std::fs::remove_dir_all(root).unwrap();
}

/// The same oracle for the operation a user reaches for constantly: moving a
/// whole folder somewhere else, then working inside it at its new place.
#[tokio::test(flavor = "multi_thread")]
async fn test_moving_a_folder_keeps_the_watcher_and_reconcile_in_agreement() {
    let (app, repo, root) = journey_repo("foldermove").await;
    std::fs::create_dir_all(root.join("inbox/trip")).unwrap();
    std::fs::write(root.join("inbox/trip/x.jpg"), b"x").unwrap();
    std::fs::create_dir(root.join("sorted")).unwrap();
    settle_on(&app, &repo, &["", "inbox", "inbox/trip", "inbox/trip/x.jpg", "sorted"]).await;
    reconcile_agrees(&app, &repo, "before the move").await;

    std::fs::rename(root.join("inbox/trip"), root.join("sorted/trip")).unwrap();
    settle_on(&app, &repo, &["", "inbox", "sorted", "sorted/trip", "sorted/trip/x.jpg"]).await;
    reconcile_agrees(&app, &repo, "after the move").await;

    // Working inside the folder at its new location.
    std::fs::write(root.join("sorted/trip/y.jpg"), b"y").unwrap();
    settle_on(
        &app,
        &repo,
        &["", "inbox", "sorted", "sorted/trip", "sorted/trip/x.jpg", "sorted/trip/y.jpg"],
    )
    .await;
    reconcile_agrees(&app, &repo, "new file in the moved folder").await;

    assert_eq!(orphan_count(&app, &repo).await, 0, "a move must orphan nothing");

    std::fs::remove_dir_all(root).unwrap();
}
