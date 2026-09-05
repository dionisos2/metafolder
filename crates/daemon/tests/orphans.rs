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

// ── Relinking orphans by fingerprint ──────────────────────────────────────────

/// Stores the two fingerprints on `uuid`, as the duplicate scan would.
fn store_hashes(repo: &RepoState, uuid: Uuid, abs: &Path) {
    let partial = metafolder_daemon::fingerprint::partial_hash(abs).unwrap();
    let full = metafolder_daemon::fingerprint::full_hash(abs).unwrap();
    let mut conn = repo.conn.lock().unwrap();
    let mut w = Writer::begin(&mut conn, None).unwrap();
    w.set_field(uuid, "mfr_partial_hash", Value::String(partial)).unwrap();
    w.set_field(uuid, "mfr_full_hash", Value::String(full)).unwrap();
    w.commit().unwrap();
}

fn path_of(repo: &RepoState, rel: &str) -> Option<Uuid> {
    let conn = repo.conn.lock().unwrap();
    let mut cache = repo.cache.lock().unwrap();
    cache.resolve_path(&conn, "mfr_path", rel).unwrap()
}

/// The metadata an orphan holds follows its file when the file turns up again
/// under a new name — but only when the user asks for it, since confirming the
/// identity means hashing the candidate.
///
/// The orphan keeps its uuid: that is the whole point of re-homing rather than
/// tracking afresh, since references elsewhere point at it.
#[test]
fn test_relink_re_homes_an_orphan_onto_its_reappeared_file() {
    let (repo, root) = setup("relink");
    write_file(&root, "song.mp3", b"some audio content");
    reconcile::reconcile(&repo).unwrap();
    let original = path_of(&repo, "/song.mp3").expect("tracked");
    store_hashes(&repo, original, &root.join("song.mp3"));
    {
        let mut conn = repo.conn.lock().unwrap();
        let mut w = Writer::begin(&mut conn, None).unwrap();
        w.set_field(original, "rating", Value::Int(5)).unwrap();
        w.commit().unwrap();
    }

    // The file is renamed while nothing was watching, then reconcile records
    // the new name as a metarecord of its own and leaves the old one orphaned.
    std::fs::rename(root.join("song.mp3"), root.join("renamed.mp3")).unwrap();
    orphans::clear_orphans(&repo, &[original]).unwrap();
    reconcile::reconcile(&repo).unwrap();
    let fresh = path_of(&repo, "/renamed.mp3").expect("the new name is tracked");
    assert_ne!(fresh, original, "reconcile tracked it afresh");

    let result = orphans::relink(&repo).unwrap();
    assert_eq!(result.relinked, 1, "the orphan is re-homed onto its file");

    assert_eq!(
        path_of(&repo, "/renamed.mp3"),
        Some(original),
        "the orphan took the position, keeping its uuid and its metadata"
    );
    let conn = repo.conn.lock().unwrap();
    assert_eq!(
        db::get_metarecord(&conn, original).unwrap().unwrap().get("rating"),
        Some(&Value::Int(5)),
        "the metadata came back with it"
    );
    assert!(
        db::get_metarecord(&conn, fresh).unwrap().is_none(),
        "the freshly tracked duplicate is gone"
    );
}

/// A candidate the user has already annotated is *not* absorbed: re-homing the
/// orphan onto it would delete whatever was attached to it in the meantime.
/// Reported instead, and left alone.
#[test]
fn test_relink_refuses_to_absorb_an_annotated_metarecord() {
    let (repo, root) = setup("relink-conflict");
    write_file(&root, "song.mp3", b"some audio content");
    reconcile::reconcile(&repo).unwrap();
    let original = path_of(&repo, "/song.mp3").expect("tracked");
    store_hashes(&repo, original, &root.join("song.mp3"));

    std::fs::rename(root.join("song.mp3"), root.join("renamed.mp3")).unwrap();
    orphans::clear_orphans(&repo, &[original]).unwrap();
    reconcile::reconcile(&repo).unwrap();
    let fresh = path_of(&repo, "/renamed.mp3").expect("the new name is tracked");
    {
        let mut conn = repo.conn.lock().unwrap();
        let mut w = Writer::begin(&mut conn, None).unwrap();
        w.set_field(fresh, "note", Value::String("mine".into())).unwrap();
        w.commit().unwrap();
    }

    let result = orphans::relink(&repo).unwrap();
    assert_eq!(result.relinked, 0);
    assert_eq!(result.conflicts, 1, "the annotated metarecord is reported, not absorbed");
    assert_eq!(path_of(&repo, "/renamed.mp3"), Some(fresh), "it kept its position");
    let conn = repo.conn.lock().unwrap();
    assert!(db::get_metarecord(&conn, fresh).unwrap().is_some(), "and it still exists");
}
