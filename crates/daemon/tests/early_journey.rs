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

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use metafolder_daemon::routes;
use serde_json::{json, Value};
use tower::util::ServiceExt;

mod common;
use common::{Regime, TempDir};

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
async fn journey_repo(prefix: &str, regime: Regime) -> (Router, String, TempDir) {
    let app = routes::build(std::sync::Arc::new(common::watching_state_on(regime)));
    let root = TempDir::new(&format!("journey_{prefix}"));

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

    // The regime asked for is the one running: otherwise both variants would
    // test the same source under two names.
    let (_, watch) = request(&app, "GET", &format!("/repos/{repo}/watch"), None).await;
    assert_eq!(watch["backend"], regime.backend(), "watch source: {watch}");

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

/// Waits for the watcher to settle on `expected` (its quiet period — 500 ms here,
/// see `common::watching_state` — plus
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
        let (_, body) = request(app, "GET", &format!("/repos/{repo}/tasks/{task_id}"), None).await;
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

async fn test_first_minutes_of_a_repository(regime: Regime) {
    let fs = regime.fs();
    // A folder that already holds content when the repository is created —
    // including the build junk and VCS metadata the default preset ignores.
    let app_root = {
        let (app, repo, root) = journey_repo("firstuse", regime).await;
        fs.create_dir_all(root.join("photos/2024")).unwrap();
        fs.write(root.join("photos/2024/a.jpg"), b"a").unwrap();
        fs.write(root.join("notes.txt"), b"hello").unwrap();
        fs.create_dir_all(root.join(".git/objects")).unwrap();
        fs.write(root.join(".git/HEAD"), b"ref: refs/heads/main").unwrap();
        fs.create_dir_all(root.join("node_modules/left-pad")).unwrap();
        fs.write(root.join("node_modules/left-pad/index.js"), b"//").unwrap();

        // Everything eligible is picked up; the ignored subtrees are not.
        settle_on(&app, &repo, &["", "photos", "photos/2024", "photos/2024/a.jpg", "notes.txt"])
            .await;
        reconcile_agrees(&app, &repo, "initial content").await;
        assert_eq!(orphan_count(&app, &repo).await, 0, "initial content: no orphan expected");
        (app, repo, root)
    };
    let (app, repo, root) = app_root;

    // ── Add a file, the way the file-manager does ────────────────────────────
    fs.write(root.join("photos/2024/b.jpg"), b"b").unwrap();
    settle_on(
        &app,
        &repo,
        &["", "photos", "photos/2024", "photos/2024/a.jpg", "photos/2024/b.jpg", "notes.txt"],
    )
    .await;
    reconcile_agrees(&app, &repo, "new file").await;
    assert_eq!(orphan_count(&app, &repo).await, 0, "new file: no orphan expected");

    // ── Rename a file ────────────────────────────────────────────────────────
    fs.rename(root.join("notes.txt"), root.join("notes-2024.txt")).unwrap();
    settle_on(
        &app,
        &repo,
        &["", "photos", "photos/2024", "photos/2024/a.jpg", "photos/2024/b.jpg", "notes-2024.txt"],
    )
    .await;
    reconcile_agrees(&app, &repo, "renamed file").await;
    assert_eq!(orphan_count(&app, &repo).await, 0, "renamed file: no orphan expected");

    // ── Rename a directory (its children follow) ─────────────────────────────
    fs.rename(root.join("photos/2024"), root.join("photos/holidays")).unwrap();
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
    fs.create_dir(root.join("archive")).unwrap();
    fs.rename(root.join("photos/holidays/a.jpg"), root.join("archive/a.jpg")).unwrap();
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
    fs.remove_file(root.join("photos/holidays/b.jpg")).unwrap();
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
async fn test_moving_a_folder_keeps_the_watcher_and_reconcile_in_agreement(regime: Regime) {
    let fs = regime.fs();
    let (app, repo, root) = journey_repo("foldermove", regime).await;
    fs.create_dir_all(root.join("inbox/trip")).unwrap();
    fs.write(root.join("inbox/trip/x.jpg"), b"x").unwrap();
    fs.create_dir(root.join("sorted")).unwrap();
    settle_on(&app, &repo, &["", "inbox", "inbox/trip", "inbox/trip/x.jpg", "sorted"]).await;
    reconcile_agrees(&app, &repo, "before the move").await;

    fs.rename(root.join("inbox/trip"), root.join("sorted/trip")).unwrap();
    settle_on(&app, &repo, &["", "inbox", "sorted", "sorted/trip", "sorted/trip/x.jpg"]).await;
    reconcile_agrees(&app, &repo, "after the move").await;

    // Working inside the folder at its new location.
    fs.write(root.join("sorted/trip/y.jpg"), b"y").unwrap();
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

/// A repository holding `files` (repo-relative, parents made) once the watcher
/// has settled on them and a reconcile agrees.
async fn repo_with(prefix: &str, regime: Regime, files: &[&str]) -> (Router, String, TempDir) {
    let (app, repo, root) = journey_repo(prefix, regime).await;
    let fs = regime.fs();
    let mut expected = vec![String::new()];
    for f in files {
        let path = root.join(f);
        fs.create_dir_all(path.parent().unwrap()).unwrap();
        fs.write(&path, f.as_bytes()).unwrap();
        let mut at = std::path::Path::new(f);
        while !at.as_os_str().is_empty() {
            expected.push(at.to_str().unwrap().to_string());
            at = at.parent().unwrap();
        }
    }
    expected.sort();
    expected.dedup();
    let expected: Vec<&str> = expected.iter().map(String::as_str).collect();
    settle_on(&app, &repo, &expected).await;
    reconcile_agrees(&app, &repo, "before").await;
    (app, repo, root)
}

// ── Work the source reads late ────────────────────────────────────────────────
//
// A broker reads the kernel's queue when it gets to it, and resolves each
// handle then — to where the object is *by then*; the kernel merges what is
// still unread. Under the simulated broker `fs.hold()` makes it late on
// purpose; the real sources read when they read, and must agree all the same.

async fn test_a_file_removed_and_made_again_unread_stays_tracked(regime: Regime) {
    let (app, repo, root) = repo_with("remade", regime, &["doc.txt"]).await;
    let fs = regime.fs();
    {
        let _late = fs.hold();
        fs.remove_file(root.join("doc.txt")).unwrap();
        fs.write(root.join("doc.txt"), b"a new one").unwrap();
    }
    settle_on(&app, &repo, &["", "doc.txt"]).await;
    reconcile_agrees(&app, &repo, "removed and made again").await;
    std::fs::remove_dir_all(root).ok();
}

async fn test_a_file_renamed_back_and_forth_unread_ends_where_it_is(regime: Regime) {
    let (app, repo, root) = repo_with("pingpong", regime, &["ping"]).await;
    let fs = regime.fs();
    {
        let _late = fs.hold();
        fs.rename(root.join("ping"), root.join("pong")).unwrap();
        fs.rename(root.join("pong"), root.join("ping")).unwrap();
        fs.rename(root.join("ping"), root.join("pong")).unwrap();
    }
    settle_on(&app, &repo, &["", "pong"]).await;
    reconcile_agrees(&app, &repo, "renamed there, back and there again").await;
    assert_eq!(orphan_count(&app, &repo).await, 0, "a rename orphans nothing");
    std::fs::remove_dir_all(root).ok();
}

async fn test_a_folder_made_filled_and_renamed_unread_is_tracked_at_its_name(regime: Regime) {
    let (app, repo, root) = repo_with("fillmove", regime, &[]).await;
    let fs = regime.fs();
    {
        let _late = fs.hold();
        fs.create_dir(root.join("e")).unwrap();
        fs.write(root.join("e/z"), b"z").unwrap();
        fs.rename(root.join("e"), root.join("f")).unwrap();
        fs.write(root.join("f/y"), b"y").unwrap();
    }
    settle_on(&app, &repo, &["", "f", "f/y", "f/z"]).await;
    reconcile_agrees(&app, &repo, "made, filled, renamed").await;
    assert_eq!(orphan_count(&app, &repo).await, 0);
    std::fs::remove_dir_all(root).ok();
}

async fn test_a_tree_removed_unread_leaves_one_orphan_per_entry(regime: Regime) {
    let (app, repo, root) = repo_with("rmtree", regime, &["t/a/1", "t/a/2", "t/b/3", "keep"]).await;
    let fs = regime.fs();
    {
        let _late = fs.hold();
        fs.remove_dir_all(root.join("t")).unwrap();
    }
    settle_on(&app, &repo, &["", "keep"]).await;
    reconcile_agrees(&app, &repo, "a tree removed").await;
    // t, t/a, t/b and the three files: every record kept, as orphans.
    assert_eq!(orphan_count(&app, &repo).await, 6);
    std::fs::remove_dir_all(root).ok();
}

async fn test_a_folder_moved_and_worked_in_unread_keeps_its_records(regime: Regime) {
    let (app, repo, root) = repo_with("movework", regime, &["in/trip/x.jpg", "out/k"]).await;
    let fs = regime.fs();
    {
        let _late = fs.hold();
        fs.rename(root.join("in/trip"), root.join("out/trip")).unwrap();
        fs.write(root.join("out/trip/y.jpg"), b"y").unwrap();
        fs.rename(root.join("out/trip/x.jpg"), root.join("out/x.jpg")).unwrap();
    }
    settle_on(&app, &repo, &["", "in", "out", "out/k", "out/trip", "out/trip/y.jpg", "out/x.jpg"])
        .await;
    reconcile_agrees(&app, &repo, "moved and worked in").await;
    assert_eq!(orphan_count(&app, &repo).await, 0);
    std::fs::remove_dir_all(root).ok();
}

async fn test_an_atomic_save_unread_keeps_the_saved_file(regime: Regime) {
    // How editors save: write a temporary, rename it over the file.
    let (app, repo, root) = repo_with("atomicsave", regime, &["notes.md"]).await;
    let fs = regime.fs();
    {
        let _late = fs.hold();
        fs.write(root.join(".notes.md.tmp"), b"v2").unwrap();
        fs.rename(root.join(".notes.md.tmp"), root.join("notes.md")).unwrap();
        fs.write(root.join(".notes.md.tmp"), b"v3").unwrap();
        fs.rename(root.join(".notes.md.tmp"), root.join("notes.md")).unwrap();
    }
    settle_on(&app, &repo, &["", "notes.md"]).await;
    reconcile_agrees(&app, &repo, "saved twice").await;
    std::fs::remove_dir_all(root).ok();
}

async fn test_a_folder_removed_and_made_again_unread_is_tracked(regime: Regime) {
    let (app, repo, root) = repo_with("remadedir", regime, &["d/old"]).await;
    let fs = regime.fs();
    {
        let _late = fs.hold();
        fs.remove_dir_all(root.join("d")).unwrap();
        fs.create_dir(root.join("d")).unwrap();
        fs.write(root.join("d/new"), b"n").unwrap();
    }
    settle_on(&app, &repo, &["", "d", "d/new"]).await;
    reconcile_agrees(&app, &repo, "folder removed and made again").await;
    std::fs::remove_dir_all(root).ok();
}

async fn test_a_folder_moved_out_and_back_unread_keeps_its_records(regime: Regime) {
    let (app, repo, root) = repo_with("outandback", regime, &["d/f"]).await;
    let fs = regime.fs();
    let away = common::TempDir::new("away");
    {
        let _late = fs.hold();
        fs.rename(root.join("d"), away.join("d")).unwrap();
        fs.rename(away.join("d"), root.join("d")).unwrap();
    }
    settle_on(&app, &repo, &["", "d", "d/f"]).await;
    reconcile_agrees(&app, &repo, "out and back").await;
    std::fs::remove_dir_all(root).ok();
}

/// The metarecord tracked at `rel` (repo-relative), if one is.
async fn uuid_at(app: &Router, repo: &str, rel: &str) -> Option<String> {
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
    body.as_object().expect("a uuid → paths map").iter().find_map(|(uuid, paths)| {
        let at = paths.as_array()?.iter().filter_map(|p| p.as_str());
        at.into_iter().any(|p| p.trim_start_matches('/') == rel).then(|| uuid.clone())
    })
}

// ── Renames and removals of the same names, unread ───────────────────────────
//
// The names are reused within one unread burst, so a path in the stream can
// stand for two objects in turn: each scenario checks the identities a user
// would expect to survive, beside the reconcile oracle.

async fn test_two_files_swapped_through_a_temporary_unread_keep_their_records(regime: Regime) {
    let (app, repo, root) = repo_with("swaptmp", regime, &["a", "b"]).await;
    let (a, b) = (uuid_at(&app, &repo, "a").await, uuid_at(&app, &repo, "b").await);
    let fs = regime.fs();
    {
        let _late = fs.hold();
        fs.rename(root.join("a"), root.join("t")).unwrap();
        fs.rename(root.join("b"), root.join("a")).unwrap();
        fs.rename(root.join("t"), root.join("b")).unwrap();
    }
    for _ in 0..100 {
        if uuid_at(&app, &repo, "a").await == b {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    settle_on(&app, &repo, &["", "a", "b"]).await;
    reconcile_agrees(&app, &repo, "swapped through a temporary").await;
    assert_eq!(uuid_at(&app, &repo, "a").await, b, "b's record follows it to a");
    assert_eq!(uuid_at(&app, &repo, "b").await, a, "a's record follows it to b");
    assert_eq!(orphan_count(&app, &repo).await, 0, "a swap orphans nothing");
}

async fn test_a_file_renamed_over_another_and_its_name_reused_unread(regime: Regime) {
    let (app, repo, root) = repo_with("overreuse", regime, &["a", "b"]).await;
    let a = uuid_at(&app, &repo, "a").await;
    let fs = regime.fs();
    {
        let _late = fs.hold();
        fs.rename(root.join("a"), root.join("b")).unwrap();
        fs.write(root.join("a"), b"a new one").unwrap();
    }
    for _ in 0..100 {
        if uuid_at(&app, &repo, "b").await == a {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    settle_on(&app, &repo, &["", "a", "b"]).await;
    reconcile_agrees(&app, &repo, "renamed over, name reused").await;
    assert_eq!(uuid_at(&app, &repo, "b").await, a, "a's record follows it onto b");
    assert_ne!(uuid_at(&app, &repo, "a").await, a, "the new a is a new file");
    assert_eq!(orphan_count(&app, &repo).await, 1, "only the replaced b is gone");
}

async fn test_a_file_removed_and_another_renamed_onto_its_name_unread(regime: Regime) {
    let (app, repo, root) = repo_with("rmonto", regime, &["a", "b"]).await;
    let b = uuid_at(&app, &repo, "b").await;
    let fs = regime.fs();
    {
        let _late = fs.hold();
        fs.remove_file(root.join("a")).unwrap();
        fs.rename(root.join("b"), root.join("a")).unwrap();
    }
    settle_on(&app, &repo, &["", "a"]).await;
    reconcile_agrees(&app, &repo, "removed, another renamed onto it").await;
    assert_eq!(uuid_at(&app, &repo, "a").await, b, "b's record follows it to a");
    assert_eq!(orphan_count(&app, &repo).await, 1, "only the removed a is gone");
}

async fn test_a_file_renamed_twice_removed_and_its_name_reused_unread(regime: Regime) {
    let (app, repo, root) = repo_with("chainrm", regime, &["a"]).await;
    let a = uuid_at(&app, &repo, "a").await;
    let fs = regime.fs();
    {
        let _late = fs.hold();
        fs.rename(root.join("a"), root.join("b")).unwrap();
        fs.rename(root.join("b"), root.join("c")).unwrap();
        fs.remove_file(root.join("c")).unwrap();
        fs.write(root.join("c"), b"another c").unwrap();
    }
    settle_on(&app, &repo, &["", "c"]).await;
    reconcile_agrees(&app, &repo, "renamed twice, removed, name reused").await;
    // A removal whose path exists again at flush time is a refresh (doc
    // "Event semantics", undone deletions): c is the record a became.
    assert_eq!(uuid_at(&app, &repo, "c").await, a, "the file at c keeps a's record");
    assert_eq!(orphan_count(&app, &repo).await, 0, "the name came back: nothing is orphaned");
}

async fn test_a_folder_renamed_and_a_file_removed_from_it_unread(regime: Regime) {
    // The removal happens inside the folder's new name before the source read
    // the rename: a watch per directory still answers to the old one.
    let (app, repo, root) = repo_with("renamerm", regime, &["d/x", "d/y"]).await;
    let y = uuid_at(&app, &repo, "d/y").await;
    let fs = regime.fs();
    {
        let _late = fs.hold();
        fs.rename(root.join("d"), root.join("e")).unwrap();
        fs.remove_file(root.join("e/x")).unwrap();
    }
    settle_on(&app, &repo, &["", "e", "e/y"]).await;
    reconcile_agrees(&app, &repo, "folder renamed, a file removed from it").await;
    assert_eq!(uuid_at(&app, &repo, "e/y").await, y);
    assert_eq!(orphan_count(&app, &repo).await, 1, "the removed x is the one orphan");
}

async fn test_a_folder_renamed_and_a_file_moved_out_of_the_repository_unread(regime: Regime) {
    let (app, repo, root) = repo_with("renameout", regime, &["d/x", "d/y"]).await;
    let fs = regime.fs();
    let away = common::TempDir::new("renameout_away");
    {
        let _late = fs.hold();
        fs.rename(root.join("d"), root.join("e")).unwrap();
        fs.rename(root.join("e/x"), away.join("x")).unwrap();
    }
    settle_on(&app, &repo, &["", "e", "e/y"]).await;
    reconcile_agrees(&app, &repo, "folder renamed, a file moved out").await;
    assert_eq!(orphan_count(&app, &repo).await, 1, "x left: its record is the one orphan");
}

async fn test_a_folder_renamed_worked_in_and_its_name_reused_unread(regime: Regime) {
    let (app, repo, root) = repo_with("dirreuse", regime, &["d/x", "d/y"]).await;
    let (x, y) = (uuid_at(&app, &repo, "d/x").await, uuid_at(&app, &repo, "d/y").await);
    let fs = regime.fs();
    {
        let _late = fs.hold();
        fs.rename(root.join("d"), root.join("e")).unwrap();
        fs.remove_file(root.join("e/x")).unwrap();
        fs.create_dir(root.join("d")).unwrap();
        fs.write(root.join("d/x"), b"a new x").unwrap();
        fs.rename(root.join("e/y"), root.join("d/y")).unwrap();
    }
    settle_on(&app, &repo, &["", "d", "d/x", "d/y", "e"]).await;
    reconcile_agrees(&app, &repo, "folder renamed, worked in, name reused").await;
    assert_eq!(uuid_at(&app, &repo, "d/y").await, y, "y's record follows it back into d");
    assert_ne!(uuid_at(&app, &repo, "d/x").await, x, "the new d/x is a new file");
    assert_eq!(orphan_count(&app, &repo).await, 1, "only the removed x is gone");
}

on_every_regime!(
    test_first_minutes_of_a_repository,
    test_moving_a_folder_keeps_the_watcher_and_reconcile_in_agreement,
    test_a_file_removed_and_made_again_unread_stays_tracked,
    test_a_file_renamed_back_and_forth_unread_ends_where_it_is,
    test_a_folder_made_filled_and_renamed_unread_is_tracked_at_its_name,
    test_a_tree_removed_unread_leaves_one_orphan_per_entry,
    test_a_folder_moved_and_worked_in_unread_keeps_its_records,
    test_an_atomic_save_unread_keeps_the_saved_file,
    test_a_folder_removed_and_made_again_unread_is_tracked,
    test_a_folder_moved_out_and_back_unread_keeps_its_records,
    test_two_files_swapped_through_a_temporary_unread_keep_their_records,
    test_a_file_renamed_over_another_and_its_name_reused_unread,
    test_a_file_removed_and_another_renamed_onto_its_name_unread,
    test_a_file_renamed_twice_removed_and_its_name_reused_unread,
    test_a_folder_renamed_and_a_file_removed_from_it_unread,
    test_a_folder_renamed_and_a_file_moved_out_of_the_repository_unread,
    test_a_folder_renamed_worked_in_and_its_name_reused_unread,
);
