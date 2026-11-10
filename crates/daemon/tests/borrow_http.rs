//! Borrowing a repository of another machine (doc "Cross-machine sync"): its
//! `.metafolder/` copied to a local directory and loaded from there, the
//! remote locked meanwhile, then given back. The "remote" is a directory
//! another daemon state owns: the mount of another machine's files, seen from
//! here, is a directory like any other.

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

fn app() -> Router {
    routes::build(std::sync::Arc::new(AppState::new(common::settings())))
}

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

fn note(text: &str) -> Value {
    json!([{"name": "note", "value": {"type": "string", "value": text}}])
}

async fn create(app: &Router, repo: &str, fields: Value) -> String {
    let (status, body) = request(
        app,
        "POST",
        &format!("/repos/{repo}/metarecords"),
        Some(json!({"fields": fields, "force": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create failed: {body}");
    body["uuid"].as_str().unwrap().to_string()
}

/// The notes a repository holds, sorted.
async fn notes(app: &Router, repo: &str) -> Vec<String> {
    let (status, uuids) = request(
        app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": {"type": "is_present", "field": "note"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "query failed: {uuids}");
    let mut out = Vec::new();
    for uuid in uuids.as_array().unwrap() {
        let uuid = uuid.as_str().unwrap();
        let (_, record) =
            request(app, "GET", &format!("/repos/{repo}/metarecords/{uuid}"), None).await;
        for f in record["fields"].as_array().unwrap() {
            if f["name"] == "note" {
                out.push(f["value"]["value"].as_str().unwrap().to_string());
            }
        }
    }
    out.sort();
    out
}

/// A repository of "the other machine", initialised and given a note by its
/// own daemon, which then lets go of it: (its uuid, its root).
async fn remote_repo(remote: &Router, name: &str) -> (String, TempDir) {
    let root = TempDir::new(&format!("borrow_{name}"));
    common::seed_config(&root);
    let (status, body) = request(
        remote,
        "POST",
        "/repos/init",
        Some(json!({"root": root.to_str().unwrap(), "name": name})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    let uuid = body["repo_uuid"].as_str().unwrap().to_string();
    create(remote, &uuid, note("from the remote")).await;
    unload(remote, &uuid).await;
    (uuid, root)
}

async fn unload(app: &Router, repo: &str) {
    let (status, body) = request(app, "POST", &format!("/repos/{repo}/unload"), None).await;
    assert_eq!(status, StatusCode::OK, "unload failed: {body}");
}

async fn load(app: &Router, root: &std::path::Path) -> (StatusCode, Value) {
    request(app, "POST", "/repos/load", Some(json!({"root": root.to_str().unwrap()}))).await
}

/// Loads `root` and waits for it to be ready.
async fn load_ready(app: &Router, root: &std::path::Path) -> String {
    let (status, body) = load(app, root).await;
    assert_eq!(status, StatusCode::OK, "load failed: {body}");
    let uuid = body["repo_uuid"].as_str().unwrap().to_string();
    for _ in 0..400 {
        let (_, info) = request(app, "GET", &format!("/repos/{uuid}"), None).await;
        if info["ready"] == true {
            return uuid;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("the load did not finish");
}

async fn borrow(
    app: &Router,
    root: &std::path::Path,
    to: &std::path::Path,
    extra: Value,
) -> (StatusCode, Value) {
    let mut body = json!({"root": root.to_str().unwrap(), "to": to.to_str().unwrap()});
    for (k, v) in extra.as_object().unwrap() {
        body[k] = v.clone();
    }
    request(app, "POST", "/repos/borrow", Some(body)).await
}

fn marker(root: &std::path::Path) -> std::path::PathBuf {
    root.join(".metafolder/internal/borrowed.json")
}

/// A borrow copies the repository, locks the remote and loads the copy, under
/// another name when asked: the same records, the remote's root, and the
/// remote's own `config.json` left as it was.
#[tokio::test]
async fn a_borrowed_repository_is_a_loaded_local_copy() {
    let (remote, local) = (app(), app());
    let (uuid, root) = remote_repo(&remote, "photos").await;
    let here = TempDir::new("borrow_here");
    let to = here.join("photos-copy");
    let (status, body) = borrow(&local, &root, &to, json!({"name": "photos@remote"})).await;
    assert_eq!(status, StatusCode::OK, "borrow failed: {body}");
    assert_eq!(body["repo_uuid"], uuid, "the same repository");

    let (_, info) = request(&local, "GET", &format!("/repos/{uuid}"), None).await;
    assert_eq!(info["name"], "photos@remote");
    assert_eq!(info["root"], json!(root.canonicalize().unwrap()), "the remote's files");
    assert_eq!(
        info["borrowed_from"],
        json!(root.canonicalize().unwrap().join(".metafolder")),
        "{info}"
    );
    assert!(to.join("internal/kv/data.mdb").is_file(), "the store is local");
    assert_eq!(notes(&local, &uuid).await, ["from the remote"]);

    assert!(marker(&root).is_file(), "the remote is marked borrowed");
    let remote_config: Value = serde_json::from_str(
        &std::fs::read_to_string(root.join(".metafolder/config.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(remote_config["name"], "photos", "the remote's own file is untouched");
    assert_eq!(remote_config["root"], ".");
}

/// While borrowed, the remote's own daemon refuses to load it, naming the
/// copy; a second borrow is refused too.
#[tokio::test]
async fn a_borrowed_repository_does_not_load_at_home() {
    let (remote, local) = (app(), app());
    let (_uuid, root) = remote_repo(&remote, "music").await;
    let here = TempDir::new("borrow_here");
    let to = here.join("copy");
    let (status, body) = borrow(&local, &root, &to, json!({"name": "music@remote"})).await;
    assert_eq!(status, StatusCode::OK, "borrow failed: {body}");

    let (status, body) = load(&remote, &root).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let message = body["error"].as_str().unwrap_or_default().to_string();
    assert!(message.contains("borrowed"), "{message}");
    assert!(message.contains(to.to_str().unwrap()), "names the copy: {message}");

    let third = app();
    let (status, body) = borrow(&third, &root, &here.join("again"), json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(!here.join("again").exists(), "nothing copied");
}

/// A repository its own daemon still holds is not borrowed: the daemon says so
/// in a holder file, which the flock cannot across a network mount.
#[tokio::test]
async fn a_repository_loaded_at_home_is_not_borrowed() {
    let (remote, local) = (app(), app());
    let (_uuid, root) = remote_repo(&remote, "docs").await;
    load_ready(&remote, &root).await;
    let here = TempDir::new("borrow_here");
    let to = here.join("copy");
    let (status, body) = borrow(&local, &root, &to, json!({"name": "docs@remote"})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let message = body["error"].as_str().unwrap_or_default().to_string();
    assert!(message.contains("loaded"), "{message}");
    assert!(!marker(&root).exists(), "the mark is taken back");
    assert!(!to.exists(), "nothing copied");
}

/// A holder file left by a crash refuses the borrow too, until `force`.
#[tokio::test]
async fn a_stale_holder_is_overridden_by_force() {
    let (remote, local) = (app(), app());
    let (_uuid, root) = remote_repo(&remote, "stale").await;
    let holder = root.join(".metafolder/internal/kv/daemon.holder");
    assert!(!holder.exists(), "an unload removes it");
    std::fs::write(&holder, r#"{"host": "elsewhere", "pid": 1, "since_ms": 0}"#).unwrap();
    let here = TempDir::new("borrow_here");
    let (status, body) = borrow(&local, &root, &here.join("copy"), json!({"name": "s@r"})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body["error"].as_str().unwrap_or_default().contains("elsewhere"), "{body}");
    let (status, body) =
        borrow(&local, &root, &here.join("copy"), json!({"name": "s@r", "force": true})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// A borrow whose load fails — its name taken here — leaves nothing behind:
/// no copy, no mark on the remote.
#[tokio::test]
async fn a_failed_borrow_leaves_nothing() {
    let (remote, local) = (app(), app());
    let (_uuid, root) = remote_repo(&remote, "taken").await;
    let (_other, _other_root) = remote_repo(&local, "taken").await;
    let other_root = _other_root.to_path_buf();
    load_ready(&local, &other_root).await;
    let here = TempDir::new("borrow_here");
    let to = here.join("copy");
    let (status, body) = borrow(&local, &root, &to, json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(!marker(&root).exists());
    assert!(!to.exists());
}

/// What was written in the copy goes back: the remote loads again, holds it,
/// keeps its backups, and the copy is gone.
#[tokio::test]
async fn giving_back_sends_the_store_home() {
    let (remote, local) = (app(), app());
    let (uuid, root) = remote_repo(&remote, "books").await;
    let backups = root.join(".metafolder/internal/backups/manual-1");
    std::fs::create_dir_all(&backups).unwrap();
    std::fs::write(backups.join("backup.json"), "{}").unwrap();
    let here = TempDir::new("borrow_here");
    let to = here.join("copy");
    let (status, body) = borrow(&local, &root, &to, json!({"name": "books@remote"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!to.join("internal/backups").exists(), "the backups stay at home");
    create(&local, &uuid, note("written in the copy")).await;

    let (status, body) = request(&local, "POST", &format!("/repos/{uuid}/give-back"), None).await;
    assert_eq!(status, StatusCode::OK, "give-back failed: {body}");
    let (status, _) = request(&local, "GET", &format!("/repos/{uuid}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unloaded here");
    assert!(!to.exists(), "the copy is gone");
    assert!(!marker(&root).exists(), "the remote is unlocked");
    assert!(backups.join("backup.json").is_file(), "its backups kept");

    let back = load_ready(&remote, &root).await;
    assert_eq!(back, uuid);
    assert_eq!(notes(&remote, &uuid).await, ["from the remote", "written in the copy"]);
    let (_, info) = request(&remote, "GET", &format!("/repos/{uuid}"), None).await;
    assert_eq!(info["name"], "books", "its own name");
    assert!(info["borrowed_from"].is_null(), "{info}");
}

/// A pair's sync state the borrowed repository hosts goes back with it.
#[tokio::test]
async fn a_hosted_sync_store_goes_back() {
    let (remote, local) = (app(), app());
    let (uuid, root) = remote_repo(&remote, "pair").await;
    let here = TempDir::new("borrow_here");
    let (status, body) = borrow(&local, &root, &here.join("copy"), json!({"name": "pair@r"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let mine = TempDir::new("borrow_mine");
    common::seed_config(&mine);
    let (_, body) =
        request(&local, "POST", "/repos/init", Some(json!({"root": mine.to_str().unwrap()}))).await;
    let other = body["repo_uuid"].as_str().unwrap().to_string();
    let a = create(&local, &uuid, json!([])).await;
    let b = create(&local, &other, json!([])).await;
    let (ra, rb) = if uuid < other { (a, b) } else { (b, a) };
    let (status, body) = request(
        &local,
        "POST",
        &format!("/sync/{uuid}/{other}/links"),
        Some(json!({"record_a": ra, "record_b": rb, "host": uuid})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = request(&local, "POST", &format!("/repos/{uuid}/give-back"), None).await;
    assert_eq!(status, StatusCode::OK, "give-back failed: {body}");
    let hosted = std::fs::read_dir(root.join(".metafolder/internal"))
        .unwrap()
        .flatten()
        .any(|e| e.file_name().to_string_lossy().starts_with("sync-"));
    assert!(hosted, "the pair's store is at home");
}

/// Giving back a repository that was never borrowed is refused; so is giving
/// back once the remote's mark no longer names this copy.
#[tokio::test]
async fn giving_back_needs_the_borrow() {
    let (remote, local) = (app(), app());
    let mine = TempDir::new("borrow_mine");
    common::seed_config(&mine);
    let (_, body) =
        request(&local, "POST", "/repos/init", Some(json!({"root": mine.to_str().unwrap()}))).await;
    let own = body["repo_uuid"].as_str().unwrap().to_string();
    let (status, body) = request(&local, "POST", &format!("/repos/{own}/give-back"), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let (uuid, root) = remote_repo(&remote, "broken").await;
    let here = TempDir::new("borrow_here");
    let to = here.join("copy");
    let (status, body) = borrow(&local, &root, &to, json!({"name": "broken@r"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // The borrow broken by hand at home, the repository used there since.
    std::fs::remove_file(marker(&root)).unwrap();
    let (status, body) = request(&local, "POST", &format!("/repos/{uuid}/give-back"), None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(to.join("internal/kv/data.mdb").is_file(), "the copy is kept");
}

/// The remote's `internal/`, inside its root, is never tracked by the copy —
/// only the copy's own `internal/` would be, by the ordinary rule.
#[tokio::test]
async fn the_remote_internal_dir_is_not_tracked() {
    let (remote, local) = (app(), app());
    let (uuid, root) = remote_repo(&remote, "tracked").await;
    std::fs::write(root.join("a.txt"), b"a").unwrap();
    let here = TempDir::new("borrow_here");
    let (status, body) = borrow(&local, &root, &here.join("copy"), json!({"name": "t@r"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, roots) = request(
        &local,
        "POST",
        &format!("/repos/{uuid}/query"),
        Some(json!({"query": {"type": "is_present", "field": "mf_watch"}})),
    )
    .await;
    let root_uuid = roots[0].as_str().unwrap().to_string();
    for (field, value) in [
        ("mf_ignore", json!({"type": "string", "value": "^$"})),
        ("mf_watch", json!({"type": "bool", "value": true})),
    ] {
        let (status, body) = request(
            &local,
            "PUT",
            &format!("/repos/{uuid}/metarecords/{root_uuid}/fields/{field}"),
            Some(json!({"value": value})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let (status, body) = request(&local, "POST", &format!("/repos/{uuid}/reconcile"), None).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let task = body["task_id"].as_str().unwrap().to_string();
    for _ in 0..400 {
        let (_, t) = request(&local, "GET", &format!("/repos/{uuid}/tasks/{task}"), None).await;
        if t["status"] == "done" {
            break;
        }
        assert_ne!(t["status"], "failed", "{t}");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let resolve = |path: &'static str| {
        let (local, uuid) = (local.clone(), uuid.clone());
        async move {
            request(
                &local,
                "POST",
                &format!("/repos/{uuid}/tree/resolve-path"),
                Some(json!({"field": "mfr_path", "path": path})),
            )
            .await
            .1["uuid"]
                .clone()
        }
    };
    assert!(resolve("/a.txt").await.is_string(), "the files are tracked");
    assert!(resolve("/.metafolder/config.json").await.is_string(), "so is .metafolder/");
    assert_eq!(resolve("/.metafolder/internal").await, Value::Null, "but not its internal/");
}

/// A give-back interrupted halfway leaves its temporary names on the remote
/// and the mark in place: giving back again completes it and clears them.
#[tokio::test]
async fn an_interrupted_give_back_completes_the_next_time() {
    let (remote, local) = (app(), app());
    let (uuid, root) = remote_repo(&remote, "again").await;
    let here = TempDir::new("borrow_here");
    let (status, body) = borrow(&local, &root, &here.join("copy"), json!({"name": "a@r"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    create(&local, &uuid, note("second")).await;
    let internal = root.join(".metafolder/internal");
    std::fs::create_dir_all(internal.join("kv.returning")).unwrap();
    std::fs::write(internal.join("kv.returning/data.mdb"), b"half").unwrap();
    std::fs::create_dir_all(internal.join("sync-gone.replaced")).unwrap();

    let (status, body) = request(&local, "POST", &format!("/repos/{uuid}/give-back"), None).await;
    assert_eq!(status, StatusCode::OK, "give-back failed: {body}");
    let left: Vec<String> = std::fs::read_dir(&internal)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".returning") || n.ends_with(".replaced"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
    load_ready(&remote, &root).await;
    assert_eq!(notes(&remote, &uuid).await, ["from the remote", "second"]);
}

/// A sync plan the copy hosts is a repository loaded from inside the copy:
/// the give-back unloads it, and does not send it home — the next
/// `mf sync plan` makes a new one.
#[tokio::test]
async fn a_plan_the_copy_hosts_is_unloaded_and_stays_behind() {
    let (remote, local) = (app(), app());
    let (uuid, root) = remote_repo(&remote, "planned").await;
    let here = TempDir::new("borrow_here");
    let to = here.join("copy");
    let (status, body) = borrow(&local, &root, &to, json!({"name": "p@r"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let plan_root = to.join("internal/plan-x-y");
    std::fs::create_dir_all(&plan_root).unwrap();
    common::seed_config(&plan_root);
    let (status, body) = request(
        &local,
        "POST",
        "/repos/init",
        Some(json!({"root": plan_root.to_str().unwrap(), "system": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let plan = body["repo_uuid"].as_str().unwrap().to_string();

    let (status, body) = request(&local, "POST", &format!("/repos/{uuid}/give-back"), None).await;
    assert_eq!(status, StatusCode::OK, "give-back failed: {body}");
    let (status, _) = request(&local, "GET", &format!("/repos/{plan}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "the plan is unloaded");
    assert!(!root.join(".metafolder/internal/plan-x-y").exists(), "and not sent home");
}
