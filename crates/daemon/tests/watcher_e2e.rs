//! End-to-end test: a repository initialised through the HTTP API watches
//! its root via inotify; file operations show up as metadata entries after
//! the executor's quiet period.

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

/// Polls the query endpoint until the predicate yields a hit or times out.
async fn wait_for_match(app: &Router, repo: &str, query: Value, expect: usize) -> Vec<String> {
    for _ in 0..50 {
        let (status, body) =
            request(app, "POST", &format!("/repos/{repo}/query"), Some(json!({"query": query})))
                .await;
        assert_eq!(status, StatusCode::OK, "query failed: {body}");
        let hits: Vec<String> =
            body.as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
        if hits.len() == expect {
            return hits;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {expect} match(es)");
}

/// Every tracked path in the repository, repo-root-relative and without the
/// leading slash (the root itself is `""`), sorted. Read through the daemon's
/// own tree resolution, so it reflects what a client would see.
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

/// Polls until the single metarecord tracking `pattern` is `uuid`.
async fn wait_for_uuid_at(app: &Router, repo: &str, pattern: &str, uuid: &str) {
    let query = json!({"type": "matches", "field": "mfr_path", "pattern": format!("^{pattern}$")});
    let mut last = Vec::new();
    for _ in 0..100 {
        let (_, body) =
            request(app, "POST", &format!("/repos/{repo}/query"), Some(json!({"query": query})))
                .await;
        last = body
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
            .unwrap_or_default();
        if last == vec![uuid.to_string()] {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the metarecord at {pattern} never became {uuid}; last seen {last:?}");
}

/// Polls until the repository's tracked paths are exactly `expected`.
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

/// A repository initialised through the HTTP API with tracking enabled on its
/// root — the state a user reaches right after creating their first repository.
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

#[tokio::test(flavor = "multi_thread")]
async fn test_watcher_tracks_create_rename_delete() {
    let app = routes::build(std::sync::Arc::new(AppState::new()));
    let root = TempDir::new("e2e");

    let (status, body) =
        request(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    let repo = body["repo_uuid"].as_str().unwrap().to_string();

    // Enable tracking on the root entry.
    let (_, roots) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": {"type": "is_present", "field": "mf_watch"}})),
    )
    .await;
    let root_uuid = roots[0].as_str().unwrap().to_string();
    let (status, _) = request(
        &app,
        "PUT",
        &format!("/repos/{repo}/metarecords/{root_uuid}/fields/mf_watch"),
        Some(json!({"value": {"type": "bool", "value": true}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Create.
    std::fs::write(root.join("track_me.txt"), b"hello watcher").unwrap();
    let by_name = json!({"type": "matches", "field": "mfr_path", "pattern": "^track_me\\.txt$"});
    let hits = wait_for_match(&app, &repo, by_name, 1).await;
    let metarecord_uuid = hits[0].clone();

    // Rename.
    std::fs::rename(root.join("track_me.txt"), root.join("renamed.txt")).unwrap();
    let renamed = json!({"type": "matches", "field": "mfr_path", "pattern": "^renamed\\.txt$"});
    let hits = wait_for_match(&app, &repo, renamed, 1).await;
    assert_eq!(hits[0], metarecord_uuid, "the entry must survive the rename");

    // Delete: mfr_path becomes Nothing, the entry is preserved.
    std::fs::remove_file(root.join("renamed.txt")).unwrap();
    let absent = json!({"type": "is_absent", "field": "mfr_path"});
    let hits = wait_for_match(&app, &repo, absent, 1).await;
    assert_eq!(hits[0], metarecord_uuid, "the entry must be preserved after deletion");

    std::fs::remove_dir_all(root).unwrap();
}

/// Regression: a repository whose root contains a symlink to a directory the
/// daemon cannot read (the classic Wine `~/.wine/dosdevices/z: -> /` case) must
/// still load. The watcher must not follow the symlink, and one unwatchable
/// path must never abort the whole load.
#[tokio::test(flavor = "multi_thread")]
async fn test_load_succeeds_with_symlink_to_unreadable_dir() {
    use std::os::unix::fs::PermissionsExt;

    let app = routes::build(std::sync::Arc::new(AppState::new()));
    let root = TempDir::new("symlink");

    // A readable directory outside the repo that itself contains an *unreadable*
    // subdirectory, and a symlink to it inside the repo root. This mirrors the
    // real case `z: -> /` where `/` is readable but `/opt/containerd` is not:
    // following the symlink and trying to add an inotify watch on the deep
    // unreadable directory EACCESes.
    let secret = TempDir::new("secret");
    let locked = secret.join("locked");
    std::fs::create_dir_all(&locked).unwrap();
    std::os::unix::fs::symlink(&secret, root.join("z")).unwrap();
    let mut perm = std::fs::metadata(&locked).unwrap().permissions();
    perm.set_mode(0o000);
    std::fs::set_permissions(&locked, perm).unwrap();

    // Init: must succeed (a fresh repo is mf_watch=false — nothing watched).
    let (status, body) =
        request(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "init must not fail on the symlink: {body}");
    let repo = body["repo_uuid"].as_str().unwrap().to_string();

    // Enabling watch on the root triggers a watch refresh that walks the tree:
    // it must skip the symlink (not a real directory) and not EACCES.
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
    assert_eq!(status, StatusCode::OK, "enabling watch must not fail on the symlink: {body}");

    // A real file created in the root is still detected: the refresh placed a
    // watch on the (eligible) root directory.
    std::fs::write(root.join("real.txt"), b"hello").unwrap();
    let by_name = json!({"type": "matches", "field": "mfr_path", "pattern": "^real\\.txt$"});
    wait_for_match(&app, &repo, by_name, 1).await;

    // Cleanup: restore permissions so the trees can be removed.
    let mut perm = std::fs::metadata(&locked).unwrap().permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(&locked, perm).unwrap();
    std::fs::remove_dir_all(&secret).ok();
    std::fs::remove_dir_all(&root).ok();
}

/// Regression: creating a *directory* under a watched root must not wedge the
/// daemon. The new directory needs its own inotify watch (watching is
/// per-directory, not recursive), but notify's `watch()` cannot be called from
/// inside its own event callback — it waits for the very event loop that is
/// running the callback. Doing so blocks the watcher thread forever while it
/// holds the repository connection, and every request then hangs behind it.
#[tokio::test(flavor = "multi_thread")]
async fn test_new_directory_does_not_wedge_the_daemon() {
    let app = routes::build(std::sync::Arc::new(AppState::new()));
    let root = TempDir::new("newdir");

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
    let (status, _) = request(
        &app,
        "PUT",
        &format!("/repos/{repo}/metarecords/{root_uuid}/fields/mf_watch"),
        Some(json!({"value": {"type": "bool", "value": true}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // A directory with a nested file: the watcher must ingest both and stay
    // responsive. The timeout turns the deadlock into a failure instead of a
    // test run that never ends.
    std::fs::create_dir(root.join("A")).unwrap();
    std::fs::write(root.join("A/B.txt"), b"bee").unwrap();
    let nested = json!({"type": "matches", "field": "mfr_path", "pattern": "^B\\.txt$"});
    tokio::time::timeout(Duration::from_secs(20), wait_for_match(&app, &repo, nested, 1))
        .await
        .expect("the daemon must stay responsive after a directory is created");

    // The new directory got its own watch: a file created in it *after* the
    // arrival was processed is tracked too (nothing rescans it later).
    std::fs::write(root.join("A/C.txt"), b"cee").unwrap();
    let later = json!({"type": "matches", "field": "mfr_path", "pattern": "^C\\.txt$"});
    tokio::time::timeout(Duration::from_secs(20), wait_for_match(&app, &repo, later, 1))
        .await
        .expect("the new directory must be watched for its own future events");

    std::fs::remove_dir_all(root).unwrap();
}

/// Everyday directory operations, against a live watcher. Watching is
/// *per-directory*: every one of these moves a watch's own directory, so the
/// live set has to follow the filesystem or later events land under a stale
/// path (or nowhere at all). The executor tests cover the same semantics from
/// synthetic events — what they cannot see is whether the events arrive.
#[tokio::test(flavor = "multi_thread")]
async fn test_renamed_directory_keeps_being_watched() {
    let (app, repo, root) = watched_repo("dirrename").await;

    std::fs::create_dir(root.join("A")).unwrap();
    std::fs::write(root.join("A/one.txt"), b"1").unwrap();
    let one = json!({"type": "matches", "field": "mfr_path", "pattern": "^one\\.txt$"});
    tokio::time::timeout(Duration::from_secs(20), wait_for_match(&app, &repo, one, 1))
        .await
        .expect("the nested file is tracked");

    // Rename the directory, then create a file *inside it under its new name*.
    std::fs::rename(root.join("A"), root.join("B")).unwrap();
    tokio::time::timeout(
        Duration::from_secs(20),
        wait_for_paths(&app, &repo, &["", "B", "B/one.txt"]),
    )
    .await
    .expect("the rename is recorded for the directory and its child");

    std::fs::write(root.join("B/two.txt"), b"2").unwrap();
    let two = json!({"type": "matches", "field": "mfr_path", "pattern": "^two\\.txt$"});
    tokio::time::timeout(Duration::from_secs(20), wait_for_match(&app, &repo, two, 1))
        .await
        .expect("a file created in the renamed directory must still be seen");
    // …and at the right place: under B, not under the directory's old name.
    tokio::time::timeout(
        Duration::from_secs(20),
        wait_for_paths(&app, &repo, &["", "B", "B/one.txt", "B/two.txt"]),
    )
    .await
    .expect("the new file is tracked under the directory's current path");

    std::fs::remove_dir_all(root).unwrap();
}

/// A file moved between two watched directories keeps its metarecord and lands
/// at the new path (one revision, not delete + create).
#[tokio::test(flavor = "multi_thread")]
async fn test_file_moved_between_directories_keeps_its_metarecord() {
    let (app, repo, root) = watched_repo("dirmove").await;

    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::create_dir(root.join("dst")).unwrap();
    std::fs::write(root.join("src/x.txt"), b"x").unwrap();
    let by_name = json!({"type": "matches", "field": "mfr_path", "pattern": "^x\\.txt$"});
    let hits = tokio::time::timeout(
        Duration::from_secs(20),
        wait_for_match(&app, &repo, by_name.clone(), 1),
    )
    .await
    .expect("tracked in src");
    let uuid = hits[0].clone();

    std::fs::rename(root.join("src/x.txt"), root.join("dst/x.txt")).unwrap();
    tokio::time::timeout(
        Duration::from_secs(20),
        wait_for_paths(&app, &repo, &["", "src", "dst", "dst/x.txt"]),
    )
    .await
    .expect("the file is recorded under dst");

    let hits =
        tokio::time::timeout(Duration::from_secs(20), wait_for_match(&app, &repo, by_name, 1))
            .await
            .expect("exactly one metarecord for the moved file");
    assert_eq!(hits[0], uuid, "the move must preserve the metarecord, not duplicate it");

    std::fs::remove_dir_all(root).unwrap();
}

/// A whole subtree created at once (`mkdir -p a/b` then a file) is ingested:
/// the watch on each new directory has to be placed before its own children
/// arrive, and whatever slipped through has to be caught by the arrival scan.
#[tokio::test(flavor = "multi_thread")]
async fn test_nested_subtree_created_at_once_is_ingested() {
    let (app, repo, root) = watched_repo("subtree").await;

    std::fs::create_dir_all(root.join("a/b/c")).unwrap();
    std::fs::write(root.join("a/b/c/deep.txt"), b"deep").unwrap();
    std::fs::write(root.join("a/top.txt"), b"top").unwrap();

    tokio::time::timeout(
        Duration::from_secs(20),
        wait_for_paths(&app, &repo, &["", "a", "a/b", "a/b/c", "a/b/c/deep.txt", "a/top.txt"]),
    )
    .await
    .expect("every node of the new subtree is tracked");

    // The deepest directory is watched too: a file added later is picked up.
    std::fs::write(root.join("a/b/c/later.txt"), b"later").unwrap();
    let later = json!({"type": "matches", "field": "mfr_path", "pattern": "^later\\.txt$"});
    tokio::time::timeout(Duration::from_secs(20), wait_for_match(&app, &repo, later, 1))
        .await
        .expect("the deepest new directory must be watched for its own events");

    std::fs::remove_dir_all(root).unwrap();
}

/// Removing a directory orphans its whole subtree, and recreating the same path
/// starts tracking again (the watch was dropped with the directory and has to be
/// placed anew).
#[tokio::test(flavor = "multi_thread")]
async fn test_removed_directory_can_be_recreated_and_tracked_again() {
    let (app, repo, root) = watched_repo("dirremove").await;

    std::fs::create_dir(root.join("d")).unwrap();
    std::fs::write(root.join("d/f.txt"), b"f").unwrap();
    tokio::time::timeout(
        Duration::from_secs(20),
        wait_for_paths(&app, &repo, &["", "d", "d/f.txt"]),
    )
    .await
    .expect("tracked before the removal");

    std::fs::remove_dir_all(root.join("d")).unwrap();
    tokio::time::timeout(Duration::from_secs(20), wait_for_paths(&app, &repo, &[""]))
        .await
        .expect("the subtree is orphaned");

    std::fs::create_dir(root.join("d")).unwrap();
    std::fs::write(root.join("d/g.txt"), b"g").unwrap();
    tokio::time::timeout(
        Duration::from_secs(20),
        wait_for_paths(&app, &repo, &["", "d", "d/g.txt"]),
    )
    .await
    .expect("a recreated directory is watched again");

    std::fs::remove_dir_all(root).unwrap();
}

/// Moving a file into a directory created in the same breath — "make a folder,
/// drop the file in it", the most ordinary file-manager gesture there is. The
/// file must keep its metarecord: everything the user attached to it (tags,
/// ratings, notes) hangs off that identity, and a fresh metarecord at the new
/// path silently leaves all of it behind on an orphan.
#[tokio::test(flavor = "multi_thread")]
async fn test_move_into_a_brand_new_directory_keeps_the_metarecord() {
    let (app, repo, root) = watched_repo("newdirmove").await;

    std::fs::write(root.join("x.txt"), b"x").unwrap();
    let by_name = json!({"type": "matches", "field": "mfr_path", "pattern": "^x\\.txt$"});
    let hits = tokio::time::timeout(
        Duration::from_secs(20),
        wait_for_match(&app, &repo, by_name.clone(), 1),
    )
    .await
    .expect("tracked at the root");
    let uuid = hits[0].clone();

    // The directory and the move land in the same watcher batch.
    std::fs::create_dir(root.join("dest")).unwrap();
    std::fs::rename(root.join("x.txt"), root.join("dest/x.txt")).unwrap();

    tokio::time::timeout(
        Duration::from_secs(20),
        wait_for_paths(&app, &repo, &["", "dest", "dest/x.txt"]),
    )
    .await
    .expect("the file is tracked at its new path");

    let hits =
        tokio::time::timeout(Duration::from_secs(20), wait_for_match(&app, &repo, by_name, 1))
            .await
            .expect("exactly one metarecord for the moved file");
    assert_eq!(hits[0], uuid, "the move must keep the metarecord, not create a second one");

    // And nothing was left orphaned behind it.
    let (_, orphans) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": {"type": "is_absent", "field": "mfr_path"}})),
    )
    .await;
    assert_eq!(orphans.as_array().unwrap().len(), 0, "a move must orphan nothing: {orphans}");

    std::fs::remove_dir_all(root).unwrap();
}

/// The same gesture with a whole folder: "make a folder, drag another one into
/// it". The subtree must keep its metarecords — children reference their parent
/// by uuid, so re-homing the directory carries them along; duplicating it
/// instead orphans the whole subtree at once.
#[tokio::test(flavor = "multi_thread")]
async fn test_move_a_directory_into_a_brand_new_directory_keeps_the_subtree() {
    let (app, repo, root) = watched_repo("newdirdirmove").await;

    std::fs::create_dir(root.join("trip")).unwrap();
    std::fs::write(root.join("trip/x.jpg"), b"x").unwrap();
    tokio::time::timeout(
        Duration::from_secs(20),
        wait_for_paths(&app, &repo, &["", "trip", "trip/x.jpg"]),
    )
    .await
    .expect("tracked before the move");
    let by_name = json!({"type": "matches", "field": "mfr_path", "pattern": "^x\\.jpg$"});
    let hits = wait_for_match(&app, &repo, by_name.clone(), 1).await;
    let child = hits[0].clone();

    std::fs::create_dir(root.join("albums")).unwrap();
    std::fs::rename(root.join("trip"), root.join("albums/trip")).unwrap();

    tokio::time::timeout(
        Duration::from_secs(20),
        wait_for_paths(&app, &repo, &["", "albums", "albums/trip", "albums/trip/x.jpg"]),
    )
    .await
    .expect("the subtree is tracked at its new path");

    let hits = wait_for_match(&app, &repo, by_name, 1).await;
    assert_eq!(hits[0], child, "the nested file keeps its metarecord");

    let (_, orphans) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": {"type": "is_absent", "field": "mfr_path"}})),
    )
    .await;
    assert_eq!(orphans.as_array().unwrap().len(), 0, "nothing orphaned: {orphans}");

    std::fs::remove_dir_all(root).unwrap();
}

/// Overwriting a tracked file by moving another one onto it — what `mv`, `cp`,
/// a download and every editor that saves atomically all do.
///
/// The tree index allows one metarecord per path, so the moved record cannot
/// take a position another record still holds. Left unhandled, the whole flush
/// fails on the constraint, its batch is never drained, and the watcher stops
/// recording *anything* for that repository from then on — including across a
/// restart, since the pending buffer is replayed at load. The last assertion is
/// that one: the watcher is still alive afterwards.
#[tokio::test(flavor = "multi_thread")]
async fn test_overwriting_a_tracked_file_keeps_the_watcher_alive() {
    let (app, repo, root) = watched_repo("overwrite").await;

    std::fs::write(root.join("a.txt"), b"AAA").unwrap();
    std::fs::write(root.join("b.txt"), b"BBB").unwrap();
    tokio::time::timeout(
        Duration::from_secs(20),
        wait_for_paths(&app, &repo, &["", "a.txt", "b.txt"]),
    )
    .await
    .expect("both files tracked");
    let a = wait_for_match(
        &app,
        &repo,
        json!({"type": "matches", "field": "mfr_path", "pattern": "^a\\.txt$"}),
        1,
    )
    .await[0]
        .clone();

    // a.txt's bytes are now at b.txt; b.txt's own bytes are destroyed.
    std::fs::rename(root.join("a.txt"), root.join("b.txt")).unwrap();
    tokio::time::timeout(Duration::from_secs(20), wait_for_paths(&app, &repo, &["", "b.txt"]))
        .await
        .expect("only b.txt is left on disk, and only it is tracked");

    // The surviving file keeps *its own* metarecord: the record that moved is
    // the one whose bytes are still there.
    let at_b = wait_for_match(
        &app,
        &repo,
        json!({"type": "matches", "field": "mfr_path", "pattern": "^b\\.txt$"}),
        1,
    )
    .await;
    assert_eq!(at_b[0], a, "the moved file's metarecord follows it to b.txt");

    // The displaced record is orphaned — its content is gone, like a deletion.
    let (_, orphans) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": {"type": "is_absent", "field": "mfr_path"}})),
    )
    .await;
    assert_eq!(
        orphans.as_array().unwrap().len(),
        1,
        "the overwritten file's record is preserved as an orphan: {orphans}"
    );

    // The watcher still works: the flush was not left stuck on a failing batch.
    std::fs::write(root.join("later.txt"), b"later").unwrap();
    tokio::time::timeout(
        Duration::from_secs(20),
        wait_for_paths(&app, &repo, &["", "b.txt", "later.txt"]),
    )
    .await
    .expect("the watcher must keep recording after an overwrite");

    std::fs::remove_dir_all(root).unwrap();
}

/// Swapping two tracked files through a temporary name — three renames in one
/// batch, each landing on a path another metarecord held moments before. Both
/// records must survive and end up crossed over; evicting a destination too
/// eagerly would orphan a record that the next rename was about to fill.
#[tokio::test(flavor = "multi_thread")]
async fn test_swapping_two_tracked_files_keeps_both_metarecords() {
    let (app, repo, root) = watched_repo("swap").await;

    std::fs::write(root.join("a.txt"), b"AAAA").unwrap();
    std::fs::write(root.join("b.txt"), b"BB").unwrap();
    tokio::time::timeout(
        Duration::from_secs(20),
        wait_for_paths(&app, &repo, &["", "a.txt", "b.txt"]),
    )
    .await
    .expect("both tracked");
    let at = |name: &str| json!({"type": "matches", "field": "mfr_path", "pattern": format!("^{name}$")});
    let a = wait_for_match(&app, &repo, at("a\\.txt"), 1).await[0].clone();
    let b = wait_for_match(&app, &repo, at("b\\.txt"), 1).await[0].clone();

    std::fs::rename(root.join("a.txt"), root.join("swap.tmp")).unwrap();
    std::fs::rename(root.join("b.txt"), root.join("a.txt")).unwrap();
    std::fs::rename(root.join("swap.tmp"), root.join("b.txt")).unwrap();

    // Both names still exist, so "which paths are tracked" cannot tell the swap
    // apart from the state before it: wait for the records to have crossed over.
    tokio::time::timeout(Duration::from_secs(20), wait_for_uuid_at(&app, &repo, "a\\.txt", &b))
        .await
        .expect("b's metarecord must follow its bytes to a.txt");
    assert_eq!(wait_for_match(&app, &repo, at("b\\.txt"), 1).await[0], a, "a's record is at b.txt");

    let (_, orphans) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": {"type": "is_absent", "field": "mfr_path"}})),
    )
    .await;
    assert_eq!(orphans.as_array().unwrap().len(), 0, "a swap orphans nothing: {orphans}");

    std::fs::remove_dir_all(root).unwrap();
}
