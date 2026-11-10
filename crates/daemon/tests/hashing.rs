//! Tests for computing content hashes on demand and keeping them fresh (doc
//! "Content hashes kept fresh").

use std::path::Path;
use std::sync::Arc;

use metafolder_core::metarecord::Value;
use metafolder_daemon::fingerprint;
use metafolder_daemon::hashing::{self, HashOptions, HashResult};
use metafolder_daemon::log::Writer;
use metafolder_daemon::state::RepoState;
use metafolder_daemon::{reconcile, repo};
use uuid::Uuid;

mod common;
use common::TempDir;

const DEFAULT_PATTERNS: &[&str] = &[r"\.metafolder(/.*)?$", r"(^|/)\.[^/]+"];

fn setup(prefix: &str) -> (Arc<RepoState>, TempDir) {
    let root = TempDir::new(&format!("hash_{prefix}"));
    common::seed_config(&root);
    let opened = repo::init_repository(&root, None, None, false).unwrap();
    let repo_state = Arc::new(RepoState::from_opened_with(opened, &common::settings()));
    let root_uuid = root_uuid(&repo_state);
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

fn root_uuid(repo: &RepoState) -> Uuid {
    let conn = repo.conn.lock().unwrap();
    metafolder_daemon::store::Rows::child_by_bytes(&*conn, "mfr_path", None, b"").unwrap().unwrap()
}

fn write_file(root: &Path, rel: &str, content: &[u8]) {
    let path = root.join(rel.trim_start_matches('/'));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn resolve(repo: &RepoState, path: &str) -> Uuid {
    let conn = repo.conn.lock().unwrap();
    let cache = repo.tree();
    cache.resolve_path(&conn, "mfr_path", path).unwrap().unwrap()
}

fn field_value(repo: &RepoState, uuid: Uuid, name: &str) -> Option<Value> {
    let conn = repo.conn.lock().unwrap();
    metafolder_daemon::store::Rows::metarecord(&*conn, uuid).unwrap().unwrap().get(name).cloned()
}

fn set_field(repo: &RepoState, uuid: Uuid, name: &str, value: Value) {
    let mut conn = repo.conn.lock().unwrap();
    let mut w = Writer::begin(&mut conn, None).unwrap();
    w.set_field(uuid, name, value).unwrap();
    w.commit().unwrap();
}

fn revisions(repo: &RepoState) -> i64 {
    let conn = repo.conn.lock().unwrap();
    metafolder_daemon::store::Log::counts(&*conn).unwrap().1
}

fn operations(repo: &RepoState) -> i64 {
    let conn = repo.conn.lock().unwrap();
    metafolder_daemon::store::Log::counts(&*conn).unwrap().0
}

fn version_of(repo: &RepoState, uuid: Uuid) -> Option<u64> {
    let conn = repo.conn.lock().unwrap();
    metafolder_daemon::store::Rows::version(&*conn, uuid).unwrap()
}

fn query(text: &str) -> metafolder_core::query::Query {
    metafolder_core::dsl::parse_query(text).unwrap()
}

fn hash(repo: &RepoState, text: &str, rehash: bool) -> HashResult {
    hashing::hash_query(repo, &query(text), &HashOptions { rehash }).unwrap()
}

fn string(v: &str) -> Option<Value> {
    Some(Value::String(v.into()))
}

/// The file's hashes as stored match its content, and their stamp its `stat`.
fn assert_hashed(repo: &RepoState, root: &Path, rel: &str) {
    let abs = root.join(rel.trim_start_matches('/'));
    let uuid = resolve(repo, rel);
    let full = fingerprint::full_hash(&abs).unwrap();
    let partial = fingerprint::partial_hash(&abs).unwrap();
    assert_eq!(field_value(repo, uuid, "mfr_full_hash"), string(&full), "{rel}");
    assert_eq!(field_value(repo, uuid, "mfr_partial_hash"), string(&partial), "{rel}");
    assert_eq!(field_value(repo, uuid, "mfr_hash_size"), field_value(repo, uuid, "mfr_size"));
    assert_eq!(field_value(repo, uuid, "mfr_hash_mtime"), field_value(repo, uuid, "mfr_mtime"));
}

// ── mf hash QUERY ────────────────────────────────────────────────────────────

#[test]
fn every_selected_file_gets_its_hashes_and_their_stamp() {
    let (repo, root) = setup("all");
    write_file(&root, "a.txt", b"alpha");
    write_file(&root, "sub/b.txt", b"beta, a little longer");
    reconcile::reconcile(&repo).unwrap();

    let result = hash(&repo, r#"mfr_type = "file""#, false);
    assert_eq!(result, HashResult { hashed: 2, reused: 0, skipped: 0 });
    assert_hashed(&repo, &root, "/a.txt");
    assert_hashed(&repo, &root, "/sub/b.txt");
}

#[test]
fn only_files_are_hashed_and_only_those_selected() {
    let (repo, root) = setup("selected");
    write_file(&root, "a.txt", b"alpha");
    write_file(&root, "sub/b.txt", b"beta");
    reconcile::reconcile(&repo).unwrap();
    let a = resolve(&repo, "/a.txt");
    let sub = resolve(&repo, "/sub");
    set_field(&repo, a, "pick", Value::Bool(true));
    set_field(&repo, sub, "pick", Value::Bool(true));

    let result = hash(&repo, "pick = true", false);
    assert_eq!(result, HashResult { hashed: 1, reused: 0, skipped: 0 }, "the directory is no file");
    assert_hashed(&repo, &root, "/a.txt");
    let b = resolve(&repo, "/sub/b.txt");
    assert_eq!(field_value(&repo, b, "mfr_full_hash"), None, "not selected");
    assert_eq!(field_value(&repo, sub, "mfr_full_hash"), None);
}

#[test]
fn a_fresh_hash_is_reused_without_writing() {
    let (repo, root) = setup("reuse");
    write_file(&root, "a.txt", b"alpha");
    reconcile::reconcile(&repo).unwrap();
    hash(&repo, r#"mfr_type = "file""#, false);
    let before = revisions(&repo);

    let result = hash(&repo, r#"mfr_type = "file""#, false);
    assert_eq!(result, HashResult { hashed: 0, reused: 1, skipped: 0 });
    assert_eq!(revisions(&repo), before, "nothing to write");
}

#[test]
fn a_file_changed_since_is_hashed_again() {
    let (repo, root) = setup("changed");
    write_file(&root, "a.txt", b"alpha");
    write_file(&root, "b.txt", b"beta");
    reconcile::reconcile(&repo).unwrap();
    hash(&repo, r#"mfr_type = "file""#, false);
    write_file(&root, "a.txt", b"alpha, rewritten to another size");
    reconcile::reconcile_full(&repo, None, false, false, true).unwrap();

    let result = hash(&repo, r#"mfr_type = "file""#, false);
    assert_eq!(result, HashResult { hashed: 1, reused: 1, skipped: 0 });
    assert_hashed(&repo, &root, "/a.txt");
}

#[test]
fn rehash_ignores_a_stored_hash_its_stamp_vouches_for() {
    let (repo, root) = setup("rehash");
    write_file(&root, "a.txt", b"alpha");
    reconcile::reconcile(&repo).unwrap();
    hash(&repo, r#"mfr_type = "file""#, false);
    let a = resolve(&repo, "/a.txt");
    // A wrong hash under a stamp that still matches: only --rehash sees it.
    set_field(&repo, a, "mfr_full_hash", Value::String("0000000000000000".into()));

    let result = hash(&repo, r#"mfr_type = "file""#, false);
    assert_eq!(result.reused, 1);
    assert_eq!(field_value(&repo, a, "mfr_full_hash"), string("0000000000000000"));

    let result = hash(&repo, r#"mfr_type = "file""#, true);
    assert_eq!(result, HashResult { hashed: 1, reused: 0, skipped: 0 });
    assert_hashed(&repo, &root, "/a.txt");
}

#[test]
fn a_file_gone_from_disk_is_skipped() {
    let (repo, root) = setup("gone");
    write_file(&root, "a.txt", b"alpha");
    reconcile::reconcile(&repo).unwrap();
    std::fs::remove_file(root.join("a.txt")).unwrap();

    let result = hash(&repo, r#"mfr_type = "file""#, false);
    assert_eq!(result, HashResult { hashed: 0, reused: 0, skipped: 1 });
}

// ── The background worker (doc "Content hashes kept fresh") ──────────────────────
//
// The worker thread only calls `hashing::drain`; the tests call it themselves,
// right after the flush, so nothing waits on time.

use metafolder_daemon::executor::{self, FsEvent};

fn mark(repo: &RepoState, rel: &str, on: bool) {
    let uuid = if rel == "/" { root_uuid(repo) } else { resolve(repo, rel) };
    set_field(repo, uuid, "mf_hash", Value::Bool(on));
}

fn flush_and_drain(repo: &RepoState, events: &[FsEvent]) {
    for ev in events {
        executor::enqueue(repo, ev.clone(), None);
    }
    executor::flush_pending(repo).unwrap();
    hashing::drain(repo).unwrap();
}

fn mtime_of(root: &Path, rel: &str) -> std::time::SystemTime {
    std::fs::metadata(root.join(rel.trim_start_matches('/'))).unwrap().modified().unwrap()
}

/// Rewrites `rel` with `content` and puts its mtime back: the same size and the
/// same mtime, which the stamp cannot tell from no change at all.
fn rewrite_unseen(root: &Path, rel: &str, content: &[u8]) {
    let when = mtime_of(root, rel);
    write_file(root, rel, content);
    let path = root.join(rel.trim_start_matches('/'));
    std::fs::File::options().write(true).open(path).unwrap().set_modified(when).unwrap();
}

#[test]
fn a_marked_file_modified_gets_its_hash_back() {
    let (repo, root) = setup("worker_modify");
    write_file(&root, "a.txt", b"alpha");
    reconcile::reconcile(&repo).unwrap();
    mark(&repo, "/", true);

    write_file(&root, "a.txt", b"alpha, now longer");
    flush_and_drain(&repo, &[FsEvent::ModifyData("/a.txt".into())]);
    assert_hashed(&repo, &root, "/a.txt");
}

#[test]
fn an_unmarked_file_modified_gets_no_hash() {
    let (repo, root) = setup("worker_unmarked");
    write_file(&root, "a.txt", b"alpha");
    write_file(&root, "keep/b.txt", b"beta");
    reconcile::reconcile(&repo).unwrap();
    mark(&repo, "/", true);
    mark(&repo, "/keep", false);

    write_file(&root, "keep/b.txt", b"beta, now longer");
    flush_and_drain(&repo, &[FsEvent::ModifyData("/keep/b.txt".into())]);
    let b = resolve(&repo, "/keep/b.txt");
    assert_eq!(field_value(&repo, b, "mfr_full_hash"), None, "opted out");
}

#[test]
fn a_file_created_under_a_marked_directory_is_hashed() {
    let (repo, root) = setup("worker_create");
    std::fs::create_dir_all(root.join("d")).unwrap();
    reconcile::reconcile(&repo).unwrap();
    mark(&repo, "/d", true);

    write_file(&root, "d/new.txt", b"fresh content");
    flush_and_drain(&repo, &[FsEvent::Create("/d/new.txt".into())]);
    assert_hashed(&repo, &root, "/d/new.txt");
}

#[test]
fn a_same_size_same_second_rewrite_is_seen_by_the_hash() {
    let (repo, root) = setup("worker_unseen");
    write_file(&root, "a.txt", b"alpha");
    reconcile::reconcile(&repo).unwrap();
    mark(&repo, "/", true);
    hash(&repo, r#"mfr_type = "file""#, false);

    rewrite_unseen(&root, "a.txt", b"omega");
    flush_and_drain(&repo, &[FsEvent::ModifyData("/a.txt".into())]);
    assert_hashed(&repo, &root, "/a.txt");
}

#[test]
fn an_echo_of_unchanged_content_keeps_the_hash_and_the_version() {
    let (repo, root) = setup("worker_echo");
    write_file(&root, "a.txt", b"alpha");
    reconcile::reconcile(&repo).unwrap();
    mark(&repo, "/", true);
    hash(&repo, r#"mfr_type = "file""#, false);
    let a = resolve(&repo, "/a.txt");
    let (version, ops) = (version_of(&repo, a), operations(&repo));

    flush_and_drain(&repo, &[FsEvent::ModifyData("/a.txt".into())]);
    assert_eq!(operations(&repo), ops, "nothing changed, nothing written");
    assert_eq!(version_of(&repo, a), version);
    assert_hashed(&repo, &root, "/a.txt");
}

#[test]
fn a_file_its_record_has_not_caught_up_with_is_left_to_its_event() {
    // Changed on disk, its event not flushed yet: a hash stamped with the new
    // stat would sit on a record still describing the old one.
    let (repo, root) = setup("worker_lag");
    write_file(&root, "a.txt", b"alpha");
    reconcile::reconcile(&repo).unwrap();
    mark(&repo, "/", true);
    let a = resolve(&repo, "/a.txt");

    write_file(&root, "a.txt", b"alpha, now longer");
    repo.hash_queue.push([a]);
    hashing::drain(&repo).unwrap();
    assert_eq!(field_value(&repo, a, "mfr_full_hash"), None);
}

// ── Reconcile hands over what has no fresh hash ──────────────────────────────────

#[test]
fn reconcile_hands_over_a_marked_file_without_a_hash() {
    let (repo, root) = setup("reconcile_missing");
    write_file(&root, "a.txt", b"alpha");
    write_file(&root, "plain/b.txt", b"beta");
    reconcile::reconcile(&repo).unwrap();
    mark(&repo, "/", true);
    mark(&repo, "/plain", false);

    reconcile::reconcile(&repo).unwrap();
    hashing::drain(&repo).unwrap();
    assert_hashed(&repo, &root, "/a.txt");
    let b = resolve(&repo, "/plain/b.txt");
    assert_eq!(field_value(&repo, b, "mfr_full_hash"), None, "unmarked: left alone");
}

#[test]
fn reconcile_hands_over_a_marked_file_whose_hash_is_stale() {
    // Rewritten while nothing watched: the refresh moves the record's stat,
    // and the stored hash no longer describes it.
    let (repo, root) = setup("reconcile_stale");
    write_file(&root, "a.txt", b"alpha");
    reconcile::reconcile(&repo).unwrap();
    mark(&repo, "/", true);
    hash(&repo, r#"mfr_type = "file""#, false);

    write_file(&root, "a.txt", b"alpha, rewritten unwatched");
    reconcile::reconcile_full(&repo, None, false, false, true).unwrap();
    hashing::drain(&repo).unwrap();
    assert_hashed(&repo, &root, "/a.txt");
}

#[test]
fn a_scoped_reconcile_hands_over_its_subtree_only() {
    let (repo, root) = setup("reconcile_scoped");
    write_file(&root, "in/a.txt", b"alpha");
    write_file(&root, "out/b.txt", b"beta");
    reconcile::reconcile(&repo).unwrap();
    mark(&repo, "/", true);

    let scope = resolve(&repo, "/in");
    reconcile::reconcile_metarecord(&repo, scope, false, false, false).unwrap();
    hashing::drain(&repo).unwrap();
    assert_hashed(&repo, &root, "/in/a.txt");
    let b = resolve(&repo, "/out/b.txt");
    assert_eq!(field_value(&repo, b, "mfr_full_hash"), None, "outside the scope");
}
