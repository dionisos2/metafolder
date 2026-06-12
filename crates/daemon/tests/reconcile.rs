//! Tests for reconcile (spec-file-tracking "Reconcile"): filesystem walk,
//! fingerprint phase, candidates, creation of new entries.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use metafolder_core::entry::Value;
use metafolder_daemon::db;
use metafolder_daemon::fingerprint;
use metafolder_daemon::log::Writer;
use metafolder_daemon::reconcile::{self, ReconcileResult};
use metafolder_daemon::repo;
use metafolder_daemon::state::RepoState;
use uuid::Uuid;

fn temp_dir(prefix: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("metafolder_rec_{prefix}_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn setup(prefix: &str) -> (Arc<RepoState>, PathBuf) {
    let root = temp_dir(prefix);
    let opened = repo::init_repository(&root, None).unwrap();
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
    db::get_entry(&conn, uuid).unwrap().unwrap().get(name).cloned()
}

fn set_field(repo: &RepoState, uuid: Uuid, name: &str, value: Value) {
    let mut conn = repo.conn.lock().unwrap();
    let mut w = Writer::begin(&mut conn, repo.config.repo_uuid, None).unwrap();
    w.set_field(uuid, name, value).unwrap();
    w.commit().unwrap();
}

fn run(repo: &RepoState) -> ReconcileResult {
    reconcile::reconcile(repo).unwrap()
}

// ── Creation ──────────────────────────────────────────────────────────────────

#[test]
fn test_reconcile_creates_entries_for_new_files() {
    let (repo, root) = setup("create");
    write_file(&root, "a.txt", b"a");
    write_file(&root, "sub/b.txt", b"bb");

    let result = run(&repo);
    assert_eq!(result.created, 5, "a.txt + sub + sub/b.txt + .metafolder + config.json");
    assert_eq!(result.moved, 0);
    assert!(result.candidates.is_empty());

    let b = resolve(&repo, "/sub/b.txt").expect("entry for new file");
    assert_eq!(field_value(&repo, b, "mfr_size"), Some(Value::Int(2)));
    let sub = resolve(&repo, "/sub").unwrap();
    assert_eq!(field_value(&repo, sub, "mfr_type"), Some(Value::String("dir".into())));

    // Idempotent: nothing new on the second run.
    let again = run(&repo);
    assert_eq!(again.created, 0);

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_reconcile_respects_eligibility() {
    let (repo, root) = setup("elig");
    write_file(&root, ".git/config", b"x");
    write_file(&root, "ok.txt", b"y");

    let result = run(&repo);
    assert_eq!(result.created, 3, "ok.txt + .metafolder + config.json; .git is ignored");
    assert!(resolve(&repo, "/.git").is_none());

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_reconcile_tracks_metafolder_but_skips_internal() {
    let (repo, root) = setup("metafolder");

    run(&repo);
    // .metafolder/ is ordinary trackable content (the root has mf_watch)...
    assert!(resolve(&repo, "/.metafolder/config.json").is_some());
    // ...but internal/ (live database and other daemon-managed files) is
    // always excluded, by absolute path.
    assert!(resolve(&repo, "/.metafolder/internal").is_none());
    assert!(resolve(&repo, "/.metafolder/internal/db.sqlite").is_none());

    std::fs::remove_dir_all(root).unwrap();
}

// ── Orphans stay stale ────────────────────────────────────────────────────────

#[test]
fn test_reconcile_never_clears_paths() {
    let (repo, root) = setup("stale");
    write_file(&root, "gone.txt", b"data");
    run(&repo);
    let uuid = resolve(&repo, "/gone.txt").unwrap();

    std::fs::remove_file(root.join("gone.txt")).unwrap();
    let result = run(&repo);

    assert_eq!(result.moved, 0);
    // The stale TreeRef is left in place — reconcile never writes Nothing.
    assert!(matches!(
        field_value(&repo, uuid, "mfr_path"),
        Some(Value::TreeRef { .. })
    ));

    std::fs::remove_dir_all(root).unwrap();
}

// ── Fingerprint phase ─────────────────────────────────────────────────────────

#[test]
fn test_reconcile_moves_entry_on_full_hash_match() {
    let (repo, root) = setup("move");
    write_file(&root, "old_place.bin", b"unique content 42");
    run(&repo);
    let uuid = resolve(&repo, "/old_place.bin").unwrap();
    let partial = fingerprint::partial_hash(&root.join("old_place.bin")).unwrap();
    let full = fingerprint::full_hash(&root.join("old_place.bin")).unwrap();
    set_field(&repo, uuid, "mfr_partial_hash", Value::String(partial));
    set_field(&repo, uuid, "mfr_full_hash", Value::String(full));

    // Move the file while the daemon is "off" (no watcher running).
    std::fs::create_dir_all(root.join("new")).unwrap();
    std::fs::rename(root.join("old_place.bin"), root.join("new/place.bin")).unwrap();

    let result = run(&repo);
    assert_eq!(result.moved, 1);
    assert!(result.candidates.is_empty());
    assert_eq!(resolve(&repo, "/new/place.bin"), Some(uuid), "entry follows the file");

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_reconcile_partial_match_without_full_hash_is_a_candidate() {
    let (repo, root) = setup("strong");
    write_file(&root, "song.mp3", b"audio bytes here");
    run(&repo);
    let uuid = resolve(&repo, "/song.mp3").unwrap();
    let partial = fingerprint::partial_hash(&root.join("song.mp3")).unwrap();
    set_field(&repo, uuid, "mfr_partial_hash", Value::String(partial));
    // No mfr_full_hash stored → identity cannot be confirmed automatically.

    std::fs::rename(root.join("song.mp3"), root.join("renamed.mp3")).unwrap();
    let result = run(&repo);

    assert_eq!(result.moved, 0);
    assert_eq!(result.candidates.len(), 1);
    let candidate = &result.candidates[0];
    assert_eq!(candidate.entry_uuid, uuid);
    assert_eq!(candidate.stale_path, "/song.mp3");
    assert_eq!(candidate.matches.len(), 1);
    assert_eq!(candidate.matches[0].path, "/renamed.mp3");
    assert_eq!(candidate.matches[0].fingerprint, "partial_hash");
    // The candidate file must not be auto-created as a new entry.
    assert!(resolve(&repo, "/renamed.mp3").is_none());
    assert_eq!(result.created, 0);

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_reconcile_size_only_match_is_a_weak_candidate() {
    let (repo, root) = setup("weak");
    write_file(&root, "doc.txt", b"12345");
    run(&repo);
    let uuid = resolve(&repo, "/doc.txt").unwrap();
    // No hashes stored at all (normal after a plain create).

    std::fs::rename(root.join("doc.txt"), root.join("moved.txt")).unwrap();
    let result = run(&repo);

    assert_eq!(result.moved, 0);
    assert_eq!(result.candidates.len(), 1);
    assert_eq!(result.candidates[0].entry_uuid, uuid);
    assert_eq!(result.candidates[0].matches[0].fingerprint, "size");
    assert!(resolve(&repo, "/moved.txt").is_none(), "candidate file is not auto-created");

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_reconcile_mismatched_partial_creates_new_entry() {
    let (repo, root) = setup("mismatch");
    write_file(&root, "a.bin", b"AAAAA");
    run(&repo);
    let uuid = resolve(&repo, "/a.bin").unwrap();
    set_field(&repo, uuid, "mfr_partial_hash", Value::String("not-the-real-hash".into()));
    set_field(&repo, uuid, "mfr_full_hash", Value::String("nope".into()));

    std::fs::remove_file(root.join("a.bin")).unwrap();
    write_file(&root, "b.bin", b"BBBBB"); // Same size, different content.
    let result = run(&repo);

    // Partial hash mismatch → discarded, so b.bin is a brand new entry.
    assert_eq!(result.moved, 0);
    assert!(result.candidates.is_empty());
    assert_eq!(result.created, 1);
    assert!(resolve(&repo, "/b.bin").is_some());

    std::fs::remove_dir_all(root).unwrap();
}
