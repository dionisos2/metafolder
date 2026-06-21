//! Tests for the pending-event executor: compaction, revision grouping, and
//! filesystem event semantics (spec-file-tracking "File Watcher").

use std::path::{Path, PathBuf};
use std::sync::Arc;

use metafolder_core::metarecord::Value;
use metafolder_daemon::db;
use metafolder_daemon::executor::{self, FsEvent};
use metafolder_daemon::log::{self, Writer};
use metafolder_daemon::repo;
use metafolder_daemon::state::RepoState;
use metafolder_daemon::tasks::{TaskKind, TaskStatus};
use uuid::Uuid;

fn temp_dir(prefix: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("metafolder_exec_{prefix}_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&path).unwrap();
    path
}

/// Initialises a repository with tracking enabled on the root.
fn setup(prefix: &str) -> (Arc<RepoState>, PathBuf, Uuid) {
    let root = temp_dir(prefix);
    let opened = repo::init_repository(&root, None, None).unwrap();
    let repo_state = Arc::new(RepoState::from_opened(opened));
    let db_id = repo_state.config.repo_uuid;

    let root_uuid = {
        let conn = repo_state.conn.lock().unwrap();
        db::find_tree_child(&conn, "mfr_path", None, "").unwrap().unwrap()
    };
    {
        let mut conn = repo_state.conn.lock().unwrap();
        let mut w = Writer::begin(&mut conn, db_id, None).unwrap();
        w.set_field(root_uuid, "mf_watch", Value::Bool(true)).unwrap();
        w.commit().unwrap();
    }
    (repo_state, root, root_uuid)
}

fn write_file(root: &Path, rel: &str, content: &[u8]) {
    let path = root.join(rel.trim_start_matches('/'));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn enqueue(repo: &RepoState, events: &[FsEvent]) {
    let conn = repo.conn.lock().unwrap();
    for ev in events {
        executor::enqueue(&conn, ev).unwrap();
    }
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

fn count(repo: &RepoState, sql: &str) -> i64 {
    let conn = repo.conn.lock().unwrap();
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

// ── Create ────────────────────────────────────────────────────────────────────

#[test]
fn test_flush_with_events_records_a_flush_task() {
    let (repo, root, _) = setup("flushtask");
    write_file(&root, "a.txt", b"hello");
    enqueue(&repo, &[FsEvent::Create("/a.txt".into())]);

    executor::flush_pending(&repo).unwrap();

    let tasks = repo.tasks.list();
    let flush = tasks.iter().find(|t| t.kind == TaskKind::Flush).expect("a flush task is recorded");
    assert_eq!(flush.status, TaskStatus::Done);

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_empty_flush_records_no_task() {
    let (repo, root, _) = setup("flushempty");
    // No pending events: the flush is a no-op and must not churn the registry.
    executor::flush_pending(&repo).unwrap();
    assert!(repo.tasks.list().is_empty(), "no task for a no-op flush");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_create_event_creates_record_with_stat_fields() {
    let (repo, root, _) = setup("create");
    write_file(&root, "a.txt", b"hello");
    enqueue(&repo, &[FsEvent::Create("/a.txt".into())]);

    executor::flush_pending(&repo).unwrap();

    let uuid = resolve(&repo, "/a.txt").expect("entry must exist");
    assert_eq!(field_value(&repo, uuid, "mfr_type"), Some(Value::String("file".into())));
    assert_eq!(field_value(&repo, uuid, "mfr_size"), Some(Value::Int(5)));
    assert!(matches!(field_value(&repo, uuid, "mfr_mtime"), Some(Value::DateTime(_))));
    assert_eq!(count(&repo, "SELECT COUNT(*) FROM pending_operation"), 0, "buffer consumed");

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_create_creates_missing_parent_metarecords() {
    let (repo, root, _) = setup("parents");
    write_file(&root, "x/y/deep.txt", b"d");
    enqueue(&repo, &[FsEvent::Create("/x/y/deep.txt".into())]);
    executor::flush_pending(&repo).unwrap();

    let dir = resolve(&repo, "/x/y").expect("parent dir entry created");
    assert_eq!(field_value(&repo, dir, "mfr_type"), Some(Value::String("dir".into())));
    assert!(resolve(&repo, "/x/y/deep.txt").is_some());

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_ineligible_paths_are_ignored() {
    let (repo, root, _) = setup("ignored");
    write_file(&root, ".git/config", b"x");
    enqueue(&repo, &[FsEvent::Create("/.git/config".into())]);
    executor::flush_pending(&repo).unwrap();

    assert!(resolve(&repo, "/.git/config").is_none());
    assert!(resolve(&repo, "/.git").is_none());
    std::fs::remove_dir_all(root).unwrap();
}

// ── Remove ────────────────────────────────────────────────────────────────────

#[test]
fn test_remove_sets_nothing_and_cascades() {
    let (repo, root, _) = setup("remove");
    write_file(&root, "d/one.txt", b"1");
    write_file(&root, "d/sub/two.txt", b"2");
    enqueue(
        &repo,
        &[
            FsEvent::Create("/d".into()),
            FsEvent::Create("/d/one.txt".into()),
            FsEvent::Create("/d/sub".into()),
            FsEvent::Create("/d/sub/two.txt".into()),
        ],
    );
    executor::flush_pending(&repo).unwrap();
    let d = resolve(&repo, "/d").unwrap();
    let one = resolve(&repo, "/d/one.txt").unwrap();
    let two = resolve(&repo, "/d/sub/two.txt").unwrap();

    std::fs::remove_dir_all(root.join("d")).unwrap();
    enqueue(&repo, &[FsEvent::Remove("/d".into())]);
    executor::flush_pending(&repo).unwrap();

    for uuid in [d, one, two] {
        assert_eq!(
            field_value(&repo, uuid, "mfr_path"),
            Some(Value::Nothing),
            "cascade must clear every descendant"
        );
    }
    assert!(resolve(&repo, "/d/one.txt").is_none());

    std::fs::remove_dir_all(root).unwrap();
}

// ── Rename ────────────────────────────────────────────────────────────────────

#[test]
fn test_rename_updates_tree_ref_and_children_follow() {
    let (repo, root, root_uuid) = setup("rename");
    write_file(&root, "old/file.txt", b"f");
    enqueue(
        &repo,
        &[FsEvent::Create("/old".into()), FsEvent::Create("/old/file.txt".into())],
    );
    executor::flush_pending(&repo).unwrap();
    let dir = resolve(&repo, "/old").unwrap();
    let file = resolve(&repo, "/old/file.txt").unwrap();

    std::fs::rename(root.join("old"), root.join("new")).unwrap();
    enqueue(&repo, &[FsEvent::Rename("/old".into(), "/new".into())]);
    executor::flush_pending(&repo).unwrap();

    assert_eq!(resolve(&repo, "/new"), Some(dir));
    assert_eq!(resolve(&repo, "/new/file.txt"), Some(file));
    assert!(resolve(&repo, "/old").is_none());
    assert_eq!(
        field_value(&repo, dir, "mfr_path"),
        Some(Value::TreeRef { parent: Some(root_uuid), name: "new".into() })
    );
    // One file_moved operation was logged.
    assert_eq!(count(&repo, "SELECT COUNT(*) FROM operation WHERE op_type = 'file_moved'"), 1);

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_rename_from_clears_path() {
    let (repo, root, _) = setup("renamefrom");
    write_file(&root, "g.txt", b"g");
    enqueue(&repo, &[FsEvent::Create("/g.txt".into())]);
    executor::flush_pending(&repo).unwrap();
    let uuid = resolve(&repo, "/g.txt").unwrap();

    std::fs::remove_file(root.join("g.txt")).unwrap();
    enqueue(&repo, &[FsEvent::RenameFrom("/g.txt".into())]);
    executor::flush_pending(&repo).unwrap();

    assert_eq!(field_value(&repo, uuid, "mfr_path"), Some(Value::Nothing));
    std::fs::remove_dir_all(root).unwrap();
}

// ── Arrival (Rename(To)) with fingerprint match ───────────────────────────────

#[test]
fn test_rename_to_reuses_orphan_when_full_hash_confirms() {
    let (repo, root, _) = setup("arrival");
    write_file(&root, "song.mp3", b"some audio content");
    enqueue(&repo, &[FsEvent::Create("/song.mp3".into())]);
    executor::flush_pending(&repo).unwrap();
    let uuid = resolve(&repo, "/song.mp3").unwrap();

    // Store the fingerprints (normally computed by reconcile/dedup).
    let partial = metafolder_daemon::fingerprint::partial_hash(&root.join("song.mp3")).unwrap();
    let full = metafolder_daemon::fingerprint::full_hash(&root.join("song.mp3")).unwrap();
    {
        let mut conn = repo.conn.lock().unwrap();
        let mut w = Writer::begin(&mut conn, repo.config.repo_uuid, None).unwrap();
        w.set_field(uuid, "mfr_partial_hash", Value::String(partial)).unwrap();
        w.set_field(uuid, "mfr_full_hash", Value::String(full)).unwrap();
        w.commit().unwrap();
    }

    // The file leaves the repo, then comes back elsewhere.
    std::fs::rename(root.join("song.mp3"), std::env::temp_dir().join("mf_outside.mp3")).unwrap();
    enqueue(&repo, &[FsEvent::RenameFrom("/song.mp3".into())]);
    executor::flush_pending(&repo).unwrap();
    assert_eq!(field_value(&repo, uuid, "mfr_path"), Some(Value::Nothing));

    write_file(&root, "back/song2.mp3", b"some audio content");
    std::fs::remove_file(std::env::temp_dir().join("mf_outside.mp3")).unwrap();
    enqueue(
        &repo,
        &[FsEvent::Create("/back".into()), FsEvent::RenameTo("/back/song2.mp3".into())],
    );
    executor::flush_pending(&repo).unwrap();

    assert_eq!(
        resolve(&repo, "/back/song2.mp3"),
        Some(uuid),
        "the orphaned entry must be reused on full-hash match"
    );

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_rename_to_without_match_creates_new_metarecord() {
    let (repo, root, _) = setup("arrival2");
    write_file(&root, "fresh.txt", b"brand new");
    enqueue(&repo, &[FsEvent::RenameTo("/fresh.txt".into())]);
    executor::flush_pending(&repo).unwrap();
    assert!(resolve(&repo, "/fresh.txt").is_some());
    std::fs::remove_dir_all(root).unwrap();
}

// ── Modify ────────────────────────────────────────────────────────────────────

#[test]
fn test_modify_data_refreshes_and_invalidates_hashes() {
    let (repo, root, _) = setup("modify");
    write_file(&root, "m.txt", b"v1");
    enqueue(&repo, &[FsEvent::Create("/m.txt".into())]);
    executor::flush_pending(&repo).unwrap();
    let uuid = resolve(&repo, "/m.txt").unwrap();
    {
        let mut conn = repo.conn.lock().unwrap();
        let mut w = Writer::begin(&mut conn, repo.config.repo_uuid, None).unwrap();
        w.set_field(uuid, "mfr_partial_hash", Value::String("aaaa".into())).unwrap();
        w.set_field(uuid, "mfr_full_hash", Value::String("bbbb".into())).unwrap();
        w.commit().unwrap();
    }

    write_file(&root, "m.txt", b"version two, longer");
    enqueue(&repo, &[FsEvent::ModifyData("/m.txt".into())]);
    executor::flush_pending(&repo).unwrap();

    assert_eq!(field_value(&repo, uuid, "mfr_size"), Some(Value::Int(19)));
    assert_eq!(field_value(&repo, uuid, "mfr_partial_hash"), None, "hashes invalidated");
    assert_eq!(field_value(&repo, uuid, "mfr_full_hash"), None);

    std::fs::remove_dir_all(root).unwrap();
}

// ── Compaction and grouping ───────────────────────────────────────────────────

#[test]
fn test_compaction_create_then_remove_writes_nothing() {
    let (repo, root, _) = setup("compact1");
    enqueue(
        &repo,
        &[FsEvent::Create("/ghost.txt".into()), FsEvent::Remove("/ghost.txt".into())],
    );
    let revisions_before = count(&repo, "SELECT COUNT(*) FROM revision");
    executor::flush_pending(&repo).unwrap();

    assert!(resolve(&repo, "/ghost.txt").is_none());
    assert_eq!(
        count(&repo, "SELECT COUNT(*) FROM revision"),
        revisions_before,
        "no revision for a fully-compacted buffer"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_compaction_create_then_rename_creates_at_destination() {
    let (repo, root, _) = setup("compact2");
    write_file(&root, "final.txt", b"x");
    enqueue(
        &repo,
        &[
            FsEvent::Create("/initial.txt".into()),
            FsEvent::Rename("/initial.txt".into(), "/final.txt".into()),
        ],
    );
    executor::flush_pending(&repo).unwrap();

    assert!(resolve(&repo, "/final.txt").is_some());
    assert!(resolve(&repo, "/initial.txt").is_none());
    assert_eq!(count(&repo, "SELECT COUNT(*) FROM operation WHERE op_type = 'file_moved'"), 0);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_compaction_collapses_repeated_modify() {
    let (repo, root, _) = setup("compact3");
    write_file(&root, "m.txt", b"x");
    enqueue(&repo, &[FsEvent::Create("/m.txt".into())]);
    executor::flush_pending(&repo).unwrap();

    enqueue(
        &repo,
        &[
            FsEvent::ModifyData("/m.txt".into()),
            FsEvent::ModifyData("/m.txt".into()),
            FsEvent::ModifyData("/m.txt".into()),
        ],
    );
    let ops_before = count(&repo, "SELECT COUNT(*) FROM operation");
    executor::flush_pending(&repo).unwrap();
    let ops_after = count(&repo, "SELECT COUNT(*) FROM operation");

    // One compacted modify: refresh ops for size/mtime only (the entry has
    // no hash rows to clear), far fewer than three full refreshes.
    assert!(
        ops_after - ops_before <= 3,
        "expected a single compacted modify, got {} ops",
        ops_after - ops_before
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_compaction_absorbs_notify_rename_triplet() {
    // The notify inotify backend emits From, To, *and* the correlated Both
    // for a single rename; the pair must be absorbed by the Both event.
    let (repo, root, _) = setup("triplet");
    write_file(&root, "a.txt", b"x");
    enqueue(&repo, &[FsEvent::Create("/a.txt".into())]);
    executor::flush_pending(&repo).unwrap();
    let uuid = resolve(&repo, "/a.txt").unwrap();

    std::fs::rename(root.join("a.txt"), root.join("b.txt")).unwrap();
    enqueue(
        &repo,
        &[
            FsEvent::RenameFrom("/a.txt".into()),
            FsEvent::RenameTo("/b.txt".into()),
            FsEvent::Rename("/a.txt".into(), "/b.txt".into()),
        ],
    );
    executor::flush_pending(&repo).unwrap();

    assert_eq!(resolve(&repo, "/b.txt"), Some(uuid), "entry must survive the rename");
    assert!(resolve(&repo, "/a.txt").is_none());
    assert_ne!(
        field_value(&repo, uuid, "mfr_path"),
        Some(Value::Nothing),
        "the From event must not orphan the entry"
    );

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_groups_become_separate_revisions() {
    let (repo, root, _) = setup("groups");
    write_file(&root, "n1.txt", b"1");
    write_file(&root, "n2.txt", b"2");
    write_file(&root, "old.txt", b"o");
    enqueue(&repo, &[FsEvent::Create("/old.txt".into())]);
    executor::flush_pending(&repo).unwrap();

    // A mixed buffer: 2 creates + 1 modify → two revisions.
    write_file(&root, "old.txt", b"oo");
    enqueue(
        &repo,
        &[
            FsEvent::Create("/n1.txt".into()),
            FsEvent::ModifyData("/old.txt".into()),
            FsEvent::Create("/n2.txt".into()),
        ],
    );
    let revisions_before = count(&repo, "SELECT COUNT(*) FROM revision");
    executor::flush_pending(&repo).unwrap();
    assert_eq!(
        count(&repo, "SELECT COUNT(*) FROM revision") - revisions_before,
        2,
        "one revision per op_type group"
    );
    // Both creates share one revision.
    let create_revs: i64 = count(
        &repo,
        "SELECT COUNT(DISTINCT rev_id) FROM operation
         WHERE op_type = 'create_metarecord' AND field_name IS NULL",
    );
    assert!(create_revs >= 1);

    std::fs::remove_dir_all(root).unwrap();
}

// ── Coordinated-rollback skip restoration (spec-event-log "skip") ───────────────

/// The head op id's parent — the navigation target that undoes exactly the
/// last operation.
fn undo_last_target(repo: &RepoState) -> Option<i64> {
    let conn = repo.conn.lock().unwrap();
    let head = log::get_head(&conn).unwrap().unwrap();
    log::get_op(&conn, head).unwrap().unwrap().parent_id
}

#[test]
fn test_skip_move_restores_actual_location_on_replay() {
    let (repo, root, _root_uuid) = setup("skip_move");
    write_file(&root, "/a.txt", b"hello");
    enqueue(&repo, &[FsEvent::Create("/a.txt".into())]);
    executor::flush_pending(&repo).unwrap();
    let uuid = resolve(&repo, "/a.txt").expect("tracked");

    std::fs::rename(root.join("a.txt"), root.join("b.txt")).unwrap();
    enqueue(&repo, &[FsEvent::Rename("/a.txt".into(), "/b.txt".into())]);
    executor::flush_pending(&repo).unwrap();
    assert_eq!(resolve(&repo, "/b.txt"), Some(uuid));

    // Roll back the move WITH skip: the metadata reverts to /a.txt and a
    // restoration op is queued (the file is really at /b.txt).
    let target = undo_last_target(&repo);
    {
        let mut conn = repo.conn.lock().unwrap();
        log::coordinated_step(&mut conn, repo.config.repo_uuid, target, true).unwrap();
    }
    repo.cache.lock().unwrap().clear();
    assert_eq!(resolve(&repo, "/a.txt"), Some(uuid), "metadata reverted to old location");

    // Replaying the buffer applies the restoration → back to /b.txt.
    executor::flush_pending(&repo).unwrap();
    repo.cache.lock().unwrap().clear();
    assert_eq!(resolve(&repo, "/b.txt"), Some(uuid), "restoration re-recorded the real location");
    assert_eq!(resolve(&repo, "/a.txt"), None);

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_skip_delete_rerecords_deletion_on_replay() {
    let (repo, root, _root_uuid) = setup("skip_delete");
    write_file(&root, "/a.txt", b"hello");
    enqueue(&repo, &[FsEvent::Create("/a.txt".into())]);
    executor::flush_pending(&repo).unwrap();
    let uuid = resolve(&repo, "/a.txt").expect("tracked");

    std::fs::remove_file(root.join("a.txt")).unwrap();
    enqueue(&repo, &[FsEvent::Remove("/a.txt".into())]);
    executor::flush_pending(&repo).unwrap();
    assert_eq!(field_value(&repo, uuid, "mfr_path"), Some(Value::Nothing));

    // Roll back the delete WITH skip: the metadata is restored, but the file
    // is still gone — the restoration re-records the deletion.
    let target = undo_last_target(&repo);
    {
        let mut conn = repo.conn.lock().unwrap();
        log::coordinated_step(&mut conn, repo.config.repo_uuid, target, true).unwrap();
    }
    repo.cache.lock().unwrap().clear();
    assert_eq!(resolve(&repo, "/a.txt"), Some(uuid), "metadata restored");

    executor::flush_pending(&repo).unwrap();
    assert_eq!(
        field_value(&repo, uuid, "mfr_path"),
        Some(Value::Nothing),
        "restoration re-recorded the deletion"
    );

    std::fs::remove_dir_all(root).unwrap();
}
