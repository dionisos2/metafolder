//! Tests for reconcile (spec-file-tracking "Reconcile"): filesystem walk,
//! fingerprint phase, candidates, creation of new entries.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use metafolder_core::metarecord::Value;
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
fn test_reconcile_creates_records_for_new_files() {
    let (repo, root) = setup("create");
    write_file(&root, "a.txt", b"a");
    write_file(&root, "sub/b.txt", b"bb");

    let result = run(&repo);
    assert_eq!(result.created, 3, "a.txt + sub + sub/b.txt (.metafolder ignored by default)");
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
    assert_eq!(result.created, 1, "ok.txt only; .git and .metafolder are ignored");
    assert!(resolve(&repo, "/.git").is_none());

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_reconcile_skips_internal_even_when_metafolder_is_tracked() {
    let (repo, root) = setup("metafolder");
    // .metafolder is ignored by default; replace the root's mf_ignore with a
    // pattern that matches nothing so it becomes ordinary trackable content
    // again, isolating the internal/ (absolute-path) exclusion under test.
    let root_uuid = {
        let conn = repo.conn.lock().unwrap();
        db::find_tree_child(&conn, "mfr_path", None, "").unwrap().unwrap()
    };
    set_field(&repo, root_uuid, "mf_ignore", Value::String("^$".into()));

    run(&repo);
    // .metafolder/ is now trackable content (the root has mf_watch)...
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

// ── In-place modification refresh (option) ─────────────────────────────────────

#[test]
fn test_full_reconcile_refreshes_in_place_modifications_when_enabled() {
    let (repo, root) = setup("refresh_on");
    write_file(&root, "note.txt", b"hi");
    reconcile::reconcile_full(&repo, None, false, true).unwrap();
    let uuid = resolve(&repo, "/note.txt").unwrap();
    assert_eq!(field_value(&repo, uuid, "mfr_size"), Some(Value::Int(2)));

    // The file is edited in place (same path) while no watcher runs.
    write_file(&root, "note.txt", b"hello!");
    let result = reconcile::reconcile_full(&repo, None, false, true).unwrap();
    assert_eq!(result.created, 0);
    assert_eq!(result.moved, 0);
    assert_eq!(field_value(&repo, uuid, "mfr_size"), Some(Value::Int(6)));

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_full_reconcile_leaves_in_place_modifications_when_disabled() {
    let (repo, root) = setup("refresh_off");
    write_file(&root, "note.txt", b"hi");
    reconcile::reconcile_full(&repo, None, false, false).unwrap();
    let uuid = resolve(&repo, "/note.txt").unwrap();
    assert_eq!(field_value(&repo, uuid, "mfr_size"), Some(Value::Int(2)));

    write_file(&root, "note.txt", b"hello!");
    reconcile::reconcile_full(&repo, None, false, false).unwrap();
    // Without the refresh option, the stale size is left untouched.
    assert_eq!(field_value(&repo, uuid, "mfr_size"), Some(Value::Int(2)));

    std::fs::remove_dir_all(root).unwrap();
}

// ── Fingerprint phase ─────────────────────────────────────────────────────────

#[test]
fn test_reconcile_moves_record_on_full_hash_match() {
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
    assert_eq!(candidate.metarecord_uuid, uuid);
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
    assert_eq!(result.candidates[0].metarecord_uuid, uuid);
    assert_eq!(result.candidates[0].matches[0].fingerprint, "size");
    assert!(resolve(&repo, "/moved.txt").is_none(), "candidate file is not auto-created");

    std::fs::remove_dir_all(root).unwrap();
}

// ── MIME detection (spec-platform "MIME detection") ─────────────────────────────

const PNG_MAGIC: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0];

fn op_count(repo: &RepoState) -> i64 {
    let conn = repo.conn.lock().unwrap();
    conn.query_row("SELECT COUNT(*) FROM operation", [], |r| r.get(0)).unwrap()
}

#[test]
fn test_reconcile_computes_mime_when_enabled() {
    let (repo, root) = setup("mime");
    write_file(&root, "pic.png", PNG_MAGIC);
    write_file(&root, "notes.txt", b"hello"); // not magic-detectable → no mime
    reconcile::reconcile_full(&repo, None, true, false).unwrap();

    let pic = resolve(&repo, "/pic.png").unwrap();
    assert_eq!(field_value(&repo, pic, "mfr_mime"), Some(Value::String("image/png".into())));
    let notes = resolve(&repo, "/notes.txt").unwrap();
    assert_eq!(field_value(&repo, notes, "mfr_mime"), None, "undetectable → mfr_mime absent");

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_reconcile_skips_mime_when_disabled() {
    let (repo, root) = setup("nomime");
    write_file(&root, "pic.png", PNG_MAGIC);
    reconcile::reconcile_full(&repo, None, false, false).unwrap();
    let pic = resolve(&repo, "/pic.png").unwrap();
    assert_eq!(field_value(&repo, pic, "mfr_mime"), None);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_reconcile_mime_is_idempotent() {
    let (repo, root) = setup("mime_idem");
    write_file(&root, "pic.png", PNG_MAGIC);
    reconcile::reconcile_full(&repo, None, true, false).unwrap();
    let after_first = op_count(&repo);

    // A second mime reconcile must not rewrite the existing mfr_mime.
    let result = reconcile::reconcile_full(&repo, None, true, false).unwrap();
    assert_eq!(result.created, 0);
    assert_eq!(op_count(&repo), after_first, "mime must not be recomputed");

    let pic = resolve(&repo, "/pic.png").unwrap();
    assert_eq!(field_value(&repo, pic, "mfr_mime"), Some(Value::String("image/png".into())));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_reconcile_folds_mime_into_creation() {
    let (repo, root) = setup("mime_create");
    write_file(&root, "pic.png", PNG_MAGIC);
    reconcile::reconcile_full(&repo, None, true, false).unwrap();

    let pic = resolve(&repo, "/pic.png").unwrap();
    assert_eq!(field_value(&repo, pic, "mfr_mime"), Some(Value::String("image/png".into())));
    // mfr_mime is written as part of the create operation, not as a separate
    // field write: the record is born at version 0 with no follow-up op.
    let version = {
        let conn = repo.conn.lock().unwrap();
        db::get_version(&conn, pic).unwrap().unwrap()
    };
    assert_eq!(version, 0, "mfr_mime must be set at creation, not as a separate write");

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_similarity_phase_proposes_renamed_modified_file() {
    let (repo, root) = setup("similarity");
    write_file(&root, "music/old_song.mp3", &vec![b'a'; 1000]);
    run(&repo);
    let uuid = resolve(&repo, "/music/old_song.mp3").unwrap();

    // Moved AND modified: different name and size, so the fingerprint phase
    // (size pre-filter) cannot match it.
    std::fs::remove_file(root.join("music/old_song.mp3")).unwrap();
    write_file(&root, "music/old_song_v2.mp3", &vec![b'b'; 1100]);

    // Without a threshold the new file is just created (v1 behaviour).
    let v1 = reconcile::reconcile(&repo).unwrap();
    assert!(v1.candidates.is_empty(), "no similarity phase without a threshold");
    assert_eq!(v1.created, 1);
    // The orphan keeps its stale path.
    assert!(field_value(&repo, uuid, "mfr_path").is_some());

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_similarity_candidate_carries_a_score() {
    let (repo, root) = setup("sim_score");
    write_file(&root, "music/old_song.mp3", &vec![b'a'; 1000]);
    run(&repo);
    let uuid = resolve(&repo, "/music/old_song.mp3").unwrap();

    std::fs::remove_file(root.join("music/old_song.mp3")).unwrap();
    write_file(&root, "music/old_song_v2.mp3", &vec![b'b'; 1100]);

    let result = reconcile::reconcile_full(&repo, Some(0.6), false, false).unwrap();
    assert_eq!(result.moved, 0);
    assert_eq!(result.candidates.len(), 1, "{:?}", result.candidates);
    let candidate = &result.candidates[0];
    assert_eq!(candidate.metarecord_uuid, uuid);
    let m = &candidate.matches[0];
    assert_eq!(m.path, "/music/old_song_v2.mp3");
    assert_eq!(m.fingerprint, "similarity");
    assert!(m.score.unwrap() >= 0.6, "score {:?}", m.score);
    // The candidate file is not auto-created.
    assert!(resolve(&repo, "/music/old_song_v2.mp3").is_none());
    assert_eq!(result.created, 0);

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn test_reconcile_mismatched_partial_creates_new_metarecord() {
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
