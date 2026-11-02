use super::*;

// ── Event log: mf log / mf log show / mf prune (doc "Event log") ─────────────────

#[test]
fn test_log_lists_labels_most_recent_first() {
    let (repo, _root) = init_repo("log_list");
    let uuid = create_metarecord(&repo, &["rating:int=3"]);
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "set", "rating:int=5"]));

    let out = mf(&["-u", &repo, "log", "list"]);
    assert_ok(&out);
    // HEAD is marked and is the first (most recent) line.
    assert!(out.stdout.contains("\u{2190} HEAD"), "stdout: {}", out.stdout);
    let first = out.stdout.lines().next().unwrap();
    assert!(first.starts_with('>') && first.contains("\u{2190} HEAD"), "first line: {first}");
    assert!(out.stdout.contains("label "), "stdout: {}", out.stdout);
}

#[test]
fn test_log_ops_expands_operations() {
    let (repo, _root) = init_repo("log_ops");
    let uuid = create_metarecord(&repo, &["rating:int=3"]);
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "set", "rating:int=5"]));

    let out = mf(&["-u", &repo, "log", "list", "--ops"]);
    assert_ok(&out);
    assert!(out.stdout.contains("set_field(rating)"), "stdout: {}", out.stdout);
    assert!(out.stdout.contains("op "), "stdout: {}", out.stdout);
}

#[test]
fn test_log_show_displays_before_and_after() {
    let (repo, _root) = init_repo("log_show");
    let uuid = create_metarecord(&repo, &["rating:int=3"]);
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "set", "rating:int=5"]));

    let out = mf(&["-u", &repo, "log", "show", "HEAD"]);
    assert_ok(&out);
    assert!(out.stdout.starts_with("Label "), "stdout: {}", out.stdout);
    assert!(out.stdout.contains("set_field(rating)"), "stdout: {}", out.stdout);
    assert!(out.stdout.contains("before:  3"), "stdout: {}", out.stdout);
    assert!(out.stdout.contains("after:   5"), "stdout: {}", out.stdout);

    // --raw prints JSON with the label object.
    let raw = mf(&["-u", &repo, "log", "show", "HEAD", "--raw"]);
    assert_ok(&raw);
    let parsed: serde_json::Value = serde_json::from_str(&raw.stdout).unwrap();
    assert!(parsed["label"]["is_head"].as_bool().unwrap());
}

#[test]
fn test_log_show_rejects_bad_target() {
    let (repo, _root) = init_repo("log_show_bad");
    let out = mf(&["-u", &repo, "log", "show", "notanumber"]);
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
}

#[test]
fn test_prune_before_makes_target_the_root() {
    let (repo, _root) = init_repo("prune_before");
    let uuid = create_metarecord(&repo, &["rating:int=3"]);
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "set", "rating:int=5"]));
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "set", "rating:int=7"]));

    // Prune before HEAD: every older operation is removed.
    let out =
        mf(&["-u", &repo, "log", "prune", "before", "--timestamp", "@9999999999999", "--force"]);
    assert_ok(&out);
    assert!(out.stdout.starts_with("Pruned "), "stdout: {}", out.stdout);
    // History still readable afterwards.
    assert_ok(&mf(&["-u", &repo, "log", "list"]));
}

#[test]
fn test_prune_requires_a_target() {
    let (repo, _root) = init_repo("prune_notarget");
    let out = mf(&["-u", &repo, "log", "prune", "before"]);
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
}

#[test]
fn test_revert_bad_move_policy_is_usage_error() {
    let (repo, _root) = init_repo("revert_policy");
    let uuid = create_metarecord(&repo, &["rating:int=3"]);
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "set", "rating:int=5"]));
    let out = mf(&["-u", &repo, "log", "revert", "--on-move-available", "bogus"]);
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
}

#[test]
fn test_prune_without_force_aborts_on_no() {
    let (repo, _root) = init_repo("prune_confirm");
    let uuid = create_metarecord(&repo, &["rating:int=3"]);
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "set", "rating:int=5"]));
    let out = mf_full(
        &["-u", &repo, "log", "prune", "before", "--timestamp", "@9999999999999"],
        Some("n\n"),
        &[],
        true,
    );
    assert_eq!(out.code, 1, "stderr: {}", out.stderr);
    assert!(out.stderr.contains("aborted"), "stderr: {}", out.stderr);
}

// ── `mf log revert` (doc "Reverting a label") ──────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn test_log_revert_undoes_a_revision_and_reports_the_blocker() {
    let (repo, _root) = init_repo("revert");
    let uuid = create_metarecord(&repo, &["rating:int=3"]);

    let out = mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "set", "rating:int=5"]);
    assert_ok(&out);
    // The write that write produced is the one HEAD sits in.
    let out = mf(&["-u", &repo, "log", "revert", "--force"]);
    assert_ok(&out);
    assert!(out.stdout.contains("Reverted"), "unexpected output: {}", out.stdout);

    assert_eq!(rating_of(&repo, &uuid), 3, "the revert restored the previous value");

    // A revert is itself an ordinary write, so the log kept both.
    let out = mf(&["-u", &repo, "log"]);
    assert_ok(&out);
    assert!(out.stdout.lines().count() >= 3, "the log keeps the mistake and its correction");
}

/// A later write on the same cell does not block a revert (doc "Reverting
/// onto the current state"): the plan names it, the revert asks before
/// overwriting it, and goes back to what the operation replaced.
#[tokio::test(flavor = "current_thread")]
async fn test_log_revert_warns_about_a_cell_changed_since() {
    let (repo, _root) = init_repo("revert_changed");
    let uuid = create_metarecord(&repo, &["rating:int=3"]);
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "set", "rating:int=5"]));
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "set", "rating:int=7"]));
    let rev = oldest_revision_with_rating(&repo);

    let out = mf(&["-u", &repo, "log", "revert", "plan", &rev]);
    assert_ok(&out);
    assert!(out.stdout.contains("changed since by op"), "the plan warns: {}", out.stdout);

    // Without --force it asks; an empty answer is a no.
    let out = mf(&["-u", &repo, "log", "revert", &rev]);
    assert_ok(&out);
    assert!(out.stderr.contains("changed since by op"), "it names the write: {}", out.stderr);
    assert!(out.stdout.contains("Aborted"), "{}", out.stdout);
    assert_eq!(rating_of(&repo, &uuid), 7);

    let out = mf(&["-u", &repo, "log", "revert", &rev, "--force"]);
    assert_ok(&out);
    assert_eq!(rating_of(&repo, &uuid), 3, "back to what the operation replaced");
}

/// The record's single `rating` row, read back through `metarecord get`.
fn rating_of(repo: &str, uuid: &str) -> i64 {
    let out = mf(&["-u", repo, "metarecord", "-i", uuid, "get"]);
    assert_ok(&out);
    let body: serde_json::Value = serde_json::from_str(&out.stdout).expect("get returns JSON");
    body[0]["fields"]
        .as_array()
        .and_then(|fields| fields.iter().find(|f| f["name"] == "rating"))
        .and_then(|f| f["value"]["value"].as_i64())
        .unwrap_or_else(|| panic!("no rating in:\n{}", out.stdout))
}

/// The write that set `rating` to 5: writes are listed newest first, so
/// it is the one below the write that now blocks reverting it.
fn oldest_revision_with_rating(repo: &str) -> String {
    let out = mf(&["-u", repo, "log", "list"]);
    assert_ok(&out);
    let revs: Vec<String> = out
        .stdout
        .lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            while let Some(token) = it.next() {
                if token == "label" {
                    return it.next().map(str::to_string);
                }
            }
            None
        })
        .collect();
    revs.get(1).cloned().unwrap_or_else(|| panic!("no second revision in:\n{}", out.stdout))
}

#[tokio::test(flavor = "current_thread")]
async fn test_log_revert_moves_the_file_back() {
    let (repo, root) = init_repo("revert_move");
    // Track the root so the watcher records the rename.
    let root_uuid = {
        let out = mf(&["-u", &repo, "metarecord", "-q", "mf_watch IS PRESENT", "get"]);
        assert_ok(&out);
        out.stdout.trim().lines().next().unwrap().to_string()
    };
    assert_ok(&mf(&[
        "-u",
        &repo,
        "metarecord",
        "-i",
        &root_uuid,
        "field",
        "set",
        r"mf_ignore:string=\.metafolder(/.*)?$",
    ]));
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

    std::fs::write(root.join("old.txt"), b"content").unwrap();
    assert_ok(&mf(&["-u", &repo, "reconcile"]));
    std::fs::rename(root.join("old.txt"), root.join("new.txt")).unwrap();

    // Wait for the watcher's flush to record the rename.
    let mut recorded = false;
    for _ in 0..100 {
        let out = mf(&["-u", &repo, "log", "list", "--ops"]);
        assert_ok(&out);
        if out.stdout.contains("file_moved") {
            recorded = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(recorded, "the watcher never recorded the rename");
    assert!(root.join("new.txt").exists());

    // Reverting it moves the file back and records the move.
    let out = mf(&["-u", &repo, "log", "revert", "--force"]);
    assert_ok(&out);
    assert!(root.join("old.txt").exists(), "the file moved back: {}{}", out.stdout, out.stderr);
    assert!(!root.join("new.txt").exists());
}

#[test]
fn metarecord_get_without_a_selector_honours_sort_and_select() {
    // `mf metarecord get` with no selector means "every metarecord", and it used
    // to take a separate, reduced path that accepted `--sort`, `--select` and
    // `--values` on the command line and then silently dropped them. A script
    // walking the whole repository "in path order" was in fact walking it in
    // whatever order the daemon returned, so two runs disagreed — and a
    // `--select` printed uuids instead of the field asked for.
    let (repo, _root) = init_repo("nosel");
    let mut made = Vec::new();
    for name in ["c", "a", "b"] {
        made.push(create_metarecord(&repo, &[&format!("label:string={name}")]));
    }

    let out =
        mf(&["-u", &repo, "metarecord", "get", "--select", "label", "--values", "--sort", "label"]);
    assert_ok(&out);
    assert_eq!(out.stdout.lines().collect::<Vec<_>>(), vec!["a", "b", "c"]);

    let desc = mf(&[
        "-u",
        &repo,
        "metarecord",
        "get",
        "--select",
        "label",
        "--values",
        "--sort",
        "label:desc",
    ]);
    assert_ok(&desc);
    assert_eq!(desc.stdout.lines().collect::<Vec<_>>(), vec!["c", "b", "a"]);

    // The plain form still lists uuids, one per line — every metarecord in the
    // repository, which includes the filesystem root `init` created.
    let plain = mf(&["-u", &repo, "metarecord", "get"]);
    assert_ok(&plain);
    let listed: Vec<&str> = plain.stdout.lines().collect();
    for uuid in &made {
        assert!(listed.contains(&uuid.as_str()), "{uuid} missing from: {listed:?}");
    }
    assert_eq!(listed.len(), made.len() + 1, "the root metarecord should be listed too");
}

#[test]
fn log_head_prints_the_head_operation_id() {
    // The primitive a script needs to go back: the newest operation right now,
    // in the form `mf log undo --after` takes.
    let (repo, _root) = init_repo("loghead");

    let before = mf(&["-u", &repo, "log", "head"]);
    assert_ok(&before);
    let before_id: i64 = before.stdout.trim().parse().expect("an operation id");

    create_metarecord(&repo, &["label:string=x"]);

    let after = mf(&["-u", &repo, "log", "head"]);
    assert_ok(&after);
    let after_id: i64 = after.stdout.trim().parse().expect("an operation id");
    assert!(after_id > before_id, "a write moves HEAD forward: {before_id} -> {after_id}");
}

// ── `mf log undo` (doc "Undo and redo") ──────────────────────────────

/// Undo reverts the user's last revision: the log keeps it, and the correction.
#[test]
fn test_log_undo_reverts_the_last_manual_revision() {
    let (repo, _root) = init_repo("log_undo_last");
    let uuid = create_metarecord(&repo, &["rating:int=3"]);
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "set", "rating:int=5"]));

    // Without a label, undo names the labels it could take back: the
    // command that wrote the change, innermost first.
    let plan = mf(&["-u", &repo, "log", "undo"]);
    assert_ok(&plan);
    assert!(plan.stdout.contains("field set rating:int=5"), "stdout: {}", plan.stdout);
    let label = first_candidate(&plan.stdout).expect("a candidate");
    let plan = mf(&["-u", &repo, "log", "undo", "plan", &label]);
    assert_ok(&plan);
    assert!(plan.stdout.contains("set_field(rating)"), "stdout: {}", plan.stdout);
    // A plan writes nothing.
    let out = mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "get", "rating"]);
    assert!(out.stdout.contains('5'), "plan should not have written: {}", out.stdout);

    let undo = take_back(&repo, "undo", &[]);
    assert_ok(&undo);
    let out = mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "get", "rating"]);
    assert!(out.stdout.contains('3'), "stdout: {}", out.stdout);
}

/// A second undo walks further back through the user's changes instead of
/// undoing the first one.
#[test]
fn test_log_undo_repeats_down_the_users_changes() {
    let (repo, _root) = init_repo("log_undo_repeat");
    let uuid = create_metarecord(&repo, &["rating:int=1"]);
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "set", "rating:int=2"]));
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "set", "tag:string=x"]));

    assert_ok(&take_back(&repo, "undo", &[])); // undoes the tag
    let out = mf(&["-u", &repo, "metarecord", "-i", &uuid, "get"]);
    assert!(!out.stdout.contains("\"tag\""), "the tag should be gone: {}", out.stdout);

    assert_ok(&take_back(&repo, "undo", &[])); // undoes rating=2
    let out = mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "get", "rating"]);
    assert!(out.stdout.contains('1'), "stdout: {}", out.stdout);
}

/// Undo runs out rather than eating the repository: it stops at the write
/// that founds the history (the root metarecord the daemon writes at init),
/// and says so instead of emptying the log.
#[test]
fn test_log_undo_stops_at_the_root_of_the_history() {
    let (repo, _root) = init_repo("log_undo_empty");
    create_metarecord(&repo, &["rating:int=3"]);

    let mut said_nothing = false;
    for _ in 0..6 {
        let out = take_back(&repo, "undo", &[]);
        assert_ok(&out);
        if out.stdout.to_lowercase().contains("nothing to undo") {
            said_nothing = true;
            break;
        }
    }
    assert!(said_nothing, "undo never ran out");
    // The root metarecord is still there: the foundation was never undone.
    let out = mf(&["-u", &repo, "metarecord", "get"]);
    assert_ok(&out);
    assert!(!out.stdout.trim().is_empty(), "the repository lost its root metarecord");
}

/// The case undo exists for: the user changes a field, then the watcher records
/// something of its own. Undo reverts the user's change and leaves the
/// watcher's work standing.
#[test]
fn test_log_undo_reverts_when_the_watcher_has_written_since() {
    let (repo, root) = init_repo("log_undo_watcher");
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

    // The user's change, then a file arriving under the watched root.
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &root_uuid, "field", "set", "note:string=a"]));
    std::fs::write(root.join("later.txt"), b"hi").unwrap();
    assert!(poll(40, || uuids_at(&repo, "later.txt").len() == 1), "watcher should track the file");

    let out = take_back(&repo, "undo", &[]);
    assert_ok(&out);
    assert!(out.stdout.contains("Reverted"), "undo reverts: {}", out.stdout);

    // The user's field is gone, and the watcher's metarecord is still there.
    let record = mf(&["-u", &repo, "metarecord", "-i", &root_uuid, "get"]);
    assert!(!record.stdout.contains("\"note\""), "the note should be undone: {}", record.stdout);
    assert_eq!(uuids_at(&repo, "later.txt").len(), 1, "the tracked file must survive the undo");
}

/// Redo takes an undo back, by reverting it.
#[test]
fn test_log_redo_reapplies_what_undo_took_back() {
    let (repo, _root) = init_repo("log_redo_forward");
    let uuid = create_metarecord(&repo, &["rating:int=3"]);
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "set", "rating:int=5"]));
    assert_ok(&take_back(&repo, "undo", &[]));
    let out = mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "get", "rating"]);
    assert!(out.stdout.contains('3'), "undo should have taken it back: {}", out.stdout);

    let plan = mf(&["-u", &repo, "log", "redo"]);
    assert_ok(&plan);
    assert!(plan.stdout.contains("log undo"), "the undo is offered: {}", plan.stdout);

    assert_ok(&take_back(&repo, "redo", &[]));
    let out = mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "get", "rating"]);
    assert!(out.stdout.contains('5'), "redo should have put it back: {}", out.stdout);
}

/// The case the history tree lost (doc "Why the log stops being a tree"): the
/// watcher writes between the undo and the redo, and redo still works.
#[test]
fn test_log_redo_survives_the_watcher_writing_after_the_undo() {
    let (repo, root) = init_repo("log_redo_revert");
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

    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &root_uuid, "field", "set", "note:string=a"]));
    let undo = take_back(&repo, "undo", &[]);
    assert_ok(&undo);
    let record = mf(&["-u", &repo, "metarecord", "-i", &root_uuid, "get"]);
    assert!(!record.stdout.contains("\"note\""), "the note should be gone: {}", record.stdout);

    // The watcher writes between the undo and the redo.
    std::fs::write(root.join("between.txt"), b"hi").unwrap();
    assert!(poll(40, || uuids_at(&repo, "between.txt").len() == 1), "watcher should track it");

    // Redo puts the change back, and the watcher's metarecord survives it.
    let redo = take_back(&repo, "redo", &[]);
    assert_ok(&redo);
    let record = mf(&["-u", &repo, "metarecord", "-i", &root_uuid, "get"]);
    assert!(record.stdout.contains("\"note\""), "the note should be back: {}", record.stdout);
    assert_eq!(uuids_at(&repo, "between.txt").len(), 1, "the tracked file must survive the redo");

    // And undo takes it away again: the pair composes.
    assert_ok(&take_back(&repo, "undo", &[]));
    let record = mf(&["-u", &repo, "metarecord", "-i", &root_uuid, "get"]);
    assert!(!record.stdout.contains("\"note\""), "undo again: {}", record.stdout);
}

/// Nothing to take back: redo says so and writes nothing.
#[test]
fn test_log_redo_with_nothing_to_redo() {
    let (repo, _root) = init_repo("log_redo_empty");
    create_metarecord(&repo, &["rating:int=3"]);
    let out = take_back(&repo, "redo", &[]);
    assert_ok(&out);
    assert!(out.stdout.to_lowercase().contains("nothing to redo"), "stdout: {}", out.stdout);
}

/// `mf repo check` reports a healthy repository (exit 0) and `mf repo
/// reindex` derives again, after which it still answers.
#[test]
fn test_repo_check_and_reindex() {
    let (repo, _root) = init_repo("check");
    let kept = create_metarecord(&repo, &["note:string=kept"]);
    let out = mf(&["-u", &repo, "repo", "check"]);
    assert_ok(&out);
    assert!(out.stdout.contains("ok"), "{}", out.stdout);
    let out = mf(&["-u", &repo, "repo", "reindex"]);
    assert_ok(&out);
    let out = mf(&["-u", &repo, "metarecord", "-q", "note = \"kept\"", "get"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), kept);
}

/// `mf repo backup` takes a verified backup and prints where it went;
/// `--to` names a new directory.
#[test]
fn test_repo_backup() {
    let (repo, root) = init_repo("backup");
    create_metarecord(&repo, &["note:string=kept"]);
    let out = mf(&["-u", &repo, "repo", "backup"]);
    assert_ok(&out);
    let path = std::path::PathBuf::from(out.stdout.trim());
    assert!(path.join("backup.json").exists(), "{}", out.stdout);
    let to = root.join("saved");
    let out = mf(&["-u", &repo, "repo", "backup", "--to", to.to_str().unwrap()]);
    assert_ok(&out);
    assert!(to.join("config.json").exists());
}

/// `mf repo restore` puts the most recent backup back: what was written
/// since is gone, and the command says so and advises a reconcile. With
/// `--path`, it restores a repository by its root.
#[test]
fn test_repo_restore() {
    let (repo, root) = init_repo("restore");
    let kept = create_metarecord(&repo, &["note:string=kept"]);
    assert_ok(&mf(&["-u", &repo, "repo", "backup"]));
    create_metarecord(&repo, &["note:string=lost"]);

    let out = mf(&["-u", &repo, "repo", "restore"]);
    assert_ok(&out);
    assert!(out.stdout.contains("restored from"), "{}", out.stdout);
    assert!(out.stderr.contains("mf reconcile"), "{}", out.stderr);
    let out = mf(&["-u", &repo, "metarecord", "-q", "note IS PRESENT", "get"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), kept);

    let out = mf(&["repo", "restore", "--path", root.to_str().unwrap()]);
    assert_ok(&out);
    assert!(out.stdout.contains("restored from"), "{}", out.stdout);
}

// `mf watch activity` walks down from the root to where the watcher's events
// come from (doc "Watch activity").
#[test]
fn test_watch_activity_ranks_the_busy_subtree() {
    let (repo, root) = init_repo("watch_activity");
    std::fs::create_dir_all(root.join("busy/deep")).unwrap();
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
    assert_ok(&mf(&["-u", &repo, "reconcile"]));
    for i in 0..4 {
        std::fs::write(root.join(format!("busy/deep/f{i}")), b"x").unwrap();
    }
    let out = poll_mf(&["-u", &repo, "watch", "activity"], |s| {
        s.contains("/busy\n") || s.ends_with("/busy")
    });
    assert!(out.starts_with("/: "), "{out}");
    let out = mf(&["-u", &repo, "watch", "activity", "/busy"]);
    assert_ok(&out);
    assert!(out.stdout.starts_with("/busy: "), "{}", out.stdout);
    assert!(out.stdout.contains("/busy/deep"), "{}", out.stdout);

    // Late events may land right after a reset, so it is checked by its clock.
    let since = |out: &Out| -> i64 {
        serde_json::from_str::<serde_json::Value>(&out.stdout).unwrap()["since_ms"]
            .as_i64()
            .unwrap()
    };
    let before = since(&mf(&["-u", &repo, "watch", "activity", "--json"]));
    std::thread::sleep(std::time::Duration::from_millis(5));
    let reset = mf(&["-u", &repo, "watch", "activity", "--reset", "--json"]);
    assert_ok(&reset);
    assert!(since(&reset) > before, "{}", reset.stdout);
    let reset = mf(&["-u", &repo, "watch", "activity", "--reset"]);
    assert_ok(&reset);
    assert_eq!(reset.stdout.trim(), "watch activity reset");
}

// Both selectors from the environment are refused like both flags: the CLI
// will not pick one of two repositories for the user.
#[test]
fn test_both_repo_selectors_from_the_environment_are_refused() {
    let (repo, _root) = init_repo("both-selectors");
    let out = mf_full(
        &["metarecord", "get"],
        None,
        &[("METAFOLDER_REPO", &repo), ("METAFOLDER_REPO_NAME", "both-selectors")],
        true,
    );
    assert_eq!(out.code, 2, "stdout: {} stderr: {}", out.stdout, out.stderr);
    assert!(out.stderr.contains("not both"), "{}", out.stderr);
}

// ── Contexts (doc "Labels and contexts") ─────────────────────────────────────

/// `mf log show HEAD --raw`: the newest operation's label.
fn head_label(repo: &str, envs: &[(&str, &str)]) -> serde_json::Value {
    let out = mf_full(&["-u", repo, "log", "show", "HEAD", "--raw"], None, envs, true);
    assert_ok(&out);
    serde_json::from_str::<serde_json::Value>(&out.stdout).unwrap()["label"].clone()
}

/// A command writes in a label of its own, named after its command line,
/// inside the context `mf label begin` opened and `METAFOLDER_LABEL` passes on.
#[test]
fn test_a_command_writes_in_its_own_label_inside_the_context() {
    let (repo, _root) = init_repo("label_context");
    let out = mf(&["-u", &repo, "label", "begin", "tag run", "all the photos"]);
    assert_ok(&out);
    let context = out.stdout.trim().to_string();
    let (_, run) = context.split_once(':').expect("<repo>:<id>");
    let envs = [("METAFOLDER_LABEL", context.as_str())];

    let out = mf_full(&["-u", &repo, "metarecord", "add", "rating:int=3"], None, &envs, true);
    assert_ok(&out);
    let label = head_label(&repo, &[]);
    assert_eq!(label["parent"].as_i64().unwrap().to_string(), run, "{label}");
    assert!(label["name"].as_str().unwrap().contains("metarecord add rating:int=3"), "{label}");
    assert_eq!(label["ancestors"][0]["description"], "all the photos");

    // `mf label show` names the context it runs in.
    let out = mf_full(&["-u", &repo, "label", "show"], None, &envs, true);
    assert_ok(&out);
    assert!(out.stdout.contains("tag run"), "{}", out.stdout);

    // A nested context is a child of the one it was begun in.
    let out = mf_full(&["-u", &repo, "label", "begin", "answer 1"], None, &envs, true);
    assert_ok(&out);
    let answer = out.stdout.trim().to_string();
    let out = mf_full(
        &["-u", &repo, "metarecord", "add", "rating:int=4"],
        None,
        &[("METAFOLDER_LABEL", answer.as_str())],
        true,
    );
    assert_ok(&out);
    let label = head_label(&repo, &[]);
    let answer_id = answer.split_once(':').unwrap().1;
    assert_eq!(label["parent"].as_i64().unwrap().to_string(), answer_id, "{label}");
    assert_eq!(label["ancestors"][1]["id"].as_i64().unwrap().to_string(), run, "{label}");
}

/// A context names its repository: in another one, it is not sent.
#[test]
fn test_a_context_is_not_sent_to_another_repository() {
    let (repo, _root) = init_repo("label_scope");
    let (other, _other_root) = init_repo("label_scope_other");
    let out = mf(&["-u", &repo, "label", "begin", "run"]);
    assert_ok(&out);
    let context = out.stdout.trim().to_string();
    let out = mf_full(
        &["-u", &other, "metarecord", "add", "rating:int=3"],
        None,
        &[("METAFOLDER_LABEL", context.as_str())],
        true,
    );
    assert_ok(&out);
    assert_eq!(head_label(&other, &[])["parent"], serde_json::Value::Null);

    let out =
        mf_full(&["-u", &repo, "metarecord", "get"], None, &[("METAFOLDER_LABEL", "12")], true);
    assert_eq!(out.code, 2, "a malformed context is a usage error: {}", out.stderr);
}

/// The innermost label `mf log undo`/`redo` offers in `listing`.
fn first_candidate(listing: &str) -> Option<String> {
    listing.lines().skip(1).find_map(|l| l.split_whitespace().next()).map(str::to_string)
}

/// `mf log undo` (or `redo`) of the innermost label it offers — what a
/// single-label change asks for. The listing when there is nothing to take
/// back.
pub(super) fn take_back(repo: &str, verb: &str, extra: &[&str]) -> Out {
    let listing = mf(&["-u", repo, "log", verb]);
    assert_ok(&listing);
    let Some(label) = first_candidate(&listing.stdout) else { return listing };
    let mut args = vec!["-u", repo, "log", verb, label.as_str()];
    args.extend_from_slice(extra);
    mf(&args)
}

/// Undo offers every label the newest change is in, innermost first: the
/// command, then the context it ran in. Undoing the context takes back every
/// command of it (doc "Undo and redo by label").
#[test]
fn test_undo_offers_the_context_and_takes_it_back_whole() {
    let (repo, _root) = init_repo("undo_context");
    let uuid = create_metarecord(&repo, &["rating:int=1"]);
    let out = mf(&["-u", &repo, "label", "begin", "two edits"]);
    assert_ok(&out);
    let context = out.stdout.trim().to_string();
    let envs = [("METAFOLDER_LABEL", context.as_str())];
    for spec in ["rating:int=2", "note:string=x"] {
        let out = mf_full(
            &["-u", &repo, "metarecord", "-i", &uuid, "field", "set", spec],
            None,
            &envs,
            true,
        );
        assert_ok(&out);
    }

    let listing = mf(&["-u", &repo, "log", "undo"]);
    assert_ok(&listing);
    let lines: Vec<&str> = listing.stdout.lines().skip(1).collect();
    assert_eq!(lines.len(), 2, "the command, then the context: {}", listing.stdout);
    assert!(lines[0].contains("note:string=x"), "{}", listing.stdout);
    assert!(lines[1].contains("two edits"), "{}", listing.stdout);

    let run = context.split_once(':').unwrap().1;
    assert_ok(&mf(&["-u", &repo, "log", "undo", run]));
    let out = mf(&["-u", &repo, "metarecord", "-i", &uuid, "get"]);
    assert!(!out.stdout.contains("\"note\""), "{}", out.stdout);
    let rating = mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "get", "rating"]);
    assert!(rating.stdout.contains('1'), "both commands taken back: {}", rating.stdout);

    // And redo puts the whole context back.
    let redo = take_back(&repo, "redo", &[]);
    assert_ok(&redo);
    let rating = mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "get", "rating"]);
    assert!(rating.stdout.contains('2'), "{}", rating.stdout);
}
