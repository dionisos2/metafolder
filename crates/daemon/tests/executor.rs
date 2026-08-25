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
        executor::enqueue(&conn, ev, None).unwrap();
    }
}

/// Enqueues `(event, cookie)` pairs, modelling notify's per-rename inotify
/// cookie so the executor can correlate a split From/To pair.
fn enqueue_tracked(repo: &RepoState, events: &[(FsEvent, Option<i64>)]) {
    let conn = repo.conn.lock().unwrap();
    for (ev, tracker) in events {
        executor::enqueue(&conn, ev, *tracker).unwrap();
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
fn test_create_directory_scans_its_existing_contents() {
    // The classic inotify recursive-watch race: a directory pasted in wholesale
    // arrives as one Create for the directory, but its contents already existed
    // before a recursive watch could be registered, so their own events are
    // lost. The executor must scan a newly-created directory and track what is
    // already inside it (spec-file-tracking "File Watcher").
    let (repo, root, _) = setup("dirscan");
    write_file(&root, "backup/a.txt", b"a");
    write_file(&root, "backup/sub/b.txt", b"bb");
    // Only the top directory's Create is delivered — the children events are lost.
    enqueue(&repo, &[FsEvent::Create("/backup".into())]);
    executor::flush_pending(&repo).unwrap();

    assert!(resolve(&repo, "/backup").is_some(), "the directory itself");
    let a = resolve(&repo, "/backup/a.txt").expect("top-level child tracked");
    assert_eq!(field_value(&repo, a, "mfr_size"), Some(Value::Int(1)));
    assert!(resolve(&repo, "/backup/sub").is_some(), "nested dir tracked");
    let b = resolve(&repo, "/backup/sub/b.txt").expect("nested child tracked");
    assert_eq!(field_value(&repo, b, "mfr_size"), Some(Value::Int(2)));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_ineligible_paths_are_ignored() {
    let (repo, root, root_uuid) = setup("ignored");
    // The daemon writes no default mf_ignore any more (patterns come from the
    // client-side `default` preset); set the `.git` pattern this test relies on.
    {
        let mut conn = repo.conn.lock().unwrap();
        let mut w = Writer::begin(&mut conn, None).unwrap();
        w.append_field(root_uuid, "mf_ignore", Value::String(r"\.git(/.*)?$".into())).unwrap();
        w.commit().unwrap();
    }
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

#[test]
fn test_remove_records_mfr_path_old_for_the_whole_subtree() {
    // Orphaning a subtree snapshots each metarecord's last real path into
    // `mfr_path_old` (a frozen String) so the origin of every orphan is legible
    // directly on the record. Captured only on the transition to Nothing.
    let (repo, root, _) = setup("path_old");
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

    for (uuid, path) in [(d, "/d"), (one, "/d/one.txt"), (two, "/d/sub/two.txt")] {
        assert_eq!(
            field_value(&repo, uuid, "mfr_path"),
            Some(Value::Nothing),
            "the record must be orphaned"
        );
        assert_eq!(
            field_value(&repo, uuid, "mfr_path_old"),
            Some(Value::String(path.into())),
            "mfr_path_old must snapshot the pre-orphan path"
        );
    }

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
    // A plain move must NOT touch mfr_path_old: it is captured only on the
    // transition to Nothing (orphaning), not on every rename.
    assert_eq!(field_value(&repo, dir, "mfr_path_old"), None);
    assert_eq!(field_value(&repo, file, "mfr_path_old"), None);

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_split_rename_with_cookie_is_one_move_not_delete_plus_arrival() {
    // notify failed to correlate the rename and delivered RenameFrom and
    // RenameTo separately, but tagged both with the same inotify cookie. The
    // executor must fuse them back into one move (not delete + arrival).
    let (repo, root, root_uuid) = setup("split_rename");
    write_file(&root, "old/file.txt", b"f");
    enqueue(
        &repo,
        &[FsEvent::Create("/old".into()), FsEvent::Create("/old/file.txt".into())],
    );
    executor::flush_pending(&repo).unwrap();
    let dir = resolve(&repo, "/old").unwrap();
    let file = resolve(&repo, "/old/file.txt").unwrap();

    std::fs::rename(root.join("old"), root.join("new")).unwrap();
    enqueue_tracked(
        &repo,
        &[
            (FsEvent::RenameFrom("/old".into()), Some(9)),
            (FsEvent::RenameTo("/new".into()), Some(9)),
        ],
    );
    executor::flush_pending(&repo).unwrap();

    // Same metarecord, moved; children follow — exactly as a native Both would.
    assert_eq!(resolve(&repo, "/new"), Some(dir));
    assert_eq!(resolve(&repo, "/new/file.txt"), Some(file));
    assert!(resolve(&repo, "/old").is_none());
    assert_eq!(
        field_value(&repo, dir, "mfr_path"),
        Some(Value::TreeRef { parent: Some(root_uuid), name: "new".into() })
    );
    // One file_moved op, and crucially no delete (no Nothing was written).
    assert_eq!(count(&repo, "SELECT COUNT(*) FROM operation WHERE op_type = 'file_moved'"), 1);
    assert_eq!(count(&repo, "SELECT COUNT(*) FROM operation WHERE op_type = 'file_deleted'"), 0);

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
        let mut w = Writer::begin(&mut conn, None).unwrap();
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
        let mut w = Writer::begin(&mut conn, None).unwrap();
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
        log::coordinated_step(&mut conn, target, true).unwrap();
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
        log::coordinated_step(&mut conn, target, true).unwrap();
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

// ── Idempotent refresh (spec-sync echo suppression) ─────────────────────────

#[test]
fn test_modify_data_on_unchanged_file_is_idempotent() {
    // A Modify(Data) event for a file whose stat did not change (e.g. the
    // watcher's echo of a change the daemon itself just recorded) must produce
    // no operation and no version bump — the executor's data refresh is
    // idempotent (spec-sync "Suppressing sync's own echoes").
    let (repo, root, _) = setup("idempotent_refresh");
    write_file(&root, "a.txt", b"hello");
    enqueue(&repo, &[FsEvent::Create("/a.txt".into())]);
    executor::flush_pending(&repo).unwrap();

    let uuid = resolve(&repo, "/a.txt").expect("file tracked after create");
    let v0 = {
        let conn = repo.conn.lock().unwrap();
        db::get_version(&conn, uuid).unwrap()
    };

    // The file is untouched on disk; its stored stat already matches.
    enqueue(&repo, &[FsEvent::ModifyData("/a.txt".into())]);
    executor::flush_pending(&repo).unwrap();

    let v1 = {
        let conn = repo.conn.lock().unwrap();
        db::get_version(&conn, uuid).unwrap()
    };
    assert_eq!(v0, v1, "an unchanged file must not bump the version");
    assert_eq!(
        count(&repo, "SELECT COUNT(*) FROM operation WHERE op_type = 'file_modified'"),
        0,
        "no file_modified operation for an unchanged file"
    );

    std::fs::remove_dir_all(root).unwrap();
}

// ── Resilience ────────────────────────────────────────────────────────────────

// A batch the executor cannot apply must not switch tracking off for good.
// The pending buffer is persistent so a crash loses no event, which also means
// a batch that always fails is retried for ever — and while it is stuck, no
// filesystem event is ever recorded again for this repository, not even after a
// restart (the buffer is replayed at load). After a bounded number of attempts
// the batch is dropped: those events are lost (a reconcile recovers them) but
// the watcher lives.
#[test]
fn test_an_unapplicable_batch_is_dropped_instead_of_stopping_the_watcher() {
    let (repo, root, _) = setup("poison");

    // A buffered event the executor cannot even parse: every flush fails on it.
    {
        let conn = repo.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO pending_operation (op_type, path) VALUES ('fs_bogus', '/nowhere')",
            [],
        )
        .unwrap();
    }
    write_file(&root, "a.txt", b"a");
    enqueue(&repo, &[FsEvent::Create("/a.txt".into())]);

    // The batch is retried while the budget lasts…
    for attempt in 1..=executor::FLUSH_FAILURE_BUDGET {
        assert!(
            executor::flush_pending(&repo).is_err(),
            "attempt {attempt} must fail on the unapplicable batch",
        );
    }
    assert!(resolve(&repo, "/a.txt").is_none(), "nothing was applied while it failed");

    // …then it is dropped, and the watcher records again.
    write_file(&root, "b.txt", b"b");
    enqueue(&repo, &[FsEvent::Create("/b.txt".into())]);
    executor::flush_pending(&repo).expect("the buffer is clear again");
    assert!(resolve(&repo, "/b.txt").is_some(), "tracking must resume once the batch is dropped");

    std::fs::remove_dir_all(root).unwrap();
}
