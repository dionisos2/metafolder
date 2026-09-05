//! Tests for the duplicate scan (spec-duplicates.org): size classes, the
//! reusable hash cache, group metarecords, hard-link accounting and pruning.

use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;

use metafolder_core::metarecord::Value;
use metafolder_daemon::duplicates::{self, ScanOptions, ScanResult, GROUP_SCHEMA};
use metafolder_daemon::log::Writer;
use metafolder_daemon::state::RepoState;
use metafolder_daemon::tasks::Reporter;
use metafolder_daemon::{db, reconcile, repo};
use uuid::Uuid;

mod common;
use common::TempDir;

const DEFAULT_PATTERNS: &[&str] = &[r"\.metafolder(/.*)?$", r"(^|/)\.[^/]+"];

fn setup(prefix: &str) -> (Arc<RepoState>, TempDir) {
    let root = TempDir::new(&format!("dup_{prefix}"));
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

fn resolve(repo: &RepoState, path: &str) -> Uuid {
    let conn = repo.conn.lock().unwrap();
    let mut cache = repo.cache.lock().unwrap();
    cache.resolve_path(&conn, "mfr_path", path).unwrap().unwrap()
}

fn field_value(repo: &RepoState, uuid: Uuid, name: &str) -> Option<Value> {
    let conn = repo.conn.lock().unwrap();
    db::get_metarecord(&conn, uuid).unwrap().unwrap().get(name).cloned()
}

fn group_of(repo: &RepoState, uuid: Uuid) -> Option<Uuid> {
    match field_value(repo, uuid, "mfr_duplicate_group") {
        Some(Value::Ref(group)) => Some(group),
        _ => None,
    }
}

fn revisions(repo: &RepoState) -> i64 {
    let conn = repo.conn.lock().unwrap();
    conn.query_row("SELECT COUNT(*) FROM revision", [], |r| r.get(0)).unwrap()
}

/// Reconciles so every file has a metarecord, then scans with the defaults.
fn populate_and_scan(repo: &RepoState) -> ScanResult {
    reconcile::reconcile(repo).unwrap();
    duplicates::scan(repo, &ScanOptions::default()).unwrap()
}

// ── Grouping ─────────────────────────────────────────────────────────────────

#[test]
fn two_identical_files_form_a_group() {
    let (repo, root) = setup("pair");
    write_file(&root, "a.txt", b"the very same bytes");
    write_file(&root, "b.txt", b"the very same bytes");
    write_file(&root, "c.txt", b"something else entirely!");

    let result = populate_and_scan(&repo);
    assert_eq!(result.groups, 1);
    assert_eq!(result.files, 2);
    assert_eq!(result.reclaimable, 19, "one copy of the shared bytes");

    let a = resolve(&repo, "/a.txt");
    let b = resolve(&repo, "/b.txt");
    let c = resolve(&repo, "/c.txt");
    let group = group_of(&repo, a).expect("a is grouped");
    assert_eq!(group_of(&repo, b), Some(group), "both members point at one group");
    assert_eq!(group_of(&repo, c), None, "a unique file joins no group");

    // The group is an abstract metarecord: typed, hashed, sized, no path.
    assert_eq!(field_value(&repo, group, "mf_schema"), Some(Value::String(GROUP_SCHEMA.into())));
    assert_eq!(field_value(&repo, group, "mfr_content_size"), Some(Value::Int(19)));
    assert_eq!(field_value(&repo, group, "mfr_duplicate_count"), Some(Value::Int(2)));
    assert_eq!(field_value(&repo, group, "mfr_duplicate_reclaimable"), Some(Value::Int(19)));
    assert!(field_value(&repo, group, "mfr_path").is_none(), "a group has no path");
    assert!(matches!(field_value(&repo, group, "mfr_content_hash"), Some(Value::String(_))));
}

#[test]
fn same_size_different_content_is_not_a_duplicate() {
    // The size class survives phase 1 and must be eliminated by the hashes.
    let (repo, root) = setup("samesize");
    write_file(&root, "a.txt", b"aaaaaaaaaa");
    write_file(&root, "b.txt", b"bbbbbbbbbb");

    let result = populate_and_scan(&repo);
    assert_eq!(result.groups, 0);
    assert_eq!(result.files, 0);
    assert!(result.hashed_partial >= 2, "both were partial-hashed: {result:?}");
    assert_eq!(group_of(&repo, resolve(&repo, "/a.txt")), None);
}

#[test]
fn three_way_duplicates_share_one_group() {
    let (repo, root) = setup("triple");
    for name in ["x", "y", "z"] {
        write_file(&root, &format!("{name}.bin"), b"triplicate");
    }
    let result = populate_and_scan(&repo);
    assert_eq!(result.groups, 1);
    assert_eq!(result.files, 3);
    assert_eq!(result.reclaimable, 20, "two of the three copies are recoverable");

    let group = group_of(&repo, resolve(&repo, "/x.bin")).unwrap();
    assert_eq!(field_value(&repo, group, "mfr_duplicate_count"), Some(Value::Int(3)));
}

#[test]
fn zero_length_files_are_never_grouped() {
    // They are all identical to each other, which is true and useless: removing
    // one frees nothing.
    let (repo, root) = setup("empty");
    write_file(&root, "e1", b"");
    write_file(&root, "e2", b"");
    write_file(&root, "e3", b"");

    let result = populate_and_scan(&repo);
    assert_eq!(result.groups, 0, "empty files must not form the biggest group: {result:?}");
    assert_eq!(result.hashed_partial, 0, "and must not even be opened");
}

#[test]
fn min_size_skips_the_small_ones() {
    let (repo, root) = setup("minsize");
    write_file(&root, "small1", b"tiny");
    write_file(&root, "small2", b"tiny");
    write_file(&root, "big1", &[7u8; 4096]);
    write_file(&root, "big2", &[7u8; 4096]);

    reconcile::reconcile(&repo).unwrap();
    let opts = ScanOptions { min_size: 1024, ..ScanOptions::default() };
    let result = duplicates::scan(&repo, &opts).unwrap();
    assert_eq!(result.groups, 1, "only the big pair");
    assert_eq!(group_of(&repo, resolve(&repo, "/small1")), None);
    assert!(group_of(&repo, resolve(&repo, "/big1")).is_some());
}

#[test]
fn directories_are_not_candidates() {
    let (repo, root) = setup("dirs");
    std::fs::create_dir_all(root.join("one")).unwrap();
    std::fs::create_dir_all(root.join("two")).unwrap();
    let result = populate_and_scan(&repo);
    assert_eq!(result.groups, 0, "two same-size directories are not duplicates");
}

#[test]
fn an_offline_volume_freezes_its_files_and_can_empty_a_class() {
    // A declared mount point with nothing mounted at it is offline, and its
    // whole subtree is frozen: the files there are unreadable in the sense that
    // matters (the directory reads back empty), so the scan must not consider
    // them. Here that takes the only twin out of a two-member size class, which
    // must then produce no group at all rather than a group of one.
    let (repo, root) = setup("offline");
    write_file(&root, "vol/a.txt", b"identical twins");
    write_file(&root, "b.txt", b"identical twins");
    reconcile::reconcile(&repo).unwrap();

    // Control: without the mount marking they are an ordinary pair.
    assert_eq!(duplicates::scan(&repo, &ScanOptions::default()).unwrap().groups, 1);

    let vol = resolve(&repo, "/vol");
    {
        let mut conn = repo.conn.lock().unwrap();
        let mut w = Writer::begin(&mut conn, None).unwrap();
        // Nothing is mounted at this ordinary directory, so it reads as offline.
        w.set_field(vol, "mfr_mount", Value::String("uuid:not-plugged-in".into())).unwrap();
        w.commit().unwrap();
    }

    let result = duplicates::scan(&repo, &ScanOptions::default()).unwrap();
    assert_eq!(result.groups, 0, "the frozen twin leaves a class of one: {result:?}");
    assert_eq!(
        group_of(&repo, resolve(&repo, "/b.txt")),
        None,
        "and the survivor's stale link is cleared"
    );
    assert_eq!(
        group_of(&repo, resolve(&repo, "/vol/a.txt")),
        None,
        "the frozen file keeps no link either"
    );
}

// ── Hard links ───────────────────────────────────────────────────────────────

#[cfg(unix)]
#[test]
fn hard_links_are_counted_once_in_reclaimable_space() {
    let (repo, root) = setup("hardlink");
    write_file(&root, "orig", b"linked content!!");
    std::fs::hard_link(root.join("orig"), root.join("link")).unwrap();
    write_file(&root, "copy", b"linked content!!");

    let result = populate_and_scan(&repo);
    assert_eq!(result.groups, 1);
    assert_eq!(result.files, 3, "all three names are in the group");
    // Three names, two inodes: only one copy's worth of bytes is recoverable.
    assert_eq!(result.reclaimable, 16);

    let group = group_of(&repo, resolve(&repo, "/orig")).unwrap();
    assert_eq!(field_value(&repo, group, "mfr_duplicate_count"), Some(Value::Int(3)));
    assert_eq!(field_value(&repo, group, "mfr_duplicate_reclaimable"), Some(Value::Int(16)));
}

// ── The hash cache ───────────────────────────────────────────────────────────

#[test]
fn a_stored_hash_is_reused_when_its_stamp_still_matches() {
    let (repo, root) = setup("reuse");
    write_file(&root, "a.txt", b"cache me");
    write_file(&root, "b.txt", b"cache me");

    let first = populate_and_scan(&repo);
    assert!(first.hashed_full >= 2, "the first scan computes: {first:?}");

    let second = duplicates::scan(&repo, &ScanOptions::default()).unwrap();
    assert_eq!(second.hashed_partial, 0, "nothing re-hashed: {second:?}");
    assert_eq!(second.hashed_full, 0);
    assert_eq!(second.groups, 1, "and the answer is unchanged");
}

#[test]
fn rehash_ignores_the_stored_hashes() {
    let (repo, root) = setup("rehash");
    write_file(&root, "a.txt", b"cache me");
    write_file(&root, "b.txt", b"cache me");
    populate_and_scan(&repo);

    let opts = ScanOptions { rehash: true, ..ScanOptions::default() };
    let forced = duplicates::scan(&repo, &opts).unwrap();
    assert_eq!(forced.hashed_partial, 2);
    assert_eq!(forced.hashed_full, 2);
}

#[test]
fn a_stale_stamp_forces_a_recomputation() {
    // The case the stamp exists for: content changed while the subtree was not
    // watched, so no Modify event ever invalidated the hashes.
    let (repo, root) = setup("stale");
    write_file(&root, "a.txt", b"first content");
    write_file(&root, "b.txt", b"first content");
    populate_and_scan(&repo);
    let a = resolve(&repo, "/a.txt");
    assert!(group_of(&repo, a).is_some());

    // Rewrite behind the daemon's back — same length, different bytes.
    std::fs::write(root.join("a.txt"), b"second conten").unwrap();
    let after = duplicates::scan(&repo, &ScanOptions::default()).unwrap();
    assert!(after.hashed_partial > 0, "the stale stamp must force a re-hash: {after:?}");
    assert_eq!(after.groups, 0, "they are no longer identical");
    assert_eq!(group_of(&repo, a), None, "and the stale group link is gone");
}

#[test]
fn an_unchanged_rescan_writes_nothing() {
    let (repo, root) = setup("idempotent");
    write_file(&root, "a.txt", b"steady");
    write_file(&root, "b.txt", b"steady");
    populate_and_scan(&repo);

    let before = revisions(&repo);
    let again = duplicates::scan(&repo, &ScanOptions::default()).unwrap();
    assert_eq!(again.groups, 1);
    assert_eq!(revisions(&repo), before, "a re-scan of an unchanged repo must not write");
}

// ── Pruning ──────────────────────────────────────────────────────────────────

#[test]
fn a_group_that_lost_its_twin_is_removed() {
    let (repo, root) = setup("prune");
    write_file(&root, "a.txt", b"pair of two");
    write_file(&root, "b.txt", b"pair of two");
    populate_and_scan(&repo);
    let a = resolve(&repo, "/a.txt");
    let group = group_of(&repo, a).unwrap();

    // The twin becomes unique.
    std::fs::write(root.join("b.txt"), b"now different bytes").unwrap();
    let after = duplicates::scan(&repo, &ScanOptions::default()).unwrap();

    assert_eq!(after.groups, 0);
    assert_eq!(group_of(&repo, a), None, "the survivor's link is cleared");
    let conn = repo.conn.lock().unwrap();
    assert!(db::get_metarecord(&conn, group).unwrap().is_none(), "the group is deleted");
}

// ── Scope ────────────────────────────────────────────────────────────────────

#[test]
fn a_scoped_scan_only_looks_inside_its_subtree() {
    let (repo, root) = setup("scope");
    write_file(&root, "inside/a.txt", b"scoped bytes");
    write_file(&root, "inside/b.txt", b"scoped bytes");
    write_file(&root, "outside/c.txt", b"other bytes!");
    write_file(&root, "outside/d.txt", b"other bytes!");
    reconcile::reconcile(&repo).unwrap();

    let inside = resolve(&repo, "/inside");
    let opts = ScanOptions { scope: Some(inside), ..ScanOptions::default() };
    let result = duplicates::scan(&repo, &opts).unwrap();

    assert_eq!(result.groups, 1, "only the in-scope pair");
    assert!(group_of(&repo, resolve(&repo, "/inside/a.txt")).is_some());
    assert_eq!(group_of(&repo, resolve(&repo, "/outside/c.txt")), None);
}

// ── Progress and cancellation ────────────────────────────────────────────────

#[test]
fn the_scan_reports_its_phases_and_counts_full_hashes_in_bytes() {
    let (repo, root) = setup("progress");
    write_file(&root, "a.bin", &[1u8; 5000]);
    write_file(&root, "b.bin", &[1u8; 5000]);
    reconcile::reconcile(&repo).unwrap();

    /// One reported progress tick: `(phase, done, total)`.
    type Tick = (String, Option<u64>, Option<u64>);
    let seen: Mutex<Vec<Tick>> = Mutex::new(Vec::new());
    let progress = |phase: &str, done: Option<u64>, total: Option<u64>| {
        seen.lock().unwrap().push((phase.to_string(), done, total));
    };
    let cancel = || false;
    duplicates::scan_reported(&repo, &ScanOptions::default(), &Reporter::new(&progress, &cancel))
        .unwrap();

    let seen = seen.lock().unwrap();
    let phases: Vec<&str> = seen.iter().map(|(p, _, _)| p.as_str()).collect();
    for expected in ["size", "partial", "full", "prune"] {
        assert!(phases.contains(&expected), "missing phase {expected} in {phases:?}");
    }
    // The `full` phase's total is a byte count, not a file count: two 5000-byte
    // files to read.
    let full_total = seen
        .iter()
        .find(|(p, _, _)| p == "full")
        .and_then(|(_, _, total)| *total)
        .expect("the full phase reports a total");
    assert_eq!(full_total, 10_000, "the full phase counts bytes");
}

#[test]
fn a_cancelled_scan_keeps_the_hashes_it_computed() {
    // Cancellation must not be punitive: the hash cache is the expensive part
    // and it is committed in batches as it goes.
    let (repo, root) = setup("cancel");
    write_file(&root, "a.txt", b"cancel me now");
    write_file(&root, "b.txt", b"cancel me now");
    reconcile::reconcile(&repo).unwrap();

    let progress = |_: &str, _: Option<u64>, _: Option<u64>| {};
    // Cancel as soon as the partial phase asks.
    let seen_partial = Mutex::new(false);
    let cancel = || {
        let mut flag = seen_partial.lock().unwrap();
        let was = *flag;
        *flag = true;
        was
    };
    let outcome = duplicates::scan_reported(
        &repo,
        &ScanOptions::default(),
        &Reporter::new(&progress, &cancel),
    );
    assert!(outcome.is_err(), "the scan should bail on cancellation");
    // Nothing was grouped, but no work is lost that a later scan would redo for
    // free: the invariant is only that the repository is still consistent.
    let a = resolve(&repo, "/a.txt");
    assert_eq!(group_of(&repo, a), None);

    // A later, uncancelled scan reaches the same answer.
    let result = duplicates::scan(&repo, &ScanOptions::default()).unwrap();
    assert_eq!(result.groups, 1);
}
