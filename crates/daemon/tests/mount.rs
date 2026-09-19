//! Tests for mount points (spec-file-tracking "Mount points"): a directory
//! carrying `mfr_mount` that is *not* a mount point right now is offline, and
//! its subtree is frozen — invisible to the reconcile walk, to the fingerprint
//! phase, to the orphan scan and to the watcher's watch placement.
//!
//! An ordinary directory is never a mount point, so the offline state (the one
//! that matters for data safety) is reproducible without root: mark a plain
//! directory with `mfr_mount` and it *is* an unplugged volume as far as every
//! component is concerned. The positive side — detecting a real mount and
//! writing the field — is covered by the unit tests of `mount` / `fs_meta`.

use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use metafolder_core::metarecord::Value;
use metafolder_daemon::executor::{self, FsEvent};
use metafolder_daemon::log::Writer;
use metafolder_daemon::mount::{self, MountState};
use metafolder_daemon::state::{AppState, RepoState};
use metafolder_daemon::{db, orphans, reconcile, repo, routes, watcher};
use tower::util::ServiceExt;
use uuid::Uuid;

mod common;
use common::TempDir;

const DEFAULT_PATTERNS: &[&str] = &[r"\.metafolder(/.*)?$", r"(^|/)\.[^/]+"];

fn setup(prefix: &str) -> (Arc<RepoState>, TempDir) {
    let root = TempDir::new(&format!("mount_{prefix}"));
    let opened = repo::init_repository(&root, None, None, false).unwrap();
    let repo_state = Arc::new(RepoState::from_opened(opened));
    let root_uuid = {
        let conn = repo_state.conn.lock().unwrap();
        db::find_tree_child(&conn, "mfr_path", None, "").unwrap().unwrap()
    };
    {
        let mut conn = repo_state.conn.lock().unwrap();
        let mut w = Writer::begin(&mut conn, None).unwrap();
        w.set_field(root_uuid, "mf_watch", Value::Bool(true)).unwrap();
        for pattern in DEFAULT_PATTERNS {
            w.append_field(root_uuid, "mf_ignore", Value::String((*pattern).into())).unwrap();
        }
        w.commit().unwrap();
    }
    (repo_state, root)
}

fn write_file(root: &Path, rel: &str, content: &[u8]) {
    let path = root.join(rel.trim_start_matches('/'));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn resolve(repo: &RepoState, path: &str) -> Option<Uuid> {
    let conn = repo.conn.lock().unwrap();
    let mut cache = repo.cache.lock().unwrap();
    cache.resolve_path(&conn, "mfr_path", path).unwrap()
}

fn field_value(repo: &RepoState, uuid: Uuid, name: &str) -> Option<Value> {
    let conn = repo.conn.lock().unwrap();
    db::get_metarecord(&conn, uuid).unwrap().unwrap().get(name).cloned()
}

/// Marks `dir_uuid` as the mount point of a volume that is not plugged in: the
/// directory is an ordinary one, so it can never be a live mount point.
fn declare_mount(repo: &RepoState, dir_uuid: Uuid, identity: &str) {
    let mut conn = repo.conn.lock().unwrap();
    let mut w = Writer::begin(&mut conn, None).unwrap();
    w.set_field(dir_uuid, mount::FIELD, Value::String(identity.into())).unwrap();
    w.commit().unwrap();
}

#[test]
fn reconcile_prunes_the_subtree_of_an_offline_mount_point() {
    let (repo, root) = setup("prune");
    write_file(&root, "/vol/a.txt", b"content");
    write_file(&root, "/kept.txt", b"x");
    reconcile::reconcile(&repo).unwrap();
    let vol = resolve(&repo, "/vol").expect("the mount point directory is tracked");
    let a = resolve(&repo, "/vol/a.txt").expect("its content was tracked while mounted");

    declare_mount(&repo, vol, "uuid:1234-ABCD");

    // Anything appearing below an offline mount point is not ours to record:
    // the volume is not there, so the walk must not descend at all.
    write_file(&root, "/vol/appeared.txt", b"y");
    write_file(&root, "/also-kept.txt", b"z");
    let result = reconcile::reconcile(&repo).unwrap();

    assert_eq!(resolve(&repo, "/vol/appeared.txt"), None, "walked into an offline mount");
    assert!(resolve(&repo, "/also-kept.txt").is_some(), "the rest of the repo still reconciles");
    assert_eq!(result.created, 1, "only the file outside the mount point");
    // The frozen records keep everything they had.
    assert!(matches!(field_value(&repo, a, "mfr_path"), Some(Value::TreeRef { .. })));
    assert!(resolve(&repo, "/vol/a.txt").is_some());
}

#[test]
fn reconcile_never_offers_a_frozen_record_as_a_move_candidate() {
    let (repo, root) = setup("candidate");
    write_file(&root, "/vol/a.txt", b"1234567890");
    reconcile::reconcile(&repo).unwrap();
    let vol = resolve(&repo, "/vol").unwrap();
    let a = resolve(&repo, "/vol/a.txt").unwrap();

    declare_mount(&repo, vol, "label:BACKUP");

    // The volume is unplugged: its files are gone from the filesystem. A
    // same-sized file elsewhere must *not* be proposed as where a.txt went.
    std::fs::remove_file(root.join("vol/a.txt")).unwrap();
    write_file(&root, "/downloads/other.txt", b"0987654321");
    let result = reconcile::reconcile(&repo).unwrap();

    assert!(result.candidates.is_empty(), "frozen record proposed as moved: {result:?}");
    assert_eq!(result.moved, 0);
    assert!(resolve(&repo, "/vol/a.txt").is_some(), "the frozen record kept its path");
    assert_eq!(a, resolve(&repo, "/vol/a.txt").unwrap());
}

#[test]
fn orphan_scan_never_reports_a_path_under_an_offline_mount() {
    let (repo, root) = setup("orphan");
    write_file(&root, "/vol/a.txt", b"content");
    write_file(&root, "/plain/b.txt", b"content");
    reconcile::reconcile(&repo).unwrap();
    let vol = resolve(&repo, "/vol").unwrap();
    let a = resolve(&repo, "/vol/a.txt").unwrap();
    let b = resolve(&repo, "/plain/b.txt").unwrap();

    declare_mount(&repo, vol, "uuid:1234-ABCD");
    // Both files are absent from a readable, existing parent directory: the
    // only thing telling them apart is the mount point above one of them.
    std::fs::remove_file(root.join("vol/a.txt")).unwrap();
    std::fs::remove_file(root.join("plain/b.txt")).unwrap();

    let reported: Vec<Uuid> =
        orphans::scan_orphans(&repo).unwrap().into_iter().map(|o| o.uuid).collect();
    assert!(reported.contains(&b), "an ordinary deleted file is still an orphan");
    assert!(!reported.contains(&a), "an unplugged volume must never mass-orphan a subtree");

    // …and `clear` re-verifies, so even an explicit uuid is refused.
    let cleared = orphans::clear_orphans(&repo, &[a]).unwrap();
    assert_eq!(cleared, 0);
    assert!(matches!(field_value(&repo, a, "mfr_path"), Some(Value::TreeRef { .. })));
}

#[test]
fn the_watcher_places_no_watch_inside_an_offline_mount() {
    let (repo, root) = setup("watch");
    write_file(&root, "/vol/sub/a.txt", b"content");
    write_file(&root, "/plain/sub/b.txt", b"content");
    reconcile::reconcile(&repo).unwrap();
    let vol = resolve(&repo, "/vol").unwrap();
    declare_mount(&repo, vol, "uuid:1234-ABCD");

    let internal = repo.internal_dir();
    let dirs = {
        let conn = repo.conn.lock().unwrap();
        let mut cache = repo.cache.lock().unwrap();
        watcher::compute_watched_dirs_timed(&conn, &mut cache, &root, &internal, None).dirs
    };

    assert!(dirs.contains(&root.path().to_path_buf()));
    assert!(dirs.contains(&root.join("plain")));
    assert!(dirs.contains(&root.join("plain/sub")));
    assert!(!dirs.contains(&root.join("vol")), "watched an offline mount point");
    assert!(!dirs.contains(&root.join("vol/sub")), "watched inside an offline mount point");
}

#[test]
fn declared_mount_points_report_their_state_and_expected_volume() {
    let (repo, root) = setup("declared");
    write_file(&root, "/vol/a.txt", b"content");
    reconcile::reconcile(&repo).unwrap();
    let vol = resolve(&repo, "/vol").unwrap();
    declare_mount(&repo, vol, "uuid:1234-ABCD");

    let mounts = {
        let conn = repo.conn.lock().unwrap();
        let mut cache = repo.cache.lock().unwrap();
        mount::declared(&conn, &mut cache, &root).unwrap()
    };
    assert_eq!(mounts.len(), 1);
    let m = &mounts[0];
    assert_eq!(m.uuid, vol);
    assert_eq!(m.path.as_deref(), Some("/vol"));
    assert_eq!(m.expected, "uuid:1234-ABCD");
    assert_eq!(m.current, None);
    assert_eq!(m.state, MountState::Offline);
}

#[test]
fn the_executor_drops_an_event_landing_in_an_offline_mount() {
    let (repo, root) = setup("events");
    write_file(&root, "/vol/a.txt", b"content");
    write_file(&root, "/plain/b.txt", b"content");
    reconcile::reconcile(&repo).unwrap();
    let vol = resolve(&repo, "/vol").unwrap();
    let a = resolve(&repo, "/vol/a.txt").unwrap();
    let b = resolve(&repo, "/plain/b.txt").unwrap();
    declare_mount(&repo, vol, "uuid:1234-ABCD");

    // A stale watch, a replayed buffer, or an event the kernel delivered as the
    // volume went away: the removal must not orphan the frozen record, while
    // the identical event outside the mount point still applies.
    std::fs::remove_file(root.join("vol/a.txt")).unwrap();
    std::fs::remove_file(root.join("plain/b.txt")).unwrap();
    executor::enqueue(&repo, FsEvent::Remove("/vol/a.txt".into()), None);
    executor::enqueue(&repo, FsEvent::Remove("/plain/b.txt".into()), None);
    executor::flush_pending(&repo).unwrap();

    assert!(
        matches!(field_value(&repo, a, "mfr_path"), Some(Value::TreeRef { .. })),
        "an unplugged volume's file was orphaned by a watcher event"
    );
    assert_eq!(field_value(&repo, b, "mfr_path"), Some(Value::Nothing));
}

// ── Mount status while the repository is busy ────────────────────────────────
//
// A long write — a reconcile, a big watcher flush — holds *both* the database
// connection and the tree cache for its whole transaction (minutes, on a large
// repository). `GET /repos/:repo/mounts` used to take the two blocking, and the
// GUI asks for them on every directory listing: measured behind a full reconcile
// on a 50 k-metarecord repository, one call took 151 385 ms, all of it in
// `wait:conn`.
//
// Waiting buys nothing. What the request needs from the database is the
// *declared set* — which metarecords carry `mfr_mount`, and where they sit —
// and a writer in flight has committed none of its changes, so the set as the
// reader last saw it *is* the committed one. The volatile half (is the volume
// plugged in right now?) is read from the disk on every request either way.

/// Initialises a repository inside an `AppState`, with the watch flag and the
/// default ignore patterns of [`setup`], and returns the router beside it.
fn setup_app(prefix: &str) -> (Router, Arc<AppState>, Arc<RepoState>, String, TempDir) {
    let root = TempDir::new(&format!("mount_{prefix}"));
    let state = Arc::new(AppState::new());
    let uuid = state.init_repo(&root, None, None, false).unwrap();
    let repo = state.repo(uuid).unwrap();
    let root_uuid = {
        let conn = repo.conn.lock().unwrap();
        db::find_tree_child(&conn, "mfr_path", None, "").unwrap().unwrap()
    };
    {
        let mut conn = repo.conn.lock().unwrap();
        let mut w = Writer::begin(&mut conn, None).unwrap();
        w.set_field(root_uuid, "mf_watch", Value::Bool(true)).unwrap();
        for pattern in DEFAULT_PATTERNS {
            w.append_field(root_uuid, "mf_ignore", Value::String((*pattern).into())).unwrap();
        }
        w.commit().unwrap();
    }
    (routes::build(state.clone()), state, repo, uuid.as_simple().to_string(), root)
}

/// `GET /repos/:repo/mounts`, answered or timed out.
async fn get_mounts(app: &Router, repo: &str) -> Option<serde_json::Value> {
    let request = Request::builder()
        .method("GET")
        .uri(format!("/repos/{repo}/mounts"))
        .body(Body::empty())
        .unwrap();
    let response =
        tokio::time::timeout(std::time::Duration::from_secs(5), app.clone().oneshot(request))
            .await
            .ok()?
            .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes =
        tokio::time::timeout(std::time::Duration::from_secs(5), response.into_body().collect())
            .await
            .ok()?
            .unwrap()
            .to_bytes();
    Some(serde_json::from_slice(&bytes).unwrap())
}

/// Holds the connection and the tree cache from another thread, exactly as a
/// running reconcile does. Dropping the returned handle releases them.
struct Busy {
    release: std::sync::mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Busy {
    fn hold(repo: Arc<RepoState>) -> Busy {
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            let _conn = repo.conn.lock().unwrap();
            let _cache = repo.cache.lock().unwrap();
            locked_tx.send(()).unwrap();
            let _ = release_rx.recv();
        });
        locked_rx.recv().unwrap();
        Busy { release, thread: Some(thread) }
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        let _ = self.release.send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[tokio::test]
async fn mount_status_answers_while_a_write_holds_the_repository() {
    let (app, _state, repo, repo_id, root) = setup_app("busy");
    write_file(&root, "/vol/a.txt", b"content");
    reconcile::reconcile(&repo).unwrap();
    let vol = resolve(&repo, "/vol").unwrap();
    declare_mount(&repo, vol, "uuid:1234-ABCD");

    // A first call with the repository free: the ordinary path.
    let free = get_mounts(&app, &repo_id).await.expect("the free call must answer");
    assert_eq!(free["mounts"].as_array().unwrap().len(), 1, "{free}");

    let _busy = Busy::hold(repo.clone());
    let body = get_mounts(&app, &repo_id)
        .await
        .expect("GET /mounts must not queue behind a running write");
    let mounts = body["mounts"].as_array().unwrap();
    assert_eq!(mounts.len(), 1, "the declared set must still be served: {body}");
    assert_eq!(mounts[0]["path"], "/vol");
    assert_eq!(mounts[0]["expected"], "uuid:1234-ABCD");
    // The disk half is probed on every request, busy or not.
    assert_eq!(mounts[0]["state"], "offline");
}

#[tokio::test]
async fn mount_status_answers_on_a_freshly_loaded_repository_that_is_busy() {
    let (app, state, repo, repo_id, root) = setup_app("busy_cold");
    write_file(&root, "/vol/a.txt", b"content");
    reconcile::reconcile(&repo).unwrap();
    let vol = resolve(&repo, "/vol").unwrap();
    declare_mount(&repo, vol, "label:PHOTOS");
    drop(repo);

    // Reload it, so nothing has ever read the mount points on this repo state:
    // the load must leave them resident, or the first listing of a repository
    // opened while a reconcile runs loses the distinction altogether.
    state.unload_repo(Uuid::parse_str(&repo_id).unwrap()).unwrap();
    let uuid = state
        .load_repo(metafolder_daemon::repo::RepoLocator::Root(root.path().to_path_buf()))
        .unwrap();
    let repo = state.repo(uuid).unwrap();
    repo.warm(&|_, _, _| {}).unwrap();

    let _busy = Busy::hold(repo.clone());
    let body = get_mounts(&app, &uuid.as_simple().to_string())
        .await
        .expect("GET /mounts must not queue behind a running write");
    let mounts = body["mounts"].as_array().unwrap();
    assert_eq!(mounts.len(), 1, "the load must leave the declared set resident: {body}");
    assert_eq!(mounts[0]["expected"], "label:PHOTOS");
}
