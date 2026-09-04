//! Tests for the orphan scan (spec-file-tracking "Orphan scan"): tracked
//! metarecords whose `mfr_path` points to a file that is definitely gone are
//! reported, while records under an unreadable/missing-mount ancestor are left
//! as "unknown" (never falsely orphaned).

use std::path::Path;
use std::sync::Arc;

use metafolder_core::metarecord::Value;
use metafolder_daemon::log::Writer;
use metafolder_daemon::orphans::{self, OrphanEntry};
use metafolder_daemon::state::RepoState;
use metafolder_daemon::{db, reconcile, repo};
use uuid::Uuid;

mod common;
use common::TempDir;

fn temp_dir(prefix: &str) -> TempDir {
    TempDir::new(&format!("orph_{prefix}"))
}

const DEFAULT_PATTERNS: &[&str] = &[r"\.metafolder(/.*)?$", r"(^|/)\.[^/]+"];

fn setup(prefix: &str) -> (Arc<RepoState>, TempDir) {
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

fn scan_uuids(repo: &RepoState) -> Vec<Uuid> {
    let mut uuids: Vec<Uuid> =
        orphans::scan_orphans(repo).unwrap().into_iter().map(|o: OrphanEntry| o.uuid).collect();
    uuids.sort();
    uuids
}

#[test]
fn scan_reports_a_metarecord_whose_file_was_deleted() {
    let (repo, root) = setup("gone");
    write_file(&root, "keep.txt", b"a");
    write_file(&root, "gone.txt", b"b");
    reconcile::reconcile(&repo).unwrap();
    let gone = resolve(&repo, "/gone.txt").unwrap();

    // Delete the file directly (no watcher running): the metarecord keeps its
    // stale mfr_path, so a query cannot tell it apart from a live file.
    std::fs::remove_file(root.join("gone.txt")).unwrap();

    assert_eq!(scan_uuids(&repo), vec![gone]);

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn scan_ignores_records_whose_file_still_exists() {
    let (repo, root) = setup("present");
    write_file(&root, "a.txt", b"a");
    write_file(&root, "sub/b.txt", b"b");
    reconcile::reconcile(&repo).unwrap();

    // Nothing deleted → no orphans.
    assert!(scan_uuids(&repo).is_empty());

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn scan_reports_a_whole_deleted_directory_subtree() {
    let (repo, root) = setup("subtree");
    write_file(&root, "dir/x.txt", b"x");
    write_file(&root, "dir/y.txt", b"y");
    reconcile::reconcile(&repo).unwrap();
    let dir = resolve(&repo, "/dir").unwrap();
    let x = resolve(&repo, "/dir/x.txt").unwrap();
    let y = resolve(&repo, "/dir/y.txt").unwrap();

    // Remove the whole directory: the parent (root) is readable, so every
    // record beneath it is definitely gone.
    std::fs::remove_dir_all(root.join("dir")).unwrap();

    let mut expected = vec![dir, x, y];
    expected.sort();
    assert_eq!(scan_uuids(&repo), expected);

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn scan_skips_records_under_an_unreadable_directory() {
    // A directory that cannot be read (e.g. a permission drop, or a stand-in for
    // an unmounted path) must NOT mass-orphan its children: the file may well be
    // there, we simply cannot tell. Guard against the false positive.
    let (repo, root) = setup("unreadable");
    write_file(&root, "locked/secret.txt", b"s");
    reconcile::reconcile(&repo).unwrap();

    let locked = root.join("locked");
    // Drop read/execute so read_dir fails with EACCES.
    let mut perms = std::fs::metadata(&locked).unwrap().permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o000);
    }
    std::fs::set_permissions(&locked, perms).unwrap();

    let uuids = scan_uuids(&repo);

    // Restore permissions before asserting / cleanup so the dir can be removed.
    let mut restore = std::fs::metadata(&locked).unwrap().permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        restore.set_mode(0o755);
    }
    std::fs::set_permissions(&locked, restore).unwrap();

    // secret.txt sits under an unreadable dir → unknown, not an orphan. The
    // `locked` dir itself exists and is readable from root, so it is not an
    // orphan either.
    assert!(uuids.is_empty(), "unreadable-dir children must not be orphaned: {uuids:?}");

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn clear_orphans_a_deleted_file_and_records_its_origin() {
    let (repo, root) = setup("clear");
    write_file(&root, "gone.txt", b"data");
    reconcile::reconcile(&repo).unwrap();
    let gone = resolve(&repo, "/gone.txt").unwrap();
    std::fs::remove_file(root.join("gone.txt")).unwrap();

    let cleared = orphans::clear_orphans(&repo, &[gone]).unwrap();
    assert_eq!(cleared, 1);

    // mfr_path is now the explicit-absence Nothing; the origin is frozen in
    // mfr_path_old; the metarecord itself is preserved.
    assert!(matches!(field_value(&repo, gone, "mfr_path"), Some(Value::Nothing)));
    assert_eq!(field_value(&repo, gone, "mfr_path_old"), Some(Value::String("/gone.txt".into())));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn clear_cascades_to_a_deleted_directory_subtree() {
    let (repo, root) = setup("clear-subtree");
    write_file(&root, "dir/x.txt", b"x");
    reconcile::reconcile(&repo).unwrap();
    let dir = resolve(&repo, "/dir").unwrap();
    let x = resolve(&repo, "/dir/x.txt").unwrap();
    std::fs::remove_dir_all(root.join("dir")).unwrap();

    // Clearing the directory cascades to its child in the same call.
    orphans::clear_orphans(&repo, &[dir]).unwrap();

    assert!(matches!(field_value(&repo, dir, "mfr_path"), Some(Value::Nothing)));
    assert!(matches!(field_value(&repo, x, "mfr_path"), Some(Value::Nothing)));
    assert_eq!(field_value(&repo, x, "mfr_path_old"), Some(Value::String("/dir/x.txt".into())));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn clear_skips_a_uuid_whose_file_still_exists() {
    // A stale scan must not orphan a file that came back: clear re-verifies.
    let (repo, root) = setup("clear-recheck");
    write_file(&root, "here.txt", b"data");
    reconcile::reconcile(&repo).unwrap();
    let here = resolve(&repo, "/here.txt").unwrap();

    let cleared = orphans::clear_orphans(&repo, &[here]).unwrap();
    assert_eq!(cleared, 0, "the file exists → must not be orphaned");
    assert!(matches!(field_value(&repo, here, "mfr_path"), Some(Value::TreeRef { .. })));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn clear_orphans_also_clears_the_duplicate_group_link() {
    // `mf orphan clear` performs the same transition as the watcher's delete,
    // so it carries the same consequence: the record is no longer a live
    // duplicate (spec-duplicates "Invariant"). The hashes stay — they are what
    // re-homes the file if it reappears.
    let (repo, root) = setup("clear-dupgroup");
    write_file(&root, "gone.txt", b"data");
    reconcile::reconcile(&repo).unwrap();
    let gone = resolve(&repo, "/gone.txt").unwrap();
    {
        let mut conn = repo.conn.lock().unwrap();
        let mut w = Writer::begin(&mut conn, None).unwrap();
        let group = w
            .create_metarecord(vec![metafolder_core::metarecord::Field::new(
                "mf_schema",
                Value::String("duplicate_group".into()),
            )])
            .unwrap()
            .uuid;
        w.set_field(gone, "mfr_duplicate_group", Value::Ref(group)).unwrap();
        w.set_field(gone, "mfr_full_hash", Value::String("bbbb".into())).unwrap();
        w.commit().unwrap();
    }
    std::fs::remove_file(root.join("gone.txt")).unwrap();

    assert_eq!(orphans::clear_orphans(&repo, &[gone]).unwrap(), 1);

    assert_eq!(field_value(&repo, gone, "mfr_duplicate_group"), None);
    assert_eq!(field_value(&repo, gone, "mfr_full_hash"), Some(Value::String("bbbb".into())));
}
