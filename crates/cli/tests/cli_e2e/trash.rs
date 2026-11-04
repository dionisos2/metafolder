use super::*;

// ── mf trash ────────────────────────────────────────────────────────────────

use metafolder_cli::trash::{Reason, TrashDir};

/// The repo's `internal/trash/` directory (matches the daemon-reported path).
fn repo_trash(root: &std::path::Path) -> TrashDir {
    TrashDir::new(root.join(".metafolder").join("internal").join("trash"))
}

// `mf trash list/restore/prune` over the real daemon: the CLI discovers the
// trash via `GET /repos/:repo` (internal_dir) and acts on it — no daemon
// endpoint is involved (doc "Trash").
#[test]
fn test_trash_list_restore_and_prune() {
    let (repo, root) = init_repo("trash");
    let trash = repo_trash(&root);

    // Seed the trash as a revert's overwrite would.
    let victim = root.join("victim.txt");
    std::fs::write(&victim, b"precious").unwrap();
    let e1 = trash.trash_path(&victim, Reason::Revert, Some(7), None, None).unwrap();
    let other = root.join("other.txt");
    std::fs::write(&other, b"stuff").unwrap();
    trash.trash_path(&other, Reason::Manual, None, None, None).unwrap();
    assert!(!victim.exists() && !other.exists(), "both were moved into the trash");

    // list shows both entries with their original names.
    let out = mf(&["-u", &repo, "trash", "list"]);
    assert_ok(&out);
    assert!(out.stdout.contains(&e1.id), "list shows the entry id");
    assert!(out.stdout.contains("victim.txt"), "list shows the original path");
    assert!(out.stdout.contains("other.txt"));

    // restore brings the victim back to its original path and drops the entry.
    let out = mf(&["-u", &repo, "trash", "restore", &e1.id]);
    assert_ok(&out);
    assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
    let out = mf(&["-u", &repo, "trash", "list"]);
    assert_ok(&out);
    assert!(!out.stdout.contains(&e1.id), "the restored entry is gone");

    // prune --all empties the rest.
    let out = mf(&["-u", &repo, "trash", "prune", "--all"]);
    assert_ok(&out);
    let out = mf(&["-u", &repo, "trash", "list"]);
    assert_ok(&out);
    assert!(out.stdout.contains("empty"), "trash is empty after prune --all");
}

// `mf trash restore` refuses an occupied target outright — no overwrite, no
// --force escape hatch. The occupant is left untouched and the entry stays.
#[test]
fn test_trash_restore_refuses_an_occupied_target() {
    let (repo, root) = init_repo("trashocc");
    let trash = repo_trash(&root);
    let doc = root.join("doc.txt");
    std::fs::write(&doc, b"old").unwrap();
    let e = trash.trash_path(&doc, Reason::Manual, None, None, None).unwrap();
    std::fs::write(&doc, b"new").unwrap(); // the path is occupied again

    let out = mf(&["-u", &repo, "trash", "restore", &e.id]);
    assert_eq!(out.code, 1, "an occupied target is refused (Op error)");
    assert!(out.stderr.contains("already exists"), "stderr: {}", out.stderr);
    assert_eq!(std::fs::read(&doc).unwrap(), b"new", "the occupant is untouched");

    // The entry survives the refusal; moving the occupant aside lets it restore.
    std::fs::remove_file(&doc).unwrap();
    let out = mf(&["-u", &repo, "trash", "restore", &e.id]);
    assert_ok(&out);
    assert_eq!(std::fs::read(&doc).unwrap(), b"old");
}

// `mf trash prune` with no selector is a usage error before any HTTP call.
#[test]
fn test_trash_prune_requires_a_selector() {
    let (repo, _root) = init_repo("trashsel");
    let out = mf(&["-u", &repo, "trash", "prune"]);
    assert_eq!(out.code, 2, "no -s/-d/--all is a usage error");

    let out = mf(&["-u", &repo, "trash", "prune", "-s", "1k", "--all"]);
    assert_eq!(out.code, 2, "two selectors are a usage error");
    assert!(out.stderr.contains("mutually exclusive"), "stderr: {}", out.stderr);
    let out = mf(&["-u", &repo, "trash", "prune", "-d", "soon"]);
    assert_eq!(out.code, 2, "an unparsable duration is a usage error");
}

// `mf trash prune -s` drops the oldest entries until the trash fits, `-d` the
// ones older than the duration, and `--dry-run` names what either would remove
// without removing it.
#[test]
fn test_trash_prune_by_size_and_age_and_dry_run() {
    let (repo, root) = init_repo("trashmodes");
    let trash = repo_trash(&root);
    for name in ["a.txt", "b.txt", "c.txt"] {
        let path = root.join(name);
        std::fs::write(&path, [b'x'; 100]).unwrap();
        trash.trash_path(&path, Reason::Manual, None, None, None).unwrap();
        // Distinct trashed_at, so "oldest first" has one answer.
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let left = || -> Vec<String> {
        trash.entries().unwrap().into_iter().map(|e| e.original_name).collect()
    };

    let out = mf(&["-u", &repo, "trash", "prune", "-s", "250", "--dry-run"]);
    assert_ok(&out);
    assert!(out.stdout.contains("would remove") && out.stdout.contains("a.txt"), "{}", out.stdout);
    assert!(!out.stdout.contains("b.txt"), "only the oldest is over budget: {}", out.stdout);
    assert!(out.stdout.contains("1 entrie(s)") && out.stdout.contains("dry-run"), "{}", out.stdout);
    assert_eq!(left().len(), 3, "a dry run removes nothing");

    let out = mf(&["-u", &repo, "trash", "prune", "-s", "250"]);
    assert_ok(&out);
    assert!(out.stdout.contains("removed 1 entrie(s)"), "{}", out.stdout);
    let mut names = left();
    names.sort();
    assert_eq!(names, ["b.txt", "c.txt"], "the oldest went first");

    let out = mf(&["-u", &repo, "trash", "prune", "-d", "1h"]);
    assert_ok(&out);
    assert!(out.stdout.contains("removed 0 entrie(s)"), "nothing is an hour old: {}", out.stdout);
    assert_eq!(left().len(), 2);

    let out = mf(&["-u", &repo, "trash", "prune", "-d", "0s"]);
    assert_ok(&out);
    assert!(
        out.stdout.contains("removed 2 entrie(s)"),
        "everything is older than now: {}",
        out.stdout
    );
    assert!(left().is_empty());
}

// `mf trash -f <file>` trashes a tracked file, recording its metarecord; it
// errors (before touching the file) when the file has no metarecord or the
// daemon is unreachable.
#[test]
fn test_trash_add_moves_a_tracked_file() {
    let (repo, root) = init_repo("trashadd");
    let file = root.join("doc.txt");
    std::fs::write(&file, b"data").unwrap();
    let uuid = mf(&["-u", &repo, "metarecord", "add", "--path", file.to_str().unwrap()])
        .stdout
        .trim()
        .to_string();
    assert!(is_hex_uuid(&uuid));

    let out = mf(&["-u", &repo, "trash", "-f", file.to_str().unwrap()]);
    assert_ok(&out);
    assert!(!file.exists(), "the file was moved into the trash");

    // The entry records the associated metarecord and reason=manual.
    let entries = repo_trash(&root).entries().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].reason, Reason::Manual);
    assert_eq!(entries[0].metarecord.as_deref(), Some(uuid.as_str()));

    let out = mf(&["-u", &repo, "trash", "list"]);
    assert_ok(&out);
    assert!(out.stdout.contains("doc.txt") && out.stdout.contains("manual"));
}

/// A symbolic link is trashed as the link, never its target (doc "What
/// trashing does"): the path names the link, not what it points at.
#[test]
fn test_trash_add_takes_a_symlink_not_its_target() {
    let (repo, root) = init_repo("trashadd_link");
    let target = root.join("target.txt");
    std::fs::write(&target, b"data").unwrap();
    let link = root.join("link.txt");
    std::os::unix::fs::symlink("target.txt", &link).unwrap();
    for path in [&target, &link] {
        let out = mf(&["-u", &repo, "metarecord", "add", "--path", path.to_str().unwrap()]);
        assert_ok(&out);
    }
    let link_uuid = mf(&["-u", &repo, "uuid", link.to_str().unwrap()]);
    assert_ok(&link_uuid);

    let out = mf(&["-u", &repo, "trash", "-f", link.to_str().unwrap()]);
    assert_ok(&out);
    assert!(link.symlink_metadata().is_err(), "the link went to the trash");
    assert_eq!(std::fs::read(&target).unwrap(), b"data", "its target stayed");
    let entries = repo_trash(&root).entries().unwrap();
    assert_eq!(entries[0].metarecord.as_deref(), Some(link_uuid.stdout.trim()));
}

#[test]
fn test_trash_add_rejects_an_untracked_file() {
    let (repo, root) = init_repo("trashadd_untracked");
    let file = root.join("loose.txt");
    std::fs::write(&file, b"x").unwrap();

    let out = mf(&["-u", &repo, "trash", "-f", file.to_str().unwrap()]);
    assert_eq!(out.code, 1, "stderr: {}", out.stderr);
    assert!(out.stderr.contains("no metarecord"), "stderr: {}", out.stderr);
    assert!(file.exists(), "the untracked file is left in place");
}

#[test]
fn test_trash_add_rejects_a_missing_file() {
    let (repo, root) = init_repo("trashadd_missing");
    let out = mf(&["-u", &repo, "trash", "-f", root.join("nope.txt").to_str().unwrap()]);
    assert_eq!(out.code, 1, "stderr: {}", out.stderr);
}

#[test]
fn test_orphan_list_and_clear() {
    let (repo, root) = init_repo("orphan");
    let file = root.join("gone.txt");
    std::fs::write(&file, b"data").unwrap();
    let uuid = mf(&["-u", &repo, "metarecord", "add", "--path", file.to_str().unwrap()])
        .stdout
        .trim()
        .to_string();
    assert!(is_hex_uuid(&uuid));

    // With the file present, nothing is orphaned.
    let out = mf(&["-u", &repo, "orphan", "list"]);
    assert_ok(&out);
    assert!(out.stdout.contains("No orphans"), "stdout: {}", out.stdout);

    // Delete the file behind the daemon's back → stale mfr_path.
    std::fs::remove_file(&file).unwrap();

    // `orphan` defaults to `list` and surfaces the record with its stale path.
    let out = mf(&["-u", &repo, "orphan"]);
    assert_ok(&out);
    assert!(out.stdout.contains(&uuid), "stdout: {}", out.stdout);
    assert!(out.stdout.contains("/gone.txt"), "stdout: {}", out.stdout);

    // Clear it (‑y skips the prompt); the count is reported.
    let out = mf(&["-u", &repo, "orphan", "clear", "-y"]);
    assert_ok(&out);
    assert!(out.stdout.contains("cleared 1"), "stdout: {}", out.stdout);

    // mfr_path is now Nothing and the origin is frozen in mfr_path_old.
    let entry = get_entries(&repo, &uuid);
    let field = |name: &str| {
        entry[0]["fields"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["name"] == name)
            .map(|f| &f["value"])
    };
    assert_eq!(field("mfr_path"), Some(&serde_json::json!({"type": "nothing"})));
    assert_eq!(
        field("mfr_path_old"),
        Some(&serde_json::json!({"type": "string", "value": "/gone.txt"})),
    );

    // Nothing left to clear.
    let out = mf(&["-u", &repo, "orphan", "list"]);
    assert_ok(&out);
    assert!(out.stdout.contains("No orphans"), "stdout: {}", out.stdout);
}

#[test]
fn test_orphan_detect_marks_and_unmarks() {
    let (repo, root) = init_repo("orphan-detect");
    let file = root.join("gone.txt");
    std::fs::write(&file, b"data").unwrap();
    let uuid = mf(&["-u", &repo, "metarecord", "add", "--path", file.to_str().unwrap()])
        .stdout
        .trim()
        .to_string();

    // Nothing is gone yet: the marker is not written.
    let out = mf(&["-u", &repo, "orphan", "detect"]);
    assert_ok(&out);
    assert!(out.stdout.contains("0 orphan"), "stdout: {}", out.stdout);

    std::fs::remove_file(&file).unwrap();
    let out = mf(&["-u", &repo, "orphan", "detect"]);
    assert_ok(&out);
    assert!(out.stdout.contains("1 orphan"), "stdout: {}", out.stdout);
    assert!(out.stdout.contains("1 marked"), "stdout: {}", out.stdout);

    // The marker is an ordinary field, so an ordinary query finds it.
    let out = mf(&["-u", &repo, "metarecord", "-q", "orphan = true", "get"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), uuid, "stdout: {}", out.stdout);

    // The file comes back: the next run takes the marker back.
    std::fs::write(&file, b"data").unwrap();
    let out = mf(&["-u", &repo, "orphan", "detect"]);
    assert_ok(&out);
    assert!(out.stdout.contains("1 unmarked"), "stdout: {}", out.stdout);
    let out = mf(&["-u", &repo, "metarecord", "-q", "orphan = true", "get"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "", "stdout: {}", out.stdout);
}

#[test]
fn test_trash_add_requires_a_running_daemon() {
    // Port 1: nothing listening. The daemon check (repo_info) fails before any
    // filesystem move is attempted, so the path need not even exist.
    let out = mf_full(
        &["-p", "1", "-u", &Uuid::new_v4().as_simple().to_string(), "trash", "-f", "/tmp/whatever"],
        None,
        &[],
        false,
    );
    assert_eq!(out.code, 1, "stderr: {}", out.stderr);
    assert!(out.stderr.starts_with("error:"), "stderr: {}", out.stderr);
}

// `mf trash -f <dir>` trashes a whole tracked directory (subtree and all).
#[test]
fn test_trash_add_moves_a_directory() {
    let (repo, root) = init_repo("trashdir");
    let dir = root.join("folder");
    std::fs::create_dir_all(dir.join("nested")).unwrap();
    std::fs::write(dir.join("a.txt"), b"aaa").unwrap();
    std::fs::write(dir.join("nested/b.txt"), b"bb").unwrap();
    let uuid = mf(&["-u", &repo, "metarecord", "add", "--path", dir.to_str().unwrap()])
        .stdout
        .trim()
        .to_string();
    assert!(is_hex_uuid(&uuid));

    let out = mf(&["-u", &repo, "trash", "-f", dir.to_str().unwrap()]);
    assert_ok(&out);
    assert!(!dir.exists(), "the directory was moved into the trash");

    let entries = repo_trash(&root).entries().unwrap();
    assert_eq!(entries.len(), 1);
    assert!(entries[0].is_dir, "the entry is a directory");
    assert_eq!(entries[0].size, 5, "recursive size (3 + 2 bytes)");

    // list marks the directory with a trailing slash.
    let out = mf(&["-u", &repo, "trash", "list"]);
    assert_ok(&out);
    assert!(out.stdout.contains("folder/"), "list output: {}", out.stdout);

    // restore brings the whole subtree back.
    let out = mf(&["-u", &repo, "trash", "restore", &entries[0].id]);
    assert_ok(&out);
    assert_eq!(std::fs::read(dir.join("nested/b.txt")).unwrap(), b"bb");
}

// Restoring a trashed *directory* re-links its whole subtree, not just the top
// metarecord: every orphaned descendant is put back where it was (doc "Trash"),
// so no metarecord is left orphaned and no duplicate is created.
#[test]
fn test_trash_restore_relinks_a_directory_subtree() {
    let (repo, root) = init_repo("trashdirsubtree");
    let dir = root.join("folder");
    std::fs::create_dir_all(dir.join("nested")).unwrap();
    std::fs::write(dir.join("nested/b.txt"), b"bb").unwrap();
    // Record the whole subtree (folder, nested, b.txt): --parents creates them.
    let b_uuid = mf(&[
        "-u",
        &repo,
        "metarecord",
        "add",
        "--parents",
        "--path",
        dir.join("nested/b.txt").to_str().unwrap(),
    ])
    .stdout
    .trim()
    .to_string();
    assert!(is_hex_uuid(&b_uuid));

    // Trash the directory (captures the subtree), then orphan every subtree
    // metarecord as the watcher's delete cascade would (descendants first, then
    // the directory, so the transitive query still resolves).
    assert_ok(&mf(&["-u", &repo, "trash", "-f", dir.to_str().unwrap()]));
    assert_ok(&mf(&[
        "-u",
        &repo,
        "metarecord",
        "-q",
        "mfr_path ->* \"/folder\"",
        "field",
        "unset",
        "mfr_path",
        "--force",
    ]));
    assert_ok(&mf(&[
        "-u",
        &repo,
        "metarecord",
        "-q",
        "mfr_path:value = \"folder\"",
        "field",
        "unset",
        "mfr_path",
        "--force",
    ]));
    // The descendant is now orphaned (no resolvable path).
    assert_ne!(mf(&["-u", &repo, "path", &b_uuid]).code, 0, "b.txt is orphaned before restore");

    let entry_id = repo_trash(&root).entries().unwrap()[0].id.clone();
    assert_ok(&mf(&["-u", &repo, "trash", "restore", &entry_id]));
    assert_eq!(std::fs::read(dir.join("nested/b.txt")).unwrap(), b"bb");

    // The *original* descendant metarecord is re-linked at its old location — not
    // left orphaned, and not replaced by a fresh duplicate.
    let out = mf(&["-u", &repo, "path", &b_uuid]);
    assert_ok(&out);
    assert!(
        out.stdout.contains("folder/nested/b.txt"),
        "the subtree metarecord must be re-linked, got: {}",
        out.stdout,
    );
}

// When another metarecord already holds the restored item's tree position, the
// re-link conflict is expected and skipped (that metarecord tracks the bytes) —
// the restore still succeeds rather than failing on the constraint error.
#[test]
fn test_trash_restore_skips_a_taken_tree_position() {
    let (repo, root) = init_repo("trashtaken");
    let file = root.join("A.txt");
    std::fs::write(&file, b"data").unwrap();
    let m1 = mf(&["-u", &repo, "metarecord", "add", "--path", file.to_str().unwrap()])
        .stdout
        .trim()
        .to_string();
    assert_ok(&mf(&["-u", &repo, "trash", "-f", file.to_str().unwrap()]));

    // The original's metarecord went with the bytes. Let a *different* one
    // claim A.txt's slot while the bytes sit in the trash.
    let root_uuid = mf(&["-u", &repo, "metarecord", "-q", "mfr_type = \"dir\"", "get"])
        .stdout
        .trim()
        .to_string();
    let m2 = create_metarecord(&repo, &["label:string=other"]);
    assert_ok(&mf(&[
        "-u",
        &repo,
        "metarecord",
        "-i",
        &m2,
        "field",
        "set",
        &format!("mfr_path:tree_ref={root_uuid}/A.txt"),
        "--force",
    ]));

    let entry_id = repo_trash(&root).entries().unwrap()[0].id.clone();
    let out = mf(&["-u", &repo, "trash", "restore", &entry_id]);
    assert_ok(&out); // the conflict is skipped, not a hard error
    assert_eq!(std::fs::read(&file).unwrap(), b"data");
    // m1 cannot be recreated — its position is taken — and that is skipped
    // rather than failing the restore; m2 keeps the slot.
    assert_ne!(mf(&["-u", &repo, "path", &m1]).code, 0, "m1 not put back");
    assert!(mf(&["-u", &repo, "path", &m2]).stdout.contains("A.txt"), "m2 keeps the slot");
}

/// Polls `f` every 200 ms up to `tries` times; returns whether it ever held.
pub(super) fn poll(tries: u32, f: impl Fn() -> bool) -> bool {
    for _ in 0..tries {
        if f() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    false
}

/// The uuids tracking repo-relative top-level `name`, one per line of
/// `metarecord -q 'mfr_path = "name"' get`.
pub(super) fn uuids_at(repo: &str, name: &str) -> Vec<String> {
    let out = mf(&["-u", repo, "metarecord", "-q", &format!("mfr_path:value = {name:?}"), "get"]);
    out.stdout.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect()
}

// Regression (doc "What restoring does"): trashing a file through the metarecord path
// (as the GUI's `metarecord:trash` and `mf trash -f` do) while the **live
// watcher** is running, then restoring it, must re-link the *original*
// metarecord — not orphan it and create a duplicate. The existing subtree tests
// orphan the metarecords by hand (`field unset`); this one lets the real watcher
// cascade the delete and process the restored file's arrival.
#[test]
fn test_trash_restore_relinks_after_live_watcher_delete() {
    let (repo, root) = init_repo("trashwatch");
    let root_uuid = mf(&["-u", &repo, "metarecord", "-q", "mfr_type = \"dir\"", "get"])
        .stdout
        .trim()
        .to_string();
    assert_ok(&mf(&[
        "-u",
        &repo,
        "metarecord",
        "-i",
        &root_uuid,
        "field",
        "set",
        "mf_watch:bool=true",
    ]));

    // The watcher tracks a file created under the watched root (a watcher-born
    // metarecord: no stored fingerprint hashes, exactly like the user's file).
    let file = root.join("f.txt");
    std::fs::write(&file, b"hello").unwrap();
    assert!(poll(40, || uuids_at(&repo, "f.txt").len() == 1), "watcher should track f.txt");
    let m = uuids_at(&repo, "f.txt")[0].clone();

    // Trash it through the metarecord-capturing path, then let the watcher
    // observe the deletion and orphan the metarecord (as the GUI flow does).
    assert_ok(&mf(&["-u", &repo, "trash", "-f", file.to_str().unwrap()]));
    assert!(!file.exists());
    assert!(
        poll(40, || mf(&["-u", &repo, "path", &m]).code != 0),
        "watcher should orphan the record"
    );

    // Restore, then let the watcher process the file's re-arrival.
    let id = repo_trash(&root).entries().unwrap()[0].id.clone();
    assert_ok(&mf(&["-u", &repo, "trash", "restore", &id]));
    assert!(file.exists(), "the file is back on disk");

    // The ORIGINAL metarecord must track f.txt again, and it must be the *only*
    // one — no orphaned original left behind, no fresh duplicate created.
    assert!(
        poll(40, || mf(&["-u", &repo, "path", &m]).stdout.contains("f.txt")),
        "the original metarecord must be re-linked to f.txt",
    );
    std::thread::sleep(std::time::Duration::from_millis(1200)); // let any duplicate settle
    let hits = uuids_at(&repo, "f.txt");
    assert_eq!(
        hits,
        vec![m],
        "exactly the original metarecord tracks f.txt (no duplicate): {hits:?}"
    );
}

// Same as above, but the restore happens *immediately* after trashing — before
// the watcher's 500 ms quiet window has flushed the deletion. The delete and the
// re-arrival then land in one batch, racing the re-link.
#[test]
fn test_trash_restore_relinks_when_restore_races_the_watcher() {
    let (repo, root) = init_repo("trashwatchrace");
    let root_uuid = mf(&["-u", &repo, "metarecord", "-q", "mfr_type = \"dir\"", "get"])
        .stdout
        .trim()
        .to_string();
    assert_ok(&mf(&[
        "-u",
        &repo,
        "metarecord",
        "-i",
        &root_uuid,
        "field",
        "set",
        "mf_watch:bool=true",
    ]));

    let file = root.join("f.txt");
    std::fs::write(&file, b"hello").unwrap();
    assert!(poll(40, || uuids_at(&repo, "f.txt").len() == 1), "watcher should track f.txt");
    let m = uuids_at(&repo, "f.txt")[0].clone();

    // Trash and restore back-to-back — no wait, so the deletion has not flushed.
    assert_ok(&mf(&["-u", &repo, "trash", "-f", file.to_str().unwrap()]));
    let id = repo_trash(&root).entries().unwrap()[0].id.clone();
    assert_ok(&mf(&["-u", &repo, "trash", "restore", &id]));
    assert!(file.exists(), "the file is back on disk");

    // The original metarecord must still be the only one tracking f.txt.
    std::thread::sleep(std::time::Duration::from_millis(1500)); // let the watcher settle
    let hits = uuids_at(&repo, "f.txt");
    assert_eq!(
        hits,
        vec![m],
        "exactly the original metarecord tracks f.txt (no duplicate): {hits:?}"
    );
}

// Restoring a nested file whose *ancestor* metarecord is no longer available
// (deleted, e.g. by sweeping orphans) must still bring the bytes back: the
// descendant's re-link fails with "invalid TreeRef parent", which is benign —
// it is skipped (the watcher re-tracks), not surfaced as a hard error.
#[test]
fn test_trash_restore_tolerates_an_unavailable_ancestor() {
    let (repo, root) = init_repo("trashanc");
    let dir = root.join("A");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("B.txt"), b"b").unwrap();
    let b = mf(&[
        "-u",
        &repo,
        "metarecord",
        "add",
        "--parents",
        "--path",
        dir.join("B.txt").to_str().unwrap(),
    ])
    .stdout
    .trim()
    .to_string();
    let a = mf(&["-u", &repo, "metarecord", "-q", "mfr_path:value = \"A\"", "get"])
        .stdout
        .trim()
        .to_string();
    assert!(is_hex_uuid(&b) && is_hex_uuid(&a));

    // Trash the file (capturing ancestor A while live, and taking B's own
    // metarecord with it), then delete A's metarecord. B can no longer be
    // recreated: the parent its capture names is not a live node any more.
    assert_ok(&mf(&["-u", &repo, "trash", "-f", dir.join("B.txt").to_str().unwrap()]));
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &a, "delete"]));
    let _ = &b;

    let id = repo_trash(&root).entries().unwrap()[0].id.clone();
    let out = mf(&["-u", &repo, "trash", "restore", &id]);
    assert_ok(&out); // the unavailable parent is skipped, not a hard error
    assert_eq!(std::fs::read(dir.join("B.txt")).unwrap(), b"b");
}

// Trashing a directory takes the whole subtree's metarecords with it, and
// restoring brings every one of them back at its own uuid — the guarantee the
// old design could not make, since it left the subtree orphaned for anything
// sweeping orphans to destroy (doc "Trash").
#[test]
fn test_trash_and_restore_a_directory_round_trips_the_whole_subtree() {
    let (repo, root) = init_repo("trashsubtree");
    let dir = root.join("D");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("one.txt"), b"1").unwrap();
    std::fs::write(dir.join("two.txt"), b"2").unwrap();
    let one = mf(&[
        "-u",
        &repo,
        "metarecord",
        "add",
        "--parents",
        "--path",
        dir.join("one.txt").to_str().unwrap(),
    ])
    .stdout
    .trim()
    .to_string();
    let two = mf(&[
        "-u",
        &repo,
        "metarecord",
        "add",
        "--parents",
        "--path",
        dir.join("two.txt").to_str().unwrap(),
    ])
    .stdout
    .trim()
    .to_string();
    let d = mf(&["-u", &repo, "metarecord", "-q", "mfr_path:value = \"D\"", "get"])
        .stdout
        .trim()
        .to_string();
    assert!(is_hex_uuid(&one) && is_hex_uuid(&two) && is_hex_uuid(&d));

    assert_ok(&mf(&["-u", &repo, "trash", "-f", dir.to_str().unwrap()]));
    for uuid in [&d, &one, &two] {
        assert_ne!(
            mf(&["-u", &repo, "metarecord", "-i", uuid, "get"]).code,
            0,
            "the whole subtree goes, not just the directory"
        );
    }

    let entry_id = repo_trash(&root).entries().unwrap()[0].id.clone();
    assert_ok(&mf(&["-u", &repo, "trash", "restore", &entry_id]));
    assert_eq!(std::fs::read(dir.join("one.txt")).unwrap(), b"1");
    assert_eq!(std::fs::read(dir.join("two.txt")).unwrap(), b"2");
    for uuid in [&d, &one, &two] {
        assert_ok(&mf(&["-u", &repo, "metarecord", "-i", uuid, "get"]));
    }
    assert!(mf(&["-u", &repo, "path", &one]).stdout.contains("D/one.txt"), "back in place");
}

// Reverting a trashing brings the *bytes* back too, not just the metarecord.
// A revert writes the inverse forward rather than rewinding HEAD, and nothing
// in `delete_metarecord` says a file is involved — an ordinary one touches no
// file at all. The write's `origin` is what tells the client to go looking
// in the trash-bin (doc "Trash, undo and redo").
#[test]
fn test_revert_of_a_trashing_brings_the_file_back() {
    let (repo, root) = init_repo("trashrevert");
    let file = root.join("r.txt");
    std::fs::write(&file, b"data").unwrap();
    let uuid = mf(&["-u", &repo, "metarecord", "add", "--path", file.to_str().unwrap()])
        .stdout
        .trim()
        .to_string();
    assert!(is_hex_uuid(&uuid));

    assert_ok(&mf(&["-u", &repo, "trash", "-f", file.to_str().unwrap()]));
    assert!(!file.exists());
    assert_eq!(repo_trash(&root).entries().unwrap().len(), 1);

    assert_ok(&mf(&["-u", &repo, "log", "revert", "--force"]));
    assert_eq!(std::fs::read(&file).unwrap(), b"data", "the bytes come back");
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &uuid, "get"]));
    assert!(mf(&["-u", &repo, "path", &uuid]).stdout.contains("r.txt"), "and its place");
    assert!(repo_trash(&root).entries().unwrap().is_empty(), "the entry is consumed");
}

// Restoring a nested file whose parent directory was *also* trashed re-links
// the original ancestor directory metarecords too (captured at trash time), so
// the recreated parent directory is tracked by the original metarecord rather
// than left orphaned for the watcher to duplicate (doc "What restoring does").
#[test]
fn test_trash_restore_relinks_ancestors_of_a_nested_file() {
    let (repo, root) = init_repo("trashancestor");
    let dir = root.join("A");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("B.txt"), b"b").unwrap();
    // Record B (--parents creates the A metarecord).
    let b_uuid = mf(&[
        "-u",
        &repo,
        "metarecord",
        "add",
        "--parents",
        "--path",
        dir.join("B.txt").to_str().unwrap(),
    ])
    .stdout
    .trim()
    .to_string();
    let a_uuid = mf(&["-u", &repo, "metarecord", "-q", "mfr_path:value = \"A\"", "get"])
        .stdout
        .trim()
        .to_string();
    assert!(is_hex_uuid(&b_uuid) && is_hex_uuid(&a_uuid));

    // Trash the file (capturing its ancestor A while A is still live), then the
    // directory. Each trashing takes its own metarecords with it.
    assert_ok(&mf(&["-u", &repo, "trash", "-f", dir.join("B.txt").to_str().unwrap()]));
    assert_ok(&mf(&["-u", &repo, "trash", "-f", dir.to_str().unwrap()]));
    assert!(!dir.exists());

    // Restore the file: the recreated parent directory A is re-linked to the
    // original A metarecord (its ancestor), not left orphaned.
    let file_entry = repo_trash(&root)
        .entries()
        .unwrap()
        .into_iter()
        .find(|e| !e.is_dir)
        .expect("the file entry")
        .id;
    assert_ok(&mf(&["-u", &repo, "trash", "restore", &file_entry]));
    assert_eq!(std::fs::read(dir.join("B.txt")).unwrap(), b"b");
    assert!(mf(&["-u", &repo, "path", &b_uuid]).stdout.contains("A/B.txt"), "B re-linked");
    let a_path = mf(&["-u", &repo, "path", &a_uuid]);
    assert_ok(&a_path);
    assert!(a_path.stdout.trim().ends_with("/A"), "ancestor A re-linked, got: {}", a_path.stdout);
}

// Restoring a directory whose target already exists merges the blob's contents
// into it (a directory is a container, not data) instead of refusing — the
// dead-end when part of a directory was restored first (doc "What restoring does").
#[test]
fn test_trash_restore_merges_a_directory_into_an_existing_one() {
    let (repo, root) = init_repo("trashmerge");
    let dir = root.join("A");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("c.txt"), b"c").unwrap();
    assert_ok(&mf(&[
        "-u",
        &repo,
        "metarecord",
        "add",
        "--parents",
        "--path",
        dir.join("c.txt").to_str().unwrap(),
    ]));

    assert_ok(&mf(&["-u", &repo, "trash", "-f", dir.to_str().unwrap()]));
    assert!(!dir.exists(), "the directory moved into the trash");
    // The directory is recreated meanwhile with a different file (as a partial
    // restore of a file inside it would leave it).
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("b.txt"), b"b").unwrap();

    let entry_id = repo_trash(&root).entries().unwrap()[0].id.clone();
    let out = mf(&["-u", &repo, "trash", "restore", &entry_id]);
    assert_ok(&out); // no longer a dead-end
    assert_eq!(std::fs::read(dir.join("b.txt")).unwrap(), b"b", "pre-existing kept");
    assert_eq!(std::fs::read(dir.join("c.txt")).unwrap(), b"c", "restored into it");
}

// `mf trash restore` re-links the associated metarecord authoritatively: after
// the file is back, the orphaned metarecord's mfr_path is restored (H).
#[test]
fn test_trash_restore_relinks_the_metarecord() {
    let (repo, root) = init_repo("trashrelink");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    let file = root.join("sub/file.txt");
    std::fs::write(&file, b"data").unwrap();
    let uuid =
        mf(&["-u", &repo, "metarecord", "add", "--parents", "--path", file.to_str().unwrap()])
            .stdout
            .trim()
            .to_string();
    assert!(is_hex_uuid(&uuid));

    assert_ok(&mf(&["-u", &repo, "trash", "-f", file.to_str().unwrap()]));
    // The metarecord goes with the bytes: no orphan is left behind.
    assert_ne!(
        mf(&["-u", &repo, "metarecord", "-i", &uuid, "get"]).code,
        0,
        "deleted, not orphaned"
    );
    let entry_id = repo_trash(&root).entries().unwrap()[0].id.clone();

    let out = mf(&["-u", &repo, "trash", "restore", &entry_id]);
    assert_ok(&out);
    assert_eq!(std::fs::read(&file).unwrap(), b"data");
    // mfr_path is authoritatively back: `mf path` resolves the original location.
    let out = mf(&["-u", &repo, "path", &uuid]);
    assert_ok(&out);
    assert!(out.stdout.contains("sub/file.txt"), "path output: {}", out.stdout);
}

// Regression: restoring a *top-level* file (directly in the repo root) must
// re-link its metarecord under the filesystem root metarecord — exactly as
// reconcile does (`ensure_parent_metarecords` starts from the root) — not with
// parent = None, which forges a second forest root and leaves the file to be
// re-tracked as a duplicate.
#[test]
fn test_trash_restore_relinks_a_top_level_file() {
    let (repo, root) = init_repo("trashrelinktop");
    let file = root.join("top.txt");
    std::fs::write(&file, b"data").unwrap();
    let uuid = mf(&["-u", &repo, "metarecord", "add", "--path", file.to_str().unwrap()])
        .stdout
        .trim()
        .to_string();
    assert!(is_hex_uuid(&uuid));

    // The filesystem root metarecord (the only directory) is the expected parent.
    let root_uuid = mf(&["-u", &repo, "metarecord", "-q", "mfr_type = \"dir\"", "get"])
        .stdout
        .trim()
        .to_string();
    assert!(is_hex_uuid(&root_uuid), "one dir (the fs root), got: {root_uuid}");

    assert_ok(&mf(&["-u", &repo, "trash", "-f", file.to_str().unwrap()]));
    assert_ne!(
        mf(&["-u", &repo, "metarecord", "-i", &uuid, "get"]).code,
        0,
        "deleted, not orphaned"
    );
    let entry_id = repo_trash(&root).entries().unwrap()[0].id.clone();

    assert_ok(&mf(&["-u", &repo, "trash", "restore", &entry_id]));
    assert_eq!(std::fs::read(&file).unwrap(), b"data");

    // mfr_path is back, and its parent is the fs root metarecord — not None.
    let rec = get_entries(&repo, &uuid);
    let obj = rec.get(0).unwrap_or(&rec);
    let fields = obj["fields"].as_array().expect("fields array");
    let mfr_path = fields.iter().find(|f| f["name"] == "mfr_path").expect("mfr_path present");
    assert_eq!(
        mfr_path["value"]["value"]["parent"].as_str(),
        Some(root_uuid.as_str()),
        "top-level restore must re-link under the fs root, got {}",
        mfr_path["value"],
    );
}

/// Polls `mf <args>` until `pred(stdout)` holds, up to ~10 s. Returns the
/// matching stdout (trimmed). Panics on timeout.
pub(super) fn poll_mf(args: &[&str], pred: impl Fn(&str) -> bool) -> String {
    for _ in 0..100 {
        let out = mf(args);
        if out.code == 0 && pred(out.stdout.trim()) {
            return out.stdout.trim().to_string();
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("timed out polling mf {args:?}");
}

// `mf trash -f` records the metarecord's current version on the entry, so a
// later rollback can correlate it (deterministic, no watcher needed here).
#[test]
fn test_trash_add_records_the_metarecord_version() {
    let (repo, root) = init_repo("trashver");
    let file = root.join("doc.txt");
    std::fs::write(&file, b"data").unwrap();
    let uuid = mf(&["-u", &repo, "metarecord", "add", "--path", file.to_str().unwrap()])
        .stdout
        .trim()
        .to_string();

    // The metarecord's version, straight from the daemon.
    let rec = mf(&["-u", &repo, "metarecord", "-i", &uuid, "get"]);
    assert_ok(&rec);
    // `-i … get` prints a one-element array.
    let version: u64 = serde_json::from_str::<serde_json::Value>(&rec.stdout).unwrap()[0]
        ["version"]
        .as_u64()
        .unwrap();

    mf(&["-u", &repo, "trash", "-f", file.to_str().unwrap()]);
    let entry = &repo_trash(&root).entries().unwrap()[0];
    assert_eq!(entry.version, Some(version), "the entry records the metarecord version");
}

// End to end: in a watched repo, `mf trash -f` deletes the metarecord in a
// write stamped `trash`; undoing it brings the file back from the trash
// with it (doc "Trash, undo and redo").
#[test]
fn test_undo_auto_restores_from_trash() {
    let (repo, root) = init_repo("rbrestore");
    // Enable watching on the filesystem root.
    let root_uuid = mf(&["-u", &repo, "metarecord", "get"]).stdout.trim().to_string();
    assert_ok(&mf(&[
        "-u",
        &repo,
        "metarecord",
        "-i",
        &root_uuid,
        "field",
        "set",
        "mf_watch:bool=true",
    ]));

    // Create a file and wait for the watcher to track it.
    let file = root.join("doc.txt");
    std::fs::write(&file, b"precious").unwrap();
    let uuid = poll_mf(
        &["-u", &repo, "metarecord", "-q", "mfr_path:value = \"doc.txt\"", "get"],
        is_hex_uuid,
    );

    // Trash it; wait for the watcher to record the deletion (mfr_path → Nothing).
    assert_ok(&mf(&["-u", &repo, "trash", "-f", file.to_str().unwrap()]));
    poll_mf(&["-u", &repo, "metarecord", "-q", "mfr_path:value = \"doc.txt\"", "get"], |s| {
        s.is_empty()
    });
    assert!(!file.exists(), "the file is in the trash");

    // Undo the deletion → the file is auto-restored from the trash.
    let out = take_back(&repo, "undo", &["--force"]);
    assert_ok(&out);
    assert_eq!(std::fs::read(&file).unwrap(), b"precious", "the file is back");
    // The metadata is restored too: the metarecord is at doc.txt again.
    let back =
        poll_mf(&["-u", &repo, "metarecord", "-q", "mfr_path:value = \"doc.txt\"", "get"], |s| {
            s == uuid
        });
    assert_eq!(back, uuid);
    // The trash entry was consumed.
    assert!(repo_trash(&root).entries().unwrap().is_empty(), "the entry is consumed");
}

// Undoing the trashing of a *directory* must restore the whole subtree's
// metarecords, not just the top one. The bytes come back via the trash, and
// the metarecords are recreated by the revert under their own uuids; a
// descendant left orphaned would be re-tracked as a duplicate.
#[test]
fn test_undo_restores_a_trashed_directory_subtree() {
    let (repo, root) = init_repo("rbdir");
    let root_uuid = mf(&["-u", &repo, "metarecord", "get"]).stdout.trim().to_string();
    assert_ok(&mf(&[
        "-u",
        &repo,
        "metarecord",
        "-i",
        &root_uuid,
        "field",
        "set",
        "mf_watch:bool=true",
    ]));

    // A directory with a nested file; wait for both to be tracked.
    let dir = root.join("A");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("B.txt"), b"bee").unwrap();
    let b_uuid = poll_mf(
        &["-u", &repo, "metarecord", "-q", "mfr_path:value = \"B.txt\"", "get"],
        is_hex_uuid,
    );
    let a_uuid =
        poll_mf(&["-u", &repo, "metarecord", "-q", "mfr_path:value = \"A\"", "get"], is_hex_uuid);

    // Trash the directory; wait for the watcher to cascade the deletion.
    assert_ok(&mf(&["-u", &repo, "trash", "-f", dir.to_str().unwrap()]));
    poll_mf(&["-u", &repo, "metarecord", "-q", "mfr_path:value = \"B.txt\"", "get"], |s| {
        s.is_empty()
    });
    assert!(!dir.exists(), "the directory is in the trash");

    // Undo the deletion.
    assert_ok(&take_back(&repo, "undo", &["--force"]));
    assert_eq!(std::fs::read(dir.join("B.txt")).unwrap(), b"bee", "the nested file is back");

    // The top directory *and* the nested file are restored to the SAME original
    // metarecords — the descendant is not left orphaned/duplicated.
    let a_back = mf(&["-u", &repo, "path", &a_uuid]);
    assert_ok(&a_back);
    assert!(a_back.stdout.trim().ends_with("/A"), "A restored: {}", a_back.stdout);
    let b_back = mf(&["-u", &repo, "path", &b_uuid]);
    assert_ok(&b_back);
    assert!(
        b_back.stdout.contains("A/B.txt"),
        "the descendant metarecord is restored: {}",
        b_back.stdout
    );
}

// ── Undo and redo of a trashing (doc "Trash, undo and redo") ───────

/// A repository whose root is watched.
fn watched_repo(prefix: &str) -> (String, TempDir) {
    let (repo, root) = init_repo(prefix);
    let root_uuid = mf(&["-u", &repo, "metarecord", "-q", "mfr_type = \"dir\"", "get"])
        .stdout
        .trim()
        .to_string();
    assert_ok(&mf(&[
        "-u",
        &repo,
        "metarecord",
        "-i",
        &root_uuid,
        "field",
        "set",
        "mf_watch:bool=true",
    ]));
    (repo, root)
}

/// Redoing a trashing trashes the file again — into a *new* entry, since the
/// undo consumed the first — rather than deleting its metarecord and leaving
/// the file on disk, untracked (doc "Trash, undo and redo"). And the pair composes.
#[test]
fn test_redo_of_a_trashing_trashes_the_file_again() {
    let (repo, root) = watched_repo("redo_trash_file");
    let file = root.join("doc.txt");
    std::fs::write(&file, b"precious").unwrap();
    assert!(poll(40, || uuids_at(&repo, "doc.txt").len() == 1), "watcher should track it");
    let uuid = uuids_at(&repo, "doc.txt")[0].clone();
    assert_ok(&mf(&["-u", &repo, "trash", "-f", file.to_str().unwrap()]));

    assert_ok(&take_back(&repo, "undo", &[]));
    assert_eq!(std::fs::read(&file).unwrap(), b"precious", "undo brings the file back");
    assert_eq!(uuids_at(&repo, "doc.txt"), vec![uuid.clone()]);

    let redo = take_back(&repo, "redo", &[]);
    assert_ok(&redo);
    assert!(!file.exists(), "redo sends it back to the trash: {}{}", redo.stdout, redo.stderr);
    let entries = repo_trash(&root).entries().unwrap();
    assert_eq!(entries.len(), 1, "one entry, a new one");
    assert_eq!(entries[0].metarecord.as_deref(), Some(uuid.as_str()));
    assert!(uuids_at(&repo, "doc.txt").is_empty(), "and its metarecord went with it");

    assert_ok(&take_back(&repo, "undo", &[]));
    assert_eq!(std::fs::read(&file).unwrap(), b"precious", "undo again");
    assert_eq!(uuids_at(&repo, "doc.txt"), vec![uuid]);
    assert!(repo_trash(&root).entries().unwrap().is_empty());
}

/// A directory goes back whole: one entry, its files inside, their
/// metarecords captured with it.
#[test]
fn test_redo_of_a_directory_trashing_trashes_the_whole_directory() {
    let (repo, root) = watched_repo("redo_trash_dir");
    let dir = root.join("A");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("B.txt"), b"bee").unwrap();
    assert!(poll(40, || uuids_at(&repo, "B.txt").len() == 1), "watcher should track it");
    let b = uuids_at(&repo, "B.txt")[0].clone();
    assert_ok(&mf(&["-u", &repo, "trash", "-f", dir.to_str().unwrap()]));

    assert_ok(&take_back(&repo, "undo", &[]));
    assert_eq!(std::fs::read(dir.join("B.txt")).unwrap(), b"bee");

    assert_ok(&take_back(&repo, "redo", &[]));
    assert!(!dir.exists(), "the directory is back in the trash");
    let entries = repo_trash(&root).entries().unwrap();
    assert_eq!(entries.len(), 1, "one entry for the directory and its file");
    assert!(entries[0].is_dir);
    assert!(entries[0].subtree.iter().any(|n| n.uuid == b), "the file's metarecord is captured");

    assert_ok(&take_back(&repo, "undo", &[]));
    assert_eq!(std::fs::read(dir.join("B.txt")).unwrap(), b"bee", "undo again");
    assert_eq!(uuids_at(&repo, "B.txt"), vec![b]);
}

/// When the watcher has written in between, undo is a revert and redo the
/// revert of that revert: neither carries the trash origin, and the bytes must
/// still follow the metarecord both ways.
#[test]
fn test_undo_and_redo_of_a_trashing_across_watcher_writes() {
    let (repo, root) = watched_repo("redo_trash_revert");
    let file = root.join("doc.txt");
    std::fs::write(&file, b"precious").unwrap();
    assert!(poll(40, || uuids_at(&repo, "doc.txt").len() == 1), "watcher should track it");
    let uuid = uuids_at(&repo, "doc.txt")[0].clone();
    assert_ok(&mf(&["-u", &repo, "trash", "-f", file.to_str().unwrap()]));

    std::fs::write(root.join("between.txt"), b"x").unwrap();
    assert!(poll(40, || uuids_at(&repo, "between.txt").len() == 1));
    // `--force`: a revert that touches files asks first, and nobody is here.
    let undo = take_back(&repo, "undo", &["--force"]);
    assert_ok(&undo);
    assert!(undo.stdout.contains("Reverted"), "the undo reverts: {}", undo.stdout);
    assert!(file.exists(), "the revert brings the file back: {}{}", undo.stdout, undo.stderr);
    assert_eq!(std::fs::read(&file).unwrap(), b"precious");
    assert_eq!(uuids_at(&repo, "doc.txt"), vec![uuid.clone()]);

    std::fs::write(root.join("again.txt"), b"y").unwrap();
    assert!(poll(40, || uuids_at(&repo, "again.txt").len() == 1));
    let redo = take_back(&repo, "redo", &["--force"]);
    assert_ok(&redo);
    assert!(!file.exists(), "the redo trashes it again: {}{}", redo.stdout, redo.stderr);
    assert!(uuids_at(&repo, "doc.txt").is_empty());
    assert_eq!(repo_trash(&root).entries().unwrap().len(), 1);
}

/// A file whose name is not UTF-8 goes to the trash with its folder and comes
/// back at its exact bytes, its metarecord with it — the capture used to drop
/// a node whose name is not text, leaving its metarecord behind (doc "Tree
/// names").
#[cfg(unix)]
#[test]
fn test_trash_takes_and_restores_a_name_that_is_not_utf8() {
    use std::os::unix::ffi::OsStrExt;
    let (repo, root) = super::sync::tracked_repo("trash_bytes", &[]);
    let dir = root.join("folder");
    std::fs::create_dir(&dir).unwrap();
    let file = dir.join(std::ffi::OsStr::from_bytes(b"caf\xe9.txt"));
    std::fs::write(&file, b"raw").unwrap();
    assert_ok(&mf(&["-u", &repo, "reconcile"]));
    let out = mf(&["-u", &repo, "metarecord", "-q", r#"mfr_path = "/folder/caf%E9.txt""#, "get"]);
    assert_ok(&out);
    let uuid = out.stdout.trim().to_string();
    assert!(is_hex_uuid(&uuid), "{}", out.stdout);

    assert_ok(&mf(&["-u", &repo, "trash", "-f", dir.to_str().unwrap()]));
    assert!(!dir.exists());
    let out = mf(&["-u", &repo, "metarecord", "-i", &uuid, "get"]);
    assert_ne!(out.code, 0, "the file's metarecord went with it: {}", out.stdout);

    let entry = repo_trash(&root).entries().unwrap().remove(0);
    assert_ok(&mf(&["-u", &repo, "trash", "restore", &entry.id]));
    assert_eq!(std::fs::read(&file).unwrap(), b"raw");
    let out = mf(&["-u", &repo, "path", "--relative", &uuid]);
    assert_ok(&out);
    assert_eq!(out.stdout_bytes, b"/folder/caf\xe9.txt\n");
}
