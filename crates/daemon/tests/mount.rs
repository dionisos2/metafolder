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

use metafolder_core::metarecord::Value;
use metafolder_daemon::executor::{self, FsEvent};
use metafolder_daemon::log::Writer;
use metafolder_daemon::mount::{self, MountState};
use metafolder_daemon::state::RepoState;
use metafolder_daemon::{db, orphans, reconcile, repo, watcher};
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
        watcher::compute_watched_dirs_timed(&conn, &mut cache, &root, &internal).0
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
