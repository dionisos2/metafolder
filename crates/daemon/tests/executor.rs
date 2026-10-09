//! Tests for the pending-event executor: compaction, revision grouping, and
//! filesystem event semantics (doc "Watcher").

use std::path::Path;
use std::sync::Arc;

use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::executor::{self, FsEvent};
use metafolder_daemon::log::{self, Writer};
use metafolder_daemon::repo;
use metafolder_daemon::state::RepoState;
use metafolder_daemon::tasks::{TaskKind, TaskStatus};
use uuid::Uuid;

mod common;
use common::TempDir;

/// Initialises a repository with tracking enabled on the root. The returned
/// directory removes itself when it goes out of scope, panic included.
fn setup(prefix: &str) -> (Arc<RepoState>, TempDir, Uuid) {
    let root = TempDir::new(&format!("exec_{prefix}"));
    let opened = repo::init_repository(&root, None, None, false).unwrap();
    let repo_state = Arc::new(RepoState::from_opened(opened));

    let root_uuid = {
        let conn = repo_state.conn.lock().unwrap();
        metafolder_daemon::store::Rows::child_by_bytes(&*conn, "mfr_path", None, b"")
            .unwrap()
            .unwrap()
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
    for ev in events {
        executor::enqueue(repo, ev.clone(), None);
    }
}

/// Enqueues `(event, cookie)` pairs, modelling notify's per-rename inotify
/// cookie so the executor can correlate a split From/To pair.
fn enqueue_tracked(repo: &RepoState, events: &[(FsEvent, Option<i64>)]) {
    for (ev, tracker) in events {
        executor::enqueue(repo, ev.clone(), *tracker);
    }
}

fn resolve(repo: &RepoState, path: &str) -> Option<Uuid> {
    let conn = repo.conn.lock().unwrap();
    let cache = repo.tree();
    cache.resolve_path(&conn, "mfr_path", path).unwrap()
}

fn field_value(repo: &RepoState, uuid: Uuid, name: &str) -> Option<Value> {
    let conn = repo.conn.lock().unwrap();
    metafolder_daemon::store::Rows::metarecord(&*conn, uuid).unwrap().unwrap().get(name).cloned()
}

/// How many operations the log holds, of `op_type` when given.
fn op_count(repo: &RepoState, op_type: Option<&str>) -> i64 {
    let conn = repo.conn.lock().unwrap();
    match op_type {
        None => metafolder_daemon::store::Log::counts(&*conn).unwrap().0,
        Some(t) => metafolder_daemon::store::Log::all_ops(&*conn)
            .unwrap()
            .iter()
            .filter(|op| op.op_type == t)
            .count() as i64,
    }
}

/// How many revisions the log holds.
fn revision_count(repo: &RepoState) -> i64 {
    metafolder_daemon::store::Log::counts(&*repo.conn.lock().unwrap()).unwrap().1
}

/// The newest revision (the one HEAD's operation belongs to) and its origin.
fn newest_revision(store: &dyn metafolder_daemon::store::Store) -> (i64, Option<String>) {
    let head = store.head().unwrap().unwrap();
    let rev_id = store.op(head).unwrap().unwrap().rev_id;
    let origin = store.revisions(&[rev_id]).unwrap().remove(&rev_id).unwrap().origin;
    (rev_id, origin)
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

/// Reads the diagnostics feed from `since`, keeping the entries whose message
/// mentions `needle` — the feed is process-wide and the test binary is
/// parallel, so a test recognises its own lines by their content.
fn diagnostics_about(since: u64, needle: &str) -> Vec<String> {
    metafolder_daemon::diagnostics::read(since, 1000)
        .entries
        .into_iter()
        .filter(|e| e.scope == "executor" && e.message.contains(needle))
        .map(|e| e.message)
        .collect()
}

fn diagnostics_head() -> u64 {
    metafolder_daemon::diagnostics::read(0, 1000).next_since
}

#[test]
fn test_a_flush_reports_what_it_did_to_the_diagnostics_feed() {
    let (repo, root, _) = setup("flushreport");
    write_file(&root, "reported.txt", b"hello");
    let since = diagnostics_head();
    enqueue(&repo, &[FsEvent::Create("/reported.txt".into())]);

    executor::flush_pending(&repo).unwrap();

    let lines = diagnostics_about(since, "/reported.txt");
    assert_eq!(lines.len(), 1, "one summary line per flush, got {lines:?}");
    let line = &lines[0];
    assert!(line.contains("1 event"), "{line}");
    assert!(line.contains("1 revision"), "{line}");
    assert!(line.contains("create /reported.txt"), "{line}");
}

#[test]
fn test_a_flush_that_writes_nothing_says_what_it_ignored() {
    // The user-visible question this answers: why is there a flush when
    // nothing happened? Because something *did* happen, to an ignored path.
    let (repo, root, root_uuid) = setup("flushignored");
    {
        let mut conn = repo.conn.lock().unwrap();
        let mut w = Writer::begin(&mut conn, None).unwrap();
        w.append_field(root_uuid, "mf_ignore", Value::String(r"\.git(/.*)?$".into())).unwrap();
        w.commit().unwrap();
    }
    write_file(&root, ".git/ignored-here", b"x");
    let since = diagnostics_head();
    enqueue(&repo, &[FsEvent::Create("/.git/ignored-here".into())]);

    executor::flush_pending(&repo).unwrap();

    let lines = diagnostics_about(since, "/.git/ignored-here");
    assert_eq!(lines.len(), 1, "one summary line per flush, got {lines:?}");
    assert!(lines[0].contains("nothing written (1 ignored)"), "{}", lines[0]);
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
    assert_eq!(executor::pending_count(&repo), 0, "buffer consumed");

    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn test_a_created_file_whose_name_is_not_utf8_is_tracked_live() {
    // The watcher used to skip such an event, leaving the file untracked until
    // the next reconcile. It is ingested like any other (doc "Tree names"),
    // and the metarecord carries the exact bytes.
    use metafolder_core::metarecord::TreeName;
    use metafolder_daemon::relpath::RelPath;
    use std::os::unix::ffi::OsStrExt;

    let (repo, root, _) = setup("non-utf8");
    let name = std::ffi::OsStr::from_bytes(b"caf\xe9.mp4");
    std::fs::write(root.path().join(name), b"movie").unwrap();

    let rel = RelPath::root().child(TreeName::from_bytes(b"caf\xe9.mp4".to_vec()));
    enqueue(&repo, &[FsEvent::Create(rel)]);
    executor::flush_pending(&repo).unwrap();

    let uuid = resolve(&repo, "/caf%E9.mp4").expect("the file must be tracked");
    assert_eq!(field_value(&repo, uuid, "mfr_size"), Some(Value::Int(5)));
    let Some(Value::TreeRef { name, .. }) = field_value(&repo, uuid, "mfr_path") else {
        panic!("mfr_path is not a tree_ref");
    };
    assert_eq!(name.as_bytes(), b"caf\xe9.mp4");

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
    // already inside it (doc "Watcher").
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
fn test_a_directory_removed_and_made_again_orphans_what_is_gone_under_it() {
    // `rm -r d && mkdir d` within one batch, as a source that covers the whole
    // filesystem reports it: the removal of `d` alone — the entries inside it
    // name a parent that no longer resolves by the time they are read — then
    // the new directory. The name is back, so `d` keeps its record; what was
    // under it and is gone must not stay tracked.
    let (repo, root, _) = setup("remade_dir");
    write_file(&root, "d/old.txt", b"o");
    write_file(&root, "d/kept.txt", b"k");
    write_file(&root, "d/sub/deep.txt", b"x");
    enqueue(
        &repo,
        &[
            FsEvent::Create("/d".into()),
            FsEvent::Create("/d/old.txt".into()),
            FsEvent::Create("/d/kept.txt".into()),
            FsEvent::Create("/d/sub".into()),
            FsEvent::Create("/d/sub/deep.txt".into()),
        ],
    );
    executor::flush_pending(&repo).unwrap();
    let d = resolve(&repo, "/d").unwrap();
    let old = resolve(&repo, "/d/old.txt").unwrap();
    let kept = resolve(&repo, "/d/kept.txt").unwrap();
    let sub = resolve(&repo, "/d/sub").unwrap();
    let deep = resolve(&repo, "/d/sub/deep.txt").unwrap();

    std::fs::remove_dir_all(root.join("d")).unwrap();
    write_file(&root, "d/kept.txt", b"k2");
    write_file(&root, "d/new.txt", b"n");
    enqueue(
        &repo,
        &[
            FsEvent::Remove("/d".into()),
            FsEvent::Create("/d".into()),
            FsEvent::Create("/d/kept.txt".into()),
            FsEvent::Create("/d/new.txt".into()),
        ],
    );
    executor::flush_pending(&repo).unwrap();

    for (uuid, what) in [(old, "d/old.txt"), (sub, "d/sub"), (deep, "d/sub/deep.txt")] {
        assert_eq!(field_value(&repo, uuid, "mfr_path"), Some(Value::Nothing), "{what} is gone");
    }
    assert_eq!(resolve(&repo, "/d"), Some(d), "the name is back: its record stays");
    assert_eq!(resolve(&repo, "/d/kept.txt"), Some(kept), "so is this one's");
    assert!(resolve(&repo, "/d/new.txt").is_some());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_a_file_moved_out_of_a_folder_just_moved_keeps_its_metarecord() {
    // `mv d e && mv e/x.jpg x.jpg` read by the inotify source: the folder's
    // watch still answers to its old path, and the file's departure from it
    // is lost — only its arrival is delivered. The arrival is re-paired with
    // the record the moved folder left behind rather than tracked anew.
    let (repo, root, _) = setup("move_out_of_moved");
    write_file(&root, "d/x.jpg", b"xx");
    write_file(&root, "d/stay.jpg", b"s");
    enqueue(
        &repo,
        &[
            FsEvent::Create("/d".into()),
            FsEvent::Create("/d/x.jpg".into()),
            FsEvent::Create("/d/stay.jpg".into()),
        ],
    );
    executor::flush_pending(&repo).unwrap();
    let x = resolve(&repo, "/d/x.jpg").unwrap();
    let stay = resolve(&repo, "/d/stay.jpg").unwrap();

    std::fs::rename(root.join("d"), root.join("e")).unwrap();
    std::fs::rename(root.join("e/x.jpg"), root.join("x.jpg")).unwrap();
    enqueue(
        &repo,
        &[
            FsEvent::Rename("/d".into(), "/e".into()),
            FsEvent::RenameFrom("/d".into()),
            FsEvent::RenameTo("/x.jpg".into()),
        ],
    );
    executor::flush_pending(&repo).unwrap();

    assert_eq!(resolve(&repo, "/x.jpg"), Some(x), "the same record, moved");
    assert_eq!(resolve(&repo, "/e/x.jpg"), None, "and not left behind");
    assert_eq!(resolve(&repo, "/e/stay.jpg"), Some(stay));
    std::fs::remove_dir_all(root).unwrap();
}

/// Writes each of `rels` with the same content length and the same mtime: files
/// the stat a rename keeps (kind, size, mtime) cannot tell apart.
fn write_alike(root: &Path, rels: &[&str]) {
    let when = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    for rel in rels {
        write_file(root, rel, b"same");
        let file = std::fs::File::options().write(true).open(root.join(&rel[1..])).unwrap();
        file.set_modified(when).unwrap();
    }
}

#[test]
fn test_files_alike_moved_out_of_a_folder_just_moved_keep_their_own_records() {
    // Two departures from a moved folder with the same kind, size and mtime:
    // the stat alone cannot say which arrival is which. The name can — a move
    // keeps it — whatever order the arrivals come in.
    let (repo, root, _) = setup("alike_out_of_moved");
    write_alike(&root, &["/d/p", "/d/q"]);
    enqueue(
        &repo,
        &[
            FsEvent::Create("/d".into()),
            FsEvent::Create("/d/p".into()),
            FsEvent::Create("/d/q".into()),
        ],
    );
    executor::flush_pending(&repo).unwrap();
    let (p, q) = (resolve(&repo, "/d/p").unwrap(), resolve(&repo, "/d/q").unwrap());

    std::fs::rename(root.join("d"), root.join("e")).unwrap();
    std::fs::rename(root.join("e/q"), root.join("q")).unwrap();
    std::fs::rename(root.join("e/p"), root.join("p")).unwrap();
    enqueue(
        &repo,
        &[
            FsEvent::Rename("/d".into(), "/e".into()),
            FsEvent::RenameTo("/q".into()),
            FsEvent::RenameTo("/p".into()),
        ],
    );
    executor::flush_pending(&repo).unwrap();

    assert_eq!(resolve(&repo, "/q"), Some(q), "q keeps q's record");
    assert_eq!(resolve(&repo, "/p"), Some(p), "p keeps p's record");
}

#[test]
fn test_an_arrival_two_departures_could_be_is_paired_with_neither() {
    // One file moved out and renamed, the other removed, both alike: either
    // record could be the arrival's, and a wrong guess hands a file another
    // one's fields. Neither is taken — both are orphaned, the arrival is new,
    // and `mf orphan relink` can match them by content.
    let (repo, root, _) = setup("alike_ambiguous");
    write_alike(&root, &["/d/p", "/d/q"]);
    enqueue(
        &repo,
        &[
            FsEvent::Create("/d".into()),
            FsEvent::Create("/d/p".into()),
            FsEvent::Create("/d/q".into()),
        ],
    );
    executor::flush_pending(&repo).unwrap();
    let (p, q) = (resolve(&repo, "/d/p").unwrap(), resolve(&repo, "/d/q").unwrap());

    std::fs::rename(root.join("d"), root.join("e")).unwrap();
    std::fs::rename(root.join("e/p"), root.join("r")).unwrap();
    std::fs::remove_file(root.join("e/q")).unwrap();
    enqueue(&repo, &[FsEvent::Rename("/d".into(), "/e".into()), FsEvent::RenameTo("/r".into())]);
    executor::flush_pending(&repo).unwrap();

    let r = resolve(&repo, "/r").expect("the arrival is tracked");
    assert!(r != p && r != q, "the arrival took a record it cannot be sure of");
    assert_eq!(field_value(&repo, p, "mfr_path"), Some(Value::Nothing));
    assert_eq!(field_value(&repo, q, "mfr_path"), Some(Value::Nothing));
}

#[test]
fn test_a_departure_reported_twice_is_still_one_to_pair_with() {
    // `mkdir albums && mv trip albums/` read by the inotify source: the
    // folder's own watch and its parent's both report it leaving, and the new
    // folder's scan finds it. One departure, so no ambiguity.
    let (repo, root, _) = setup("departure_twice");
    write_file(&root, "trip/x.jpg", b"x");
    enqueue(&repo, &[FsEvent::Create("/trip".into()), FsEvent::Create("/trip/x.jpg".into())]);
    executor::flush_pending(&repo).unwrap();
    let (trip, x) = (resolve(&repo, "/trip").unwrap(), resolve(&repo, "/trip/x.jpg").unwrap());

    std::fs::create_dir(root.join("albums")).unwrap();
    std::fs::rename(root.join("trip"), root.join("albums/trip")).unwrap();
    enqueue(
        &repo,
        &[
            FsEvent::Create("/albums".into()),
            FsEvent::RenameFrom("/trip".into()),
            FsEvent::RenameFrom("/trip".into()),
        ],
    );
    executor::flush_pending(&repo).unwrap();

    assert_eq!(resolve(&repo, "/albums/trip"), Some(trip));
    assert_eq!(resolve(&repo, "/albums/trip/x.jpg"), Some(x));
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
    enqueue(&repo, &[FsEvent::Create("/old".into()), FsEvent::Create("/old/file.txt".into())]);
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
    assert_eq!(op_count(&repo, Some("file_moved")), 1);
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
    enqueue(&repo, &[FsEvent::Create("/old".into()), FsEvent::Create("/old/file.txt".into())]);
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
    assert_eq!(op_count(&repo, Some("file_moved")), 1);
    assert_eq!(op_count(&repo, Some("file_deleted")), 0);

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

// ── Arrival (Rename(To)) ──────────────────────────────────────────────────────

/// A file that arrives is tracked afresh, even when an orphan holds its exact
/// content.
///
/// The flush used to search the orphans for a full-hash match and re-home one.
/// That search can only pay off by *hashing the arriving file*, and a single
/// event can carry a whole subtree: an operation heavy enough to belong to a
/// command the user runs, not to the event path. It is now `orphan relink`
/// (doc "Relinking orphans"), and the metadata waits on the
/// orphan until it is run.
#[test]
fn test_an_arrival_does_not_re_home_an_orphan_by_fingerprint() {
    let (repo, root, _) = setup("arrival");
    write_file(&root, "song.mp3", b"some audio content");
    enqueue(&repo, &[FsEvent::Create("/song.mp3".into())]);
    executor::flush_pending(&repo).unwrap();
    let uuid = resolve(&repo, "/song.mp3").unwrap();

    // Store the fingerprints (normally computed by the duplicate scan).
    let partial = metafolder_daemon::fingerprint::partial_hash(&root.join("song.mp3")).unwrap();
    let full = metafolder_daemon::fingerprint::full_hash(&root.join("song.mp3")).unwrap();
    {
        let mut conn = repo.conn.lock().unwrap();
        let mut w = Writer::begin(&mut conn, None).unwrap();
        w.set_field(uuid, "mfr_partial_hash", Value::String(partial)).unwrap();
        w.set_field(uuid, "mfr_full_hash", Value::String(full)).unwrap();
        w.commit().unwrap();
    }

    // The file leaves the repository, then comes back elsewhere.
    std::fs::rename(root.join("song.mp3"), std::env::temp_dir().join("mf_outside.mp3")).unwrap();
    enqueue(&repo, &[FsEvent::RenameFrom("/song.mp3".into())]);
    executor::flush_pending(&repo).unwrap();
    assert_eq!(field_value(&repo, uuid, "mfr_path"), Some(Value::Nothing));

    write_file(&root, "back/song2.mp3", b"some audio content");
    std::fs::remove_file(std::env::temp_dir().join("mf_outside.mp3")).unwrap();
    enqueue(&repo, &[FsEvent::Create("/back".into()), FsEvent::RenameTo("/back/song2.mp3".into())]);
    executor::flush_pending(&repo).unwrap();

    let arrived = resolve(&repo, "/back/song2.mp3").expect("the arriving file is tracked");
    assert_ne!(arrived, uuid, "the flush must not have hashed its way back to the orphan");
    assert_eq!(
        field_value(&repo, uuid, "mfr_path"),
        Some(Value::Nothing),
        "the orphan is left for `orphan relink` to re-home"
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

// ── Watch activity: the operations a flush writes (doc "Watch activity") ───────

#[test]
fn test_a_flush_counts_the_operations_it_wrote_per_path() {
    use metafolder_daemon::relpath::RelPath;
    let (repo, root, _) = setup("activity_ops");
    write_file(&root, "dir/m.txt", b"v1");
    write_file(&root, "other.txt", b"v1");
    enqueue(&repo, &[FsEvent::Create("/dir/m.txt".into()), FsEvent::Create("/other.txt".into())]);
    let before = op_count(&repo, None);
    executor::flush_pending(&repo).unwrap();
    let written = (op_count(&repo, None) - before) as u64;
    assert!(written > 0);

    let rel = |p: &str| RelPath::from_display(p);
    let (dir, other) = {
        let activity = repo.watch_activity.lock().unwrap();
        // Everything the flush wrote, and nothing else, is counted.
        assert_eq!(activity.total_operations(), written);
        let dir = activity.operations(&rel("/dir"));
        let other = activity.operations(&rel("/other.txt"));
        assert!(dir > 0 && other > 0);
        assert_eq!(dir + other, written);
        assert_eq!(activity.operations(&rel("/dir/m.txt")), dir);
        // Enqueued by hand here: no event was *delivered*.
        assert_eq!(activity.total(), 0);
        (dir, other)
    };

    // A later flush adds exactly what it wrote, on the path it wrote it for.
    write_file(&root, "other.txt", b"version two, longer");
    enqueue(&repo, &[FsEvent::ModifyData("/other.txt".into())]);
    let before = op_count(&repo, None);
    executor::flush_pending(&repo).unwrap();
    let more = (op_count(&repo, None) - before) as u64;
    assert!(more > 0);
    let activity = repo.watch_activity.lock().unwrap();
    assert_eq!(activity.total_operations(), written + more);
    assert_eq!(activity.operations(&rel("/other.txt")), other + more);
    assert_eq!(activity.operations(&rel("/dir")), dir);
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

#[test]
fn test_modify_data_invalidates_the_whole_content_derived_family() {
    // The hashes, the stamp they were computed under, and the duplicate group
    // they justified all die with the content (doc "Duplicate groups").
    let (repo, root, _) = setup("modifyfamily");
    write_file(&root, "m.txt", b"v1");
    enqueue(&repo, &[FsEvent::Create("/m.txt".into())]);
    executor::flush_pending(&repo).unwrap();
    let uuid = resolve(&repo, "/m.txt").unwrap();
    let group = {
        let mut conn = repo.conn.lock().unwrap();
        let mut w = Writer::begin(&mut conn, None).unwrap();
        let group = w
            .create_metarecord(vec![Field::new(
                "mf_schema",
                Value::String("duplicate_group".into()),
            )])
            .unwrap()
            .uuid;
        w.set_field(uuid, "mfr_partial_hash", Value::String("aaaa".into())).unwrap();
        w.set_field(uuid, "mfr_full_hash", Value::String("bbbb".into())).unwrap();
        w.set_field(uuid, "mfr_hash_mtime", Value::DateTime(1_700_000_000_000)).unwrap();
        w.set_field(uuid, "mfr_hash_size", Value::Int(2)).unwrap();
        w.set_field(uuid, "mfr_duplicate_group", Value::Ref(group)).unwrap();
        // Two other members, so the departure below leaves a group that still
        // holds a pair: what is under test here is the invalidation, not the
        // dissolution.
        for other in ["/o1.txt", "/o2.txt"] {
            let o = w
                .create_metarecord(vec![Field::new("mf_name", Value::String(other.into()))])
                .unwrap()
                .uuid;
            w.set_field(o, "mfr_duplicate_group", Value::Ref(group)).unwrap();
        }
        w.commit().unwrap();
        group
    };

    write_file(&root, "m.txt", b"version two, longer");
    enqueue(&repo, &[FsEvent::ModifyData("/m.txt".into())]);
    executor::flush_pending(&repo).unwrap();

    for name in metafolder_daemon::fingerprint::CONTENT_DERIVED_FIELDS {
        assert_eq!(field_value(&repo, uuid, name), None, "{name} should be invalidated");
    }
    // The group itself still has two members, so it survives — with its count
    // brought down to what is left of it (doc "Duplicate groups").
    assert!(field_value(&repo, group, "mf_schema").is_some());
    assert_eq!(field_value(&repo, group, "mfr_duplicate_count"), Some(Value::Int(2)));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_an_echo_on_an_unchanged_file_keeps_the_hash_cache_and_the_group() {
    // A `Create`/`Modify` event that describes a state the database already
    // holds — a tool touching a file without changing it, a crash replay, a
    // sync echo — must produce nothing. It used to clear the hashes anyway,
    // which (once duplicate detection existed) made a scan's whole result
    // evaporate the moment the watcher caught up.
    let (repo, root, _) = setup("echo");
    write_file(&root, "steady.txt", b"unchanged");
    enqueue(&repo, &[FsEvent::Create("/steady.txt".into())]);
    executor::flush_pending(&repo).unwrap();
    let uuid = resolve(&repo, "/steady.txt").unwrap();
    let group = {
        let mut conn = repo.conn.lock().unwrap();
        let mut w = Writer::begin(&mut conn, None).unwrap();
        let group = w
            .create_metarecord(vec![Field::new(
                "mf_schema",
                Value::String("duplicate_group".into()),
            )])
            .unwrap()
            .uuid;
        w.set_field(uuid, "mfr_partial_hash", Value::String("aaaa".into())).unwrap();
        w.set_field(uuid, "mfr_full_hash", Value::String("bbbb".into())).unwrap();
        w.set_field(uuid, "mfr_duplicate_group", Value::Ref(group)).unwrap();
        w.commit().unwrap();
        group
    };
    let revisions_before = revision_count(&repo);

    // The file is untouched: same bytes, same mtime.
    enqueue(
        &repo,
        &[FsEvent::ModifyData("/steady.txt".into()), FsEvent::Create("/steady.txt".into())],
    );
    executor::flush_pending(&repo).unwrap();

    assert_eq!(
        field_value(&repo, uuid, "mfr_full_hash"),
        Some(Value::String("bbbb".into())),
        "an echo must not destroy the hash cache"
    );
    assert_eq!(
        field_value(&repo, uuid, "mfr_duplicate_group"),
        Some(Value::Ref(group)),
        "nor the duplicate group it justified"
    );
    assert_eq!(revision_count(&repo), revisions_before, "and must write no revision at all");

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_remove_clears_the_duplicate_group_but_keeps_the_hashes() {
    // An orphan is no longer a live duplicate; but the hashes are exactly what
    // re-homes it when the file comes back, so they must survive.
    let (repo, root, _) = setup("removegroup");
    write_file(&root, "d.txt", b"content");
    enqueue(&repo, &[FsEvent::Create("/d.txt".into())]);
    executor::flush_pending(&repo).unwrap();
    let uuid = resolve(&repo, "/d.txt").unwrap();
    {
        let mut conn = repo.conn.lock().unwrap();
        let mut w = Writer::begin(&mut conn, None).unwrap();
        let group = w
            .create_metarecord(vec![Field::new(
                "mf_schema",
                Value::String("duplicate_group".into()),
            )])
            .unwrap()
            .uuid;
        w.set_field(uuid, "mfr_partial_hash", Value::String("aaaa".into())).unwrap();
        w.set_field(uuid, "mfr_full_hash", Value::String("bbbb".into())).unwrap();
        w.set_field(uuid, "mfr_duplicate_group", Value::Ref(group)).unwrap();
        w.commit().unwrap();
    }

    std::fs::remove_file(root.join("d.txt")).unwrap();
    enqueue(&repo, &[FsEvent::Remove("/d.txt".into())]);
    executor::flush_pending(&repo).unwrap();

    assert_eq!(field_value(&repo, uuid, "mfr_path"), Some(Value::Nothing));
    assert_eq!(field_value(&repo, uuid, "mfr_duplicate_group"), None, "group link cleared");
    assert_eq!(
        field_value(&repo, uuid, "mfr_full_hash"),
        Some(Value::String("bbbb".into())),
        "the hashes must survive an orphaning — they re-home the file"
    );

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_removing_a_member_dissolves_a_pair_and_updates_a_bigger_group() {
    // The path the GUI's trash action takes: the file moves away, the watcher
    // orphans its metarecord, and the group it left must stop claiming it
    // (doc "Duplicate groups").
    let (repo, root, _) = setup("removecounters");
    for name in ["a.txt", "b.txt", "c.txt"] {
        write_file(&root, name, b"same bytes");
        enqueue(&repo, &[FsEvent::Create(format!("/{name}").as_str().into())]);
    }
    executor::flush_pending(&repo).unwrap();
    let members: Vec<Uuid> =
        ["/a.txt", "/b.txt", "/c.txt"].iter().map(|p| resolve(&repo, p).unwrap()).collect();
    let group = {
        let mut conn = repo.conn.lock().unwrap();
        let mut w = Writer::begin(&mut conn, None).unwrap();
        let group = w
            .create_metarecord(vec![
                Field::new("mf_schema", Value::String("duplicate_group".into())),
                Field::new("mfr_content_size", Value::Int(10)),
                Field::new("mfr_duplicate_count", Value::Int(3)),
                Field::new("mfr_duplicate_reclaimable", Value::Int(20)),
            ])
            .unwrap()
            .uuid;
        for &m in &members {
            w.set_field(m, "mfr_duplicate_group", Value::Ref(group)).unwrap();
        }
        w.commit().unwrap();
        group
    };

    std::fs::remove_file(root.join("c.txt")).unwrap();
    enqueue(&repo, &[FsEvent::Remove("/c.txt".into())]);
    executor::flush_pending(&repo).unwrap();

    assert_eq!(field_value(&repo, group, "mfr_duplicate_count"), Some(Value::Int(2)));
    assert_eq!(field_value(&repo, group, "mfr_duplicate_reclaimable"), Some(Value::Int(10)));

    // Down to one member: the group is gone, and so is the survivor's link.
    std::fs::remove_file(root.join("b.txt")).unwrap();
    enqueue(&repo, &[FsEvent::Remove("/b.txt".into())]);
    executor::flush_pending(&repo).unwrap();

    {
        let conn = repo.conn.lock().unwrap();
        assert!(
            metafolder_daemon::store::Rows::metarecord(&*conn, group).unwrap().is_none(),
            "the group is deleted"
        );
    }
    assert_eq!(field_value(&repo, members[0], "mfr_duplicate_group"), None);

    std::fs::remove_dir_all(root).unwrap();
}

// ── Compaction and grouping ───────────────────────────────────────────────────

#[test]
fn test_compaction_create_then_remove_writes_nothing() {
    let (repo, root, _) = setup("compact1");
    enqueue(&repo, &[FsEvent::Create("/ghost.txt".into()), FsEvent::Remove("/ghost.txt".into())]);
    let revisions_before = revision_count(&repo);
    executor::flush_pending(&repo).unwrap();

    assert!(resolve(&repo, "/ghost.txt").is_none());
    assert_eq!(revision_count(&repo), revisions_before, "no revision for a fully-compacted buffer");
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
    assert_eq!(op_count(&repo, Some("file_moved")), 0);
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
    let ops_before = op_count(&repo, None);
    executor::flush_pending(&repo).unwrap();
    let ops_after = op_count(&repo, None);

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
    let revisions_before = revision_count(&repo);
    executor::flush_pending(&repo).unwrap();
    assert_eq!(revision_count(&repo) - revisions_before, 2, "one revision per op_type group");
    // Both creates share one revision.
    let create_revs = {
        let conn = repo.conn.lock().unwrap();
        let ops = metafolder_daemon::store::Log::all_ops(&*conn).unwrap();
        ops.iter()
            .filter(|op| op.op_type == "create_metarecord" && op.field_name.is_none())
            .map(|op| op.rev_id)
            .collect::<std::collections::HashSet<_>>()
            .len()
    };
    assert!(create_revs >= 1);

    std::fs::remove_dir_all(root).unwrap();
}

// ── Coordinated-rollback skip restoration (doc "Filesystem coordination") ───────────────

/// The head op id's parent — the navigation target that undoes exactly the
/// last operation.
fn undo_last_target(repo: &RepoState) -> Option<i64> {
    let conn = repo.conn.lock().unwrap();
    let head = metafolder_daemon::store::Log::head(&*conn).unwrap().unwrap();
    metafolder_daemon::store::Log::op(&*conn, head).unwrap().unwrap().parent_id
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
        log::coordinated_step(&mut *conn, target, true).unwrap();
    }
    assert_eq!(resolve(&repo, "/a.txt"), Some(uuid), "metadata reverted to old location");

    // Replaying the buffer applies the restoration → back to /b.txt.
    executor::flush_pending(&repo).unwrap();
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
        log::coordinated_step(&mut *conn, target, true).unwrap();
    }
    assert_eq!(resolve(&repo, "/a.txt"), Some(uuid), "metadata restored");

    executor::flush_pending(&repo).unwrap();
    assert_eq!(
        field_value(&repo, uuid, "mfr_path"),
        Some(Value::Nothing),
        "restoration re-recorded the deletion"
    );

    std::fs::remove_dir_all(root).unwrap();
}

// ── Idempotent refresh (doc "Suppressing sync's echoes") ─────────────────────────

#[test]
fn test_modify_data_on_unchanged_file_is_idempotent() {
    // A Modify(Data) event for a file whose stat did not change (e.g. the
    // watcher's echo of a change the daemon itself just recorded) must produce
    // no operation and no version bump — the executor's data refresh is
    // idempotent (doc "Suppressing sync's echoes").
    let (repo, root, _) = setup("idempotent_refresh");
    write_file(&root, "a.txt", b"hello");
    enqueue(&repo, &[FsEvent::Create("/a.txt".into())]);
    executor::flush_pending(&repo).unwrap();

    let uuid = resolve(&repo, "/a.txt").expect("file tracked after create");
    let v0 = {
        let conn = repo.conn.lock().unwrap();
        metafolder_daemon::store::Rows::version(&*conn, uuid).unwrap()
    };

    // The file is untouched on disk; its stored stat already matches.
    enqueue(&repo, &[FsEvent::ModifyData("/a.txt".into())]);
    executor::flush_pending(&repo).unwrap();

    let v1 = {
        let conn = repo.conn.lock().unwrap();
        metafolder_daemon::store::Rows::version(&*conn, uuid).unwrap()
    };
    assert_eq!(v0, v1, "an unchanged file must not bump the version");
    assert_eq!(
        op_count(&repo, Some("file_modified")),
        0,
        "no file_modified operation for an unchanged file"
    );

    std::fs::remove_dir_all(root).unwrap();
}

// ── Resilience ────────────────────────────────────────────────────────────────

// A flush must stay linear in the size of its batch.
//
// Re-pairing a move whose destination the watcher could not see compares an
// arriving path against the paths renamed away in the same batch. Done per
// pair, that is one filesystem stat and one database read for every
// (arrival, departure) combination — a batch that both loses and gains a few
// hundred files then takes a minute, holding the repository connection for all
// of it, so every query queues up behind it. That is what it looks like from
// the GUI: a `flush` task that never ends, and each new selection adding a
// query that never runs.
//
// The bound is a *ratio*, not a duration: the same arrivals are flushed twice,
// once with no departures and once with as many departures as arrivals. Linear
// pairing adds next to nothing to the baseline; per-pair pairing multiplied it
// by twelve at N = 800 on the machine this was written on. A wall-clock
// threshold would only have measured that machine.
#[test]
fn test_departures_do_not_make_a_flush_superlinear() {
    const N: usize = 400;

    /// Flushes a batch of `N` arrivals (a directory whose content the scan
    /// finds), optionally alongside `N` departures, and returns how long the
    /// flush took.
    fn timed_flush(prefix: &str, with_departures: bool) -> std::time::Duration {
        let (repo, root, _) = setup(prefix);

        // N tracked files under `old/` — the departures, when asked for.
        let mut creates = vec![FsEvent::Create("/old".into())];
        for i in 0..N {
            write_file(&root, &format!("old/f{i}.txt"), format!("old-{i}").as_bytes());
            creates.push(FsEvent::Create(format!("/old/f{i}.txt").as_str().into()));
        }
        enqueue(&repo, &creates);
        executor::flush_pending(&repo).unwrap();
        assert!(resolve(&repo, "/old/f0.txt").is_some(), "the files are tracked");

        // A directory arrives with N files inside; its content is found by the
        // scan, not by its own events.
        for i in 0..N {
            write_file(&root, &format!("new/g{i}.txt"), format!("new-{i}").as_bytes());
        }
        enqueue(&repo, &[FsEvent::Create("/new".into())]);
        if with_departures {
            // The Create comes first, so the departures are still tracked when
            // the arrivals are ingested — the worst case for the pairing.
            std::fs::remove_dir_all(root.join("old")).unwrap();
            let departures: Vec<FsEvent> = (0..N)
                .map(|i| FsEvent::RenameFrom(format!("/old/f{i}.txt").as_str().into()))
                .collect();
            enqueue(&repo, &departures);
        }

        let start = std::time::Instant::now();
        executor::flush_pending(&repo).unwrap();
        let elapsed = start.elapsed();

        assert!(resolve(&repo, "/new/g0.txt").is_some(), "the arriving files are tracked");
        std::fs::remove_dir_all(root).unwrap();
        elapsed
    }

    let baseline = timed_flush("linear_base", false);
    let with_departures = timed_flush("linear_dep", true);

    assert!(
        with_departures < baseline * 3,
        "{N} arrivals took {baseline:?} alone but {with_departures:?} \
         alongside {N} departures — the re-pairing is not linear",
    );
}

// Every file arriving in a watched directory used to be checked against the
// orphaned metarecords by asking the store, per file, for "the orphans whose
// `mfr_size` is N" — a question no key answers, so every `mfr_size` row of the
// repository was read once per arriving file. The flush was then quadratic in
// the repository, not in the batch: the same directory that landed in a second
// in a fresh repo took minutes in a real one.
//
// Counted, not timed (doc "Performance testing"): the same arrivals, flushed into a small
// repository and into one already holding eight times as many files, must read
// about the same number of keys.
#[test]
fn test_arrival_cost_does_not_grow_with_the_repository() {
    const N: usize = 300;

    /// Flushes `N` arrivals into a repository already holding `existing` files,
    /// and returns the keys the flush read.
    fn flush_reads(prefix: &str, existing: usize) -> u64 {
        let (repo, root, _) = setup(prefix);

        if existing > 0 {
            for i in 0..existing {
                write_file(&root, &format!("kept/f{i}.txt"), format!("kept-{i}").as_bytes());
            }
            enqueue(&repo, &[FsEvent::Create("/kept".into())]);
            executor::flush_pending(&repo).unwrap();
            assert!(resolve(&repo, "/kept/f0.txt").is_some(), "the existing files are tracked");
        }

        for i in 0..N {
            write_file(&root, &format!("new/g{i}.txt"), format!("new-{i}").as_bytes());
        }
        enqueue(&repo, &[FsEvent::Create("/new".into())]);

        let reads = || {
            let conn = repo.conn.lock().unwrap();
            metafolder_daemon::store::Rows::as_kv(&**conn).expect("a key-value store").reads()
        };
        let before = reads();
        executor::flush_pending(&repo).unwrap();
        let read = reads() - before;

        assert!(resolve(&repo, "/new/g0.txt").is_some(), "the arriving files are tracked");
        read
    }

    let small = flush_reads("scale_small", 0);
    let big = flush_reads("scale_big", 8 * N);

    // A deeper forest costs a few keys per lookup, never a key per file of the
    // repository: the defect this pins read `8 * N` more keys per arrival.
    assert!(
        big < small * 2,
        "{N} arrivals read {small} keys in an empty repository but {big} in one holding \
         {} files — the arrival path scales with the repository, not with the batch",
        8 * N,
    );
}

// ── Mass-orphan circuit breaker ───────────────────────────────────────────────

#[test]
fn test_a_cascade_larger_than_the_limit_is_refused() {
    // A filesystem going away can deliver the removal of a directory holding
    // the whole repository. Nulling thousands of paths on one event is never
    // what the user asked for: the cascade is skipped, the metadata survives,
    // and `mf orphan clear` remains the deliberate way to confirm it
    // (doc "Orphan scan").
    let root = TempDir::new("exec_breaker");
    let opened = repo::init_repository(&root, None, None, false).unwrap();
    let settings = metafolder_daemon::daemon_config::DaemonSettings {
        orphan_cascade_limit: 3,
        ..Default::default()
    };
    let repo_state = Arc::new(RepoState::from_opened_with(opened, &settings));
    let root_uuid = {
        let conn = repo_state.conn.lock().unwrap();
        metafolder_daemon::store::Rows::child_by_bytes(&*conn, "mfr_path", None, b"")
            .unwrap()
            .unwrap()
    };
    {
        let mut conn = repo_state.conn.lock().unwrap();
        let mut w = Writer::begin(&mut conn, None).unwrap();
        w.set_field(root_uuid, "mf_watch", Value::Bool(true)).unwrap();
        w.commit().unwrap();
    }
    for i in 0..5 {
        write_file(&root, &format!("/big/f{i}.txt"), b"x");
    }
    write_file(&root, "/small/only.txt", b"x");
    metafolder_daemon::reconcile::reconcile(&repo_state).unwrap();
    let big = resolve(&repo_state, "/big").unwrap();
    let kept = resolve(&repo_state, "/big/f0.txt").unwrap();
    let small = resolve(&repo_state, "/small").unwrap();

    std::fs::remove_dir_all(root.join("big")).unwrap();
    std::fs::remove_dir_all(root.join("small")).unwrap();
    enqueue(&repo_state, &[FsEvent::Remove("/big".into()), FsEvent::Remove("/small".into())]);
    executor::flush_pending(&repo_state).unwrap();

    // 6 records (the directory + its 5 files) exceeds the limit: nothing moved.
    assert!(matches!(field_value(&repo_state, big, "mfr_path"), Some(Value::TreeRef { .. })));
    assert!(matches!(field_value(&repo_state, kept, "mfr_path"), Some(Value::TreeRef { .. })));
    // The small deletion in the same batch is unaffected.
    assert_eq!(field_value(&repo_state, small, "mfr_path"), Some(Value::Nothing));
}

// ── Buffering the events (doc "Event batching") ───────────────

#[test]
fn test_enqueue_all_buffers_a_batch_as_one_transaction() {
    // Same rows as one-by-one enqueueing, in one transaction — which is the
    // point: in WAL mode every transaction is an fsync, so a batch buffered
    // event by event pays one per event. On this machine that was 3 ms against
    // 0.01 ms, i.e. a directory drop spending *minutes* before the flush that
    // applies it even starts.
    let (repo, root, _) = setup("enqueue_all");
    write_file(&root, "a.txt", b"a");
    write_file(&root, "b.txt", b"b");

    let batch = vec![
        (FsEvent::Create("/a.txt".into()), None),
        (FsEvent::Create("/b.txt".into()), Some(7)),
        (FsEvent::ModifyData("/a.txt".into()), None),
    ];
    executor::enqueue_all(&repo, batch);
    assert_eq!(executor::pending_count(&repo), 3);
    assert_eq!(
        repo.pending.lock().unwrap().iter().filter(|(_, t)| *t == Some(7)).count(),
        1,
        "the rename cookie is preserved"
    );

    // And they apply exactly as if they had been enqueued one at a time.
    let stats = executor::flush_pending(&repo).unwrap();
    assert_eq!(stats.events, 2, "the two events on /a.txt compact into one");
    assert!(resolve(&repo, "/a.txt").is_some());
    assert!(resolve(&repo, "/b.txt").is_some());
}

// ── Stopping a flush (doc "Pausing the watcher") ─────────────────

/// The number of buffered filesystem events left waiting.
fn pending_events(repo: &RepoState) -> usize {
    executor::pending_count(repo)
}

#[test]
fn test_paused_ingestion_applies_nothing_and_keeps_the_buffer() {
    let (repo, root, _) = setup("paused");
    write_file(&root, "a.txt", b"a");
    repo.pause_ingestion();

    enqueue(&repo, &[FsEvent::Create("/a.txt".into())]);
    let stats = executor::flush_pending(&repo).unwrap();

    assert_eq!(stats.events, 0, "nothing is applied while paused");
    assert!(resolve(&repo, "/a.txt").is_none(), "no metarecord was created");
    assert_eq!(pending_events(&repo), 1, "the event is still buffered");

    // Resuming applies exactly what was waiting.
    repo.resume_ingestion();
    let stats = executor::flush_pending(&repo).unwrap();
    assert_eq!(stats.events, 1);
    assert!(resolve(&repo, "/a.txt").is_some());
    assert_eq!(pending_events(&repo), 0);
}

#[test]
fn test_stopping_a_flush_pauses_ingestion_and_loses_nothing() {
    let (repo, root, _) = setup("stopflush");
    // Big enough that the flush is still running when the stop arrives, and
    // small enough to stay a fast test.
    for i in 0..400 {
        write_file(&root, &format!("/dropped/f{i}.txt"), b"x");
    }
    enqueue(&repo, &[FsEvent::Create("/dropped".into())]);

    // Stop it from another thread — exactly what the cancel route does — a
    // moment *after* the task appears, so the flush is caught mid-tree rather
    // than at its first event. That is the interesting case: by then the
    // abandoned group has already inserted directory nodes into the in-memory
    // tree cache, which rolling the transaction back does not undo.
    let watcher = Arc::clone(&repo);
    let stopper = std::thread::spawn(move || loop {
        if let Some(id) = watcher.tasks.active_id(TaskKind::Flush) {
            std::thread::sleep(std::time::Duration::from_millis(40));
            return watcher.tasks.request_cancel(id);
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    });

    let stats = executor::flush_pending(&repo).unwrap();
    stopper.join().unwrap();

    assert!(stats.cancelled, "the flush reports it was stopped");
    assert!(repo.is_ingestion_paused(), "stopping a flush pauses ingestion");
    // One event, so one group: abandoning it leaves the tree entirely unwritten
    // — in the database *and* in the tree cache, which is maintained alongside
    // the writes and would otherwise keep answering with uncommitted uuids.
    assert!(resolve(&repo, "/dropped").is_none(), "the abandoned group wrote nothing");
    assert_eq!(pending_events(&repo), 1, "the event is still buffered");
    let tasks = repo.tasks.list();
    let flush = tasks.iter().find(|t| t.kind == TaskKind::Flush).expect("a flush task is recorded");
    assert_eq!(flush.status, TaskStatus::Cancelled);

    // A stop is not a failure: nothing is dropped, and resuming applies it all.
    repo.resume_ingestion();
    executor::flush_pending(&repo).unwrap();
    assert!(resolve(&repo, "/dropped/f399.txt").is_some(), "everything lands after the resume");
    assert_eq!(pending_events(&repo), 0);
}

// ── Reported flush ────────────────────────────────────────────────────────────

/// Records every progress report as an owned summary, so a test can assert on
/// what a watcher would have seen.
fn record_flush(repo: &RepoState) -> Vec<String> {
    let seen = std::sync::Mutex::new(Vec::new());
    executor::flush_pending_reported(repo, &|p| {
        let line = match p {
            executor::FlushProgress::Buffered(n) => format!("buffered {n}"),
            executor::FlushProgress::Compacted(n) => format!("compacted {n}"),
            executor::FlushProgress::Applying { index, total, event } => {
                format!("applying {index}/{total} {}", executor::describe(event))
            }
            executor::FlushProgress::Scanning { dir, ingested } => {
                format!("scanning {} {ingested}", dir.display())
            }
            executor::FlushProgress::Step { name } => format!("step {name}"),
            executor::FlushProgress::Applied { index, total, elapsed } => {
                format!("applied {index}/{total} in {}ms", elapsed.as_millis())
            }
        };
        seen.lock().unwrap().push(line);
    })
    .unwrap();
    seen.into_inner().unwrap()
}

/// A load's replay of the buffered events can be the longest part of a startup
/// — the backlog is whatever the filesystem did while the daemon was down — and
/// it is the one phase whose size is not visible from outside. It must say how
/// much it has to do, and *which* event it is on: the cost per event is not
/// uniform, so a count alone does not say where the time goes.
#[test]
fn test_a_reported_flush_names_every_event_as_it_applies_it() {
    let (repo, root, _) = setup("reported");
    for i in 0..5 {
        write_file(&root, &format!("f{i}.txt"), b"x");
    }
    let mut events: Vec<FsEvent> =
        (0..5).map(|i| FsEvent::Create(format!("/f{i}.txt").as_str().into())).collect();
    // A redundant event, so compaction visibly has something to remove.
    events.push(FsEvent::ModifyData("/f0.txt".into()));
    enqueue(&repo, &events);

    let seen = record_flush(&repo);

    assert_eq!(seen[0], "buffered 6", "the backlog is reported as read: {seen:?}");
    assert_eq!(seen[1], "compacted 5", "the redundant modify is absorbed: {seen:?}");
    let applying: Vec<&String> = seen.iter().filter(|l| l.starts_with("applying")).collect();
    assert_eq!(applying.len(), 5, "one report per event, whatever the batch size: {seen:?}");
    assert!(applying[0].starts_with("applying 1/5 "), "{applying:?}");
    assert!(applying[4].starts_with("applying 5/5 "), "{applying:?}");
    assert!(applying[0].contains("f0.txt"), "the event names its path: {applying:?}");
}

/// One event can hide an arbitrarily large subtree: a directory pasted in
/// arrives as a single `Create`, and everything already inside it is ingested
/// by that one event's scan. A per-event count would sit on "3/426" for
/// minutes, so the scan reports its own progress.
#[test]
fn test_a_directory_arriving_whole_reports_its_scan() {
    let (repo, root, _) = setup("reported-scan");
    for i in 0..12 {
        write_file(&root, &format!("sub/f{i}.txt"), b"x");
    }
    enqueue(&repo, &[FsEvent::Create("/sub".into())]);

    let seen = record_flush(&repo);

    let scans: Vec<&String> = seen.iter().filter(|l| l.starts_with("scanning /sub")).collect();
    assert!(!scans.is_empty(), "the subtree scan reports nothing: {seen:?}");
    assert!(
        scans.last().unwrap().ends_with(" 12"),
        "the scan accounts for every entry it ingested: {scans:?}"
    );
    assert_eq!(
        seen.iter().filter(|l| l.starts_with("applying")).count(),
        1,
        "still one event: the subtree rides on it"
    );
}

/// A flush with nothing buffered reports nothing: the load report must not
/// carry a line per repository that had no backlog at all.
#[test]
fn test_an_empty_flush_reports_nothing() {
    let (repo, root, _) = setup("reported-empty");
    assert!(record_flush(&repo).is_empty(), "an empty flush has nothing to say");
    std::fs::remove_dir_all(root).unwrap();
}

/// Naming the event is not enough when the event itself is slow: the cost of
/// applying one is spread over a handful of steps (the eligibility walk, the
/// path resolution, the write), and which of them is the expensive one is the
/// whole question. Each announces itself, so the last line printed before a
/// stall names the step and not merely the event.
#[test]
fn test_applying_an_event_announces_its_steps_and_its_cost() {
    let (repo, root, _) = setup("reported-steps");
    write_file(&root, "a.txt", b"hello");
    enqueue(&repo, &[FsEvent::Create("/a.txt".into())]);
    executor::flush_pending(&repo).unwrap();

    // Now a real modification of a tracked file: the path the load's replay
    // spends its time in.
    std::fs::write(root.join("a.txt"), b"hello there").unwrap();
    enqueue(&repo, &[FsEvent::ModifyData("/a.txt".into())]);
    let seen = record_flush(&repo);

    let steps: Vec<&String> = seen.iter().filter(|l| l.starts_with("step ")).collect();
    assert!(
        steps.iter().any(|l| l.contains("eligibility")),
        "the eligibility walk must name itself: {seen:?}"
    );
    assert!(
        steps.iter().any(|l| l.contains("resolve")),
        "the path resolution must name itself: {seen:?}"
    );
    assert!(steps.iter().any(|l| l.contains("refresh")), "the write must name itself: {seen:?}");
    // And the event says what it cost, so a slow one is identified by number.
    assert!(
        seen.iter().any(|l| l.starts_with("applied 1/1 in ")),
        "the applied event reports its duration: {seen:?}"
    );
}

/// A flush never reads the repository's orphans.
///
/// It used to, once per arriving file, through an index built by a scan of
/// every orphan — and that scan was rebuilt whenever the batch orphaned
/// something, which a rename over a tracked destination does. A build tree
/// renames a fresh artifact over the one it replaces hundreds of times in a
/// row, so a few hundred buffered events became a few hundred full scans. The
/// whole question left the flush with `orphan relink`; this pins that it does
/// not come back.
#[test]
fn test_a_flush_never_scans_the_repository_for_orphans() {
    let (repo, root, _) = setup("no-orphan-scan");
    write_file(&root, "a.txt", b"aaa");
    write_file(&root, "t1.txt", b"ttt");
    enqueue(&repo, &[FsEvent::Create("/a.txt".into()), FsEvent::Create("/t1.txt".into())]);
    executor::flush_pending(&repo).unwrap();

    // A rename over a tracked destination (which orphans the record that held
    // it) and two arrivals from outside — the shape that used to rebuild the
    // orphan index per event.
    std::fs::rename(root.join("a.txt"), root.join("t1.txt")).unwrap();
    write_file(&root, "n1.txt", b"n1");
    write_file(&root, "n2.txt", b"n2");
    enqueue(
        &repo,
        &[
            FsEvent::Rename("/s1.txt".into(), "/n1.txt".into()),
            FsEvent::Rename("/a.txt".into(), "/t1.txt".into()),
            FsEvent::Rename("/s2.txt".into(), "/n2.txt".into()),
        ],
    );

    let seen = record_flush(&repo);
    // The *write* side stays: orphaning the record whose file was overwritten
    // is what a rename over a tracked destination means. What must not come
    // back is the repository-wide *read* the arrival used to do.
    for forbidden in ["index the repository's orphans", "fingerprint search among orphans"] {
        assert!(
            !seen.iter().any(|l| l.contains(forbidden)),
            "the flush still does '{forbidden}': {seen:?}"
        );
    }
    assert!(
        seen.iter().any(|l| l.contains("orphan cascade")),
        "the overwritten destination must still be orphaned: {seen:?}"
    );
}

/// A revision the watcher writes says so (`revision.origin`), whatever
/// operation types it holds: a file arriving is recorded as a
/// `create_metarecord`, indistinguishable by type from a user's write, and the
/// undo selection (doc "Undo and redo") must not mistake one for the
/// other.
#[test]
fn test_a_watcher_revision_records_its_origin() {
    let (repo, root, _root_uuid) = setup("origin");
    write_file(&root, "arrived.txt", b"x");
    enqueue(&repo, &[FsEvent::Create("/arrived.txt".into())]);
    executor::flush_pending(&repo).unwrap();
    assert!(resolve(&repo, "/arrived.txt").is_some(), "the watcher should track the file");

    let conn = repo.conn.lock().unwrap();
    // The revision holding the arrival — the newest one.
    let (rev_id, origin) = newest_revision(&*conn);
    let types: Vec<String> = metafolder_daemon::store::Log::revision_ops(&*conn, rev_id)
        .unwrap()
        .into_iter()
        .map(|op| op.op_type)
        .collect();
    assert!(
        types.iter().any(|t| t == "create_metarecord"),
        "an arrival is recorded as a creation: {types:?}"
    );
    assert_eq!(origin.as_deref(), Some("watcher"), "rev {rev_id} should be marked");

    // A user's write leaves it unset.
    drop(conn);
    let mut conn = repo.conn.lock().unwrap();
    let uuid = metafolder_daemon::store::Rows::child_by_bytes(&*conn, "mfr_path", None, b"")
        .unwrap()
        .unwrap();
    let mut w = Writer::begin(&mut conn, None).unwrap();
    w.set_field(uuid, "rating", Value::Int(3)).unwrap();
    w.commit().unwrap();
    let (_, origin) = newest_revision(&*conn);
    assert_eq!(origin, None, "an ordinary write is nobody's but the writer's");
}

fn metarecord_count(repo: &RepoState) -> usize {
    let conn = repo.conn.lock().unwrap();
    metafolder_daemon::store::Rows::metarecord_count(&*conn).unwrap()
}

/// A file moved into a directory created just before, told the way a source
/// that covers the whole filesystem tells it (the fanotify broker): the new
/// directory's creation, and the *whole* move — not only its departure, as
/// inotify reports a move into a directory it does not watch yet. The new
/// directory's scan finds the file already there; it must leave it to the
/// move, which keeps its identity. Ingesting it created a second metarecord,
/// which the move then orphaned (early_journey, under the broker).
#[test]
fn test_a_whole_move_into_a_new_directory_keeps_the_metarecord() {
    let (repo, root, _) = setup("wholemove");
    write_file(&root, "/photos/a.jpg", b"a");
    enqueue(&repo, &[FsEvent::Create("/photos".into()), FsEvent::Create("/photos/a.jpg".into())]);
    executor::flush_pending(&repo).unwrap();
    let a = resolve(&repo, "/photos/a.jpg").unwrap();
    let before = metarecord_count(&repo);

    std::fs::create_dir(root.join("archive")).unwrap();
    std::fs::rename(root.join("photos/a.jpg"), root.join("archive/a.jpg")).unwrap();
    enqueue(
        &repo,
        &[
            FsEvent::Create("/archive".into()),
            FsEvent::Rename("/photos/a.jpg".into(), "/archive/a.jpg".into()),
        ],
    );
    executor::flush_pending(&repo).unwrap();

    assert_eq!(resolve(&repo, "/archive/a.jpg"), Some(a), "the same metarecord, moved");
    assert_eq!(metarecord_count(&repo), before + 1, "only the new directory was created");
}

/// The same with a directory moved in: its whole subtree arrives with it.
#[test]
fn test_a_whole_directory_moved_into_a_new_directory_keeps_its_subtree() {
    let (repo, root, _) = setup("wholedirmove");
    write_file(&root, "/inbox/trip/x.jpg", b"x");
    enqueue(
        &repo,
        &[
            FsEvent::Create("/inbox".into()),
            FsEvent::Create("/inbox/trip".into()),
            FsEvent::Create("/inbox/trip/x.jpg".into()),
        ],
    );
    executor::flush_pending(&repo).unwrap();
    let trip = resolve(&repo, "/inbox/trip").unwrap();
    let x = resolve(&repo, "/inbox/trip/x.jpg").unwrap();
    let before = metarecord_count(&repo);

    std::fs::create_dir(root.join("sorted")).unwrap();
    std::fs::rename(root.join("inbox/trip"), root.join("sorted/trip")).unwrap();
    enqueue(
        &repo,
        &[
            FsEvent::Create("/sorted".into()),
            FsEvent::Rename("/inbox/trip".into(), "/sorted/trip".into()),
        ],
    );
    executor::flush_pending(&repo).unwrap();

    assert_eq!(resolve(&repo, "/sorted/trip"), Some(trip));
    assert_eq!(resolve(&repo, "/sorted/trip/x.jpg"), Some(x));
    assert_eq!(metarecord_count(&repo), before + 1, "only the new directory was created");
}
