use super::*;

// ── Duplicate detection (doc "Finding duplicates") ──────────────────────────────

#[test]
fn test_duplicate_scan_then_list() {
    let (repo, _dir) = tracked_repo(
        "dupcli",
        &[
            ("a.txt", b"the very same bytes"),
            ("b.txt", b"the very same bytes"),
            ("c.txt", b"quite different!!!!"),
        ],
    );

    let scan = mf(&["-u", &repo, "duplicate", "scan"]);
    assert_ok(&scan);
    assert!(scan.stdout.contains("groups: 1"), "summary: {}", scan.stdout);
    assert!(scan.stdout.contains("files: 2"), "summary: {}", scan.stdout);

    // `list` is the group default, so bare `mf duplicate` lists.
    let list = mf(&["-u", &repo, "duplicate"]);
    assert_ok(&list);
    assert!(list.stdout.contains("RECLAIMABLE"), "header: {}", list.stdout);
    let rows: Vec<&str> = list.stdout.lines().skip(1).filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(rows.len(), 1, "one group listed: {}", list.stdout);

    // The verbose form names the members.
    let verbose = mf(&["-u", &repo, "duplicate", "list", "-v"]);
    assert_ok(&verbose);
    assert!(verbose.stdout.contains("/a.txt"), "members: {}", verbose.stdout);
    assert!(verbose.stdout.contains("/b.txt"), "members: {}", verbose.stdout);
    assert!(!verbose.stdout.contains("/c.txt"), "the unique file is not a member");
}

#[test]
fn test_duplicate_list_is_empty_before_a_scan() {
    let (repo, _dir) = tracked_repo("dupempty", &[("a.txt", b"same"), ("b.txt", b"same")]);
    let list = mf(&["-u", &repo, "duplicate"]);
    assert_ok(&list);
    assert!(list.stdout.contains("No duplicate"), "expected the empty line: {}", list.stdout);
}

#[test]
fn test_duplicate_scan_min_size_takes_a_human_size() {
    let (repo, _dir) = tracked_repo(
        "dupmin",
        &[("s1", b"tiny"), ("s2", b"tiny"), ("b1", &[7u8; 4096]), ("b2", &[7u8; 4096])],
    );
    let scan = mf(&["-u", &repo, "duplicate", "scan", "--min-size", "1k"]);
    assert_ok(&scan);
    assert!(scan.stdout.contains("groups: 1"), "only the big pair: {}", scan.stdout);
}

#[test]
fn test_duplicate_scan_rejects_a_bad_size() {
    let (repo, _dir) = tracked_repo("dupbadsize", &[]);
    let out = mf(&["-u", &repo, "duplicate", "scan", "--min-size", "banana"]);
    assert_eq!(out.code, 2, "a malformed size is a usage error: {}", out.stderr);
}

#[cfg(unix)]
#[test]
fn test_duplicate_list_marks_hard_links() {
    // Three names, two inodes: the listing marks the linked pair and the
    // reclaimable figure counts one copy, not two.
    let (repo, dir) = tracked_repo("duplink", &[("orig", b"linked content!!")]);
    std::fs::hard_link(dir.join("orig"), dir.join("link")).unwrap();
    std::fs::write(dir.join("copy"), b"linked content!!").unwrap();
    assert_ok(&mf(&["-u", &repo, "reconcile"]));
    assert_ok(&mf(&["-u", &repo, "duplicate", "scan"]));

    let list = mf(&["-u", &repo, "duplicate", "list", "-v"]);
    assert_ok(&list);
    let marked: Vec<&str> =
        list.stdout.lines().filter(|l| l.trim_start().starts_with('+')).collect();
    assert_eq!(marked.len(), 2, "both names of the linked inode are marked: {}", list.stdout);
    assert!(list.stdout.contains("16B"), "one copy's worth is reclaimable: {}", list.stdout);
}

#[test]
fn test_duplicate_clear_removes_the_groups() {
    let (repo, _dir) =
        tracked_repo("dupclear", &[("a.txt", b"same bytes"), ("b.txt", b"same bytes")]);
    assert_ok(&mf(&["-u", &repo, "duplicate", "scan"]));
    assert_eq!(
        mf(&["-u", &repo, "metarecord", "-q", "mfr_duplicate_group IS PRESENT", "get"])
            .stdout
            .split_whitespace()
            .count(),
        2
    );

    let cleared = mf(&["-u", &repo, "duplicate", "clear", "-y"]);
    assert_ok(&cleared);
    assert!(cleared.stdout.contains("cleared"), "summary: {}", cleared.stdout);

    let groups = mf(&["-u", &repo, "metarecord", "-q", "mf_schema = \"duplicate_group\"", "get"]);
    assert_ok(&groups);
    assert!(groups.stdout.trim().is_empty(), "no group left: {}", groups.stdout);
    let linked = mf(&["-u", &repo, "metarecord", "-q", "mfr_duplicate_group IS PRESENT", "get"]);
    assert_ok(&linked);
    assert!(linked.stdout.trim().is_empty(), "no link left: {}", linked.stdout);

    // The hash cache is deliberately untouched: it is valid, expensive, and
    // useful to reconcile and sync independently of duplicate detection.
    let hashed = mf(&["-u", &repo, "metarecord", "-q", "mfr_full_hash IS PRESENT", "get"]);
    assert_ok(&hashed);
    assert_eq!(hashed.stdout.split_whitespace().count(), 2, "hashes kept: {}", hashed.stdout);
}

#[test]
fn test_duplicate_same_query_reaches_the_twins() {
    // The point of the group model: from one file to its twins, without the
    // caller ever handling a hash.
    let (repo, _dir) = tracked_repo(
        "dupsame",
        &[("a.txt", b"twin content"), ("b.txt", b"twin content"), ("c.txt", b"other conten")],
    );
    assert_ok(&mf(&["-u", &repo, "duplicate", "scan"]));
    let a = query_one(&repo, "mfr_path:value = \"a.txt\"");

    let peers =
        mf(&["-u", &repo, "metarecord", "-q", &format!("same(mfr_duplicate_group, {a})"), "get"]);
    assert_ok(&peers);
    assert_eq!(peers.stdout.split_whitespace().count(), 2, "the class: {}", peers.stdout);

    let others = mf(&[
        "-u",
        &repo,
        "metarecord",
        "-q",
        &format!("same(mfr_duplicate_group, {a}) AND NOT {a}"),
        "get",
    ]);
    assert_ok(&others);
    assert_eq!(others.stdout.split_whitespace().count(), 1, "the others: {}", others.stdout);
    assert_ne!(others.stdout.trim(), a);
}

#[test]
fn test_sync_does_not_materialise_a_duplicate_group() {
    // `mfr_duplicate_group` is content-derived, so the metadata diff never
    // writes it and each repository computes its own groups. The referential
    // closure must agree: closing over it would plant a bare, empty group
    // record in B for no purpose (doc "Duplicates and sync").
    let (a, _adir) = tracked_repo("dupclosure_a", &[("doc.txt", b"x")]);
    let (b, _bdir) = tracked_repo("dupclosure_b", &[]);
    let xa = query_one(&a, "mfr_path:value = \"doc.txt\"");
    let group_a = create_metarecord(&a, &["mf_schema:string=duplicate_group"]);
    assert_ok(&mf(&[
        "-u",
        &a,
        "metarecord",
        "-i",
        &xa,
        "field",
        "add",
        &format!("mfr_duplicate_group:ref={group_a}"),
        "--force",
    ]));

    let intents = write_intents(
        "dupclosure",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    assert_ok(&mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]));
    assert_ok(&mf(&["sync", "run", &a, &b, "--yes"]));

    let xb = query_one(&b, "mfr_path:value = \"doc.txt\"");
    assert!(is_hex_uuid(&xb), "the file itself still syncs: {xb}");
    assert_eq!(
        field_value_of(&b, &xb, "mfr_duplicate_group"),
        None,
        "the group link must not be synced"
    );
    let groups = mf(&["-u", &b, "metarecord", "-q", "mf_schema = \"duplicate_group\"", "get"]);
    assert_ok(&groups);
    assert!(
        groups.stdout.trim().is_empty(),
        "no group record may be materialised in B: {}",
        groups.stdout
    );
}

#[test]
fn test_sync_run_moves_diverged_file() {
    // Two files linked across different paths (a.txt ↔ b.txt), never synced:
    // a conflict on the position, here resolved for repo `a`. The run moves
    // the other file and record to a.txt. Nothing is destroyed.
    let (a, _adir) = tracked_repo("mvrun_a", &[("a.txt", b"content")]);
    let (b, _bdir) = tracked_repo("mvrun_b", &[("b.txt", b"content")]);
    let xa = query_one(&a, "mfr_path:value = \"a.txt\"");
    let xb = query_one(&b, "mfr_path:value = \"b.txt\"");
    assert_ok(&mf(&["sync", "link", &a, &b, &xa, &xb]));

    let intents = write_intents(
        "mvrun",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    sync_pair(&a, &b, &intents, &["--on-conflict", &format!("prefer:{a}")]);

    assert_eq!(query_one(&a, "mfr_path:value = \"a.txt\""), xa, "A stays");
    assert_eq!(query_one(&b, "mfr_path:value = \"a.txt\""), xb, "B moved to A's path");
    assert!(repo_root_of(&a).join("a.txt").exists(), "A's file stays");
    assert!(repo_root_of(&b).join("a.txt").exists(), "B's file moved");
    assert!(!repo_root_of(&b).join("b.txt").exists(), "B's old name is gone");
    let status = mf(&["sync", "status", &a, &b]);
    assert!(status.stdout.contains("in_sync"), "{}", status.stdout);
}

/// A file renamed on one side *and* given a new field between two syncs: the
/// rename reaches the other side, and a later sync does not take it back.
///
/// It used to: the `sync` op wrote B's fields, which changed B's version, so the
/// `move` op read as stale and was skipped; the commit then recorded A's new
/// path as the snapshot's, and the next run took B's unmoved path for the
/// change — moving A's file back. A run now checks each op against the version
/// its own writes left, not the plan's.
#[test]
fn test_sync_rename_with_a_field_change_is_not_reverted() {
    let (a, _adir) = tracked_repo("mvfld_a", &[("old.txt", b"content")]);
    let (b, _bdir) = tracked_repo("mvfld_b", &[("old.txt", b"content")]);
    let intents = write_intents(
        "mvfld",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    let sync = || {
        assert_ok(&mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]));
        assert_ok(&mf(&["sync", "run", &a, &b, "--yes"]));
    };
    sync();

    // Rename on A (with the watcher off, so the test drives the record itself),
    // and tag it in the same breath.
    let root_a_uuid =
        mf(&["-u", &a, "metarecord", "-q", "mf_watch = true", "get"]).stdout.trim().to_string();
    assert_ok(&mf(&[
        "-u",
        &a,
        "metarecord",
        "-i",
        &root_a_uuid,
        "field",
        "set",
        "mf_watch:bool=false",
    ]));
    let xa = query_one(&a, "mfr_path:value = \"old.txt\"");
    let root_a = repo_root_of(&a);
    std::fs::rename(root_a.join("old.txt"), root_a.join("new.txt")).unwrap();
    assert_ok(&mf(&[
        "-u",
        &a,
        "metarecord",
        "-i",
        &xa,
        "field",
        "set",
        &format!("mfr_path:tree_ref={root_a_uuid}/new.txt"),
        "-f",
    ]));
    assert_ok(&mf(&["-u", &a, "metarecord", "-i", &xa, "field", "add", "tag:string=jazz"]));

    sync();
    sync();

    assert!(root_a.join("new.txt").exists(), "A keeps its rename");
    assert!(repo_root_of(&b).join("new.txt").exists(), "B follows the rename");
    assert!(!repo_root_of(&b).join("old.txt").exists(), "B's old name is gone");
}

/// Plans and runs a sync of the pair with the given intents, asserting success.
fn sync_pair(a: &str, b: &str, intents: &std::path::Path, extra: &[&str]) {
    let mut args = vec!["sync", "plan", a, b, "--intents", intents.to_str().unwrap()];
    args.extend_from_slice(extra);
    assert_ok(&mf(&args));
    assert_ok(&mf(&["sync", "run", a, b, "--yes"]));
}

/// The plan's ops of one kind, as metarecord JSON.
pub(super) fn plan_ops(a: &str, b: &str, kind: &str) -> Vec<serde_json::Value> {
    let plan = mf(&["repo", "list", "--all"]);
    let repos: serde_json::Value = serde_json::from_str(&plan.stdout).unwrap();
    let name = plan_name(a, b);
    let plan = repos
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == name.as_str())
        .and_then(|r| r["repo_uuid"].as_str())
        .expect("a plan repo")
        .to_string();
    let got = mf(&[
        "-u",
        &plan,
        "metarecord",
        "-q",
        &format!("plan_kind = \"{kind}\""),
        "get",
        "--select",
        "*",
    ]);
    assert_ok(&got);
    if got.stdout.trim().is_empty() {
        return Vec::new();
    }
    serde_json::from_str::<serde_json::Value>(&got.stdout).unwrap().as_array().unwrap().clone()
}

/// A conflict resolved `skip` stays a conflict: the run leaves both sides alone
/// *and* remembers that they disagree, so the next plan asks again rather than
/// reading the side that did not win as "the one that changed" and silently
/// overwriting the other with it.
#[test]
fn test_sync_skipped_conflict_conflicts_again() {
    let (a, _adir) = tracked_repo("skipc_a", &[("doc.txt", b"x")]);
    let (b, _bdir) = tracked_repo("skipc_b", &[("doc.txt", b"x")]);
    let intents = write_intents(
        "skipc",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    sync_pair(&a, &b, &intents, &[]);
    let xa = query_one(&a, "mfr_path:value = \"doc.txt\"");
    let xb = query_one(&b, "mfr_path:value = \"doc.txt\"");
    assert_ok(&mf(&["-u", &a, "metarecord", "-i", &xa, "field", "set", "tag:string=jazz"]));
    assert_ok(&mf(&["-u", &b, "metarecord", "-i", &xb, "field", "set", "tag:string=rock"]));

    sync_pair(&a, &b, &intents, &["--on-conflict", "skip"]);
    assert_eq!(field_value_of(&a, &xa, "tag").as_deref(), Some("jazz"));
    assert_eq!(field_value_of(&b, &xb, "tag").as_deref(), Some("rock"));

    // Synced again: still a conflict (skipped again), not a propagation.
    sync_pair(&a, &b, &intents, &["--on-conflict", "skip"]);
    assert_eq!(field_value_of(&a, &xa, "tag").as_deref(), Some("jazz"), "A was overwritten");
    assert_eq!(field_value_of(&b, &xb, "tag").as_deref(), Some("rock"), "B was overwritten");
    assert_ok(&mf(&[
        "sync",
        "plan",
        &a,
        &b,
        "--intents",
        intents.to_str().unwrap(),
        "--on-conflict",
        "skip",
    ]));
    assert_eq!(plan_ops(&a, &b, "conflict").len(), 1, "the conflict is planned again");
}

/// A `DaemonClient` over the test daemon that runs `hook` once, right after the
/// first request `fire` matches has been answered — a concurrent writer slipping
/// in during a sync run.
struct Interloper<'a> {
    inner: metafolder_cli::client::Client,
    fire: &'a dyn Fn(&str, &str) -> bool,
    hook: std::cell::RefCell<Option<Box<dyn FnOnce() + 'a>>>,
}

impl metafolder_core::daemon_client::DaemonClient for Interloper<'_> {
    fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value, metafolder_core::daemon_client::DaemonError> {
        let out = self.inner.request_daemon(method, path, body);
        if out.is_ok() && (self.fire)(method, path) {
            if let Some(hook) = self.hook.borrow_mut().take() {
                hook();
            }
        }
        out
    }
}

struct SilentPrompter;

impl metafolder_core::sync::Prompter for SilentPrompter {
    fn resolve_conflict(
        &self,
        _: &metafolder_core::sync::ConflictQuestion,
    ) -> Result<metafolder_core::sync::Resolution, metafolder_core::sync::SyncError> {
        Ok(metafolder_core::sync::Resolution::Skip)
    }
    fn confirm(&self, _: &str) -> Result<bool, metafolder_core::sync::SyncError> {
        Ok(true)
    }
    fn warn(&self, _: &str) {}
}

/// A change made to a record *while* a run propagates it is not swallowed: the
/// run commits the versions it read and wrote, never ones it re-reads after the
/// fact — which would declare the pair in sync with a change on one side that
/// never reached the other.
#[test]
fn test_sync_run_does_not_absorb_a_concurrent_change() {
    let (a, _adir) = tracked_repo("race_a", &[("doc.txt", b"x")]);
    let (b, _bdir) = tracked_repo("race_b", &[("doc.txt", b"x")]);
    let intents = write_intents(
        "race",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    sync_pair(&a, &b, &intents, &[]);
    let xa = query_one(&a, "mfr_path:value = \"doc.txt\"");
    let xb = query_one(&b, "mfr_path:value = \"doc.txt\"");
    assert_ok(&mf(&["-u", &a, "metarecord", "-i", &xa, "field", "set", "tag:string=one"]));
    assert_ok(&mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]));

    // The run writes `tag` on B; right then, A changes again.
    let fire = |method: &str, path: &str| method == "PUT" && path.contains(&xb);
    let client = Interloper {
        inner: metafolder_cli::client::Client::new(daemon_url()),
        fire: &fire,
        hook: std::cell::RefCell::new(Some(Box::new(|| {
            assert_ok(&mf(&[
                "-u",
                &a,
                "metarecord",
                "-i",
                &xa,
                "field",
                "set",
                "note:string=late",
            ]));
        }))),
    };
    let ctx = metafolder_core::sync::SyncCtx {
        client: &client,
        prompter: &SilentPrompter,
        page_size: 500,
    };
    metafolder_core::sync::run::run(&ctx, &a, &b, true).expect("run");
    assert!(client.hook.borrow().is_none(), "the interloper never ran");
    assert_eq!(field_value_of(&b, &xb, "tag").as_deref(), Some("one"));

    let status = mf(&["sync", "status", &a, &b]);
    assert!(!status.stdout.contains("in_sync"), "the late change is pending: {}", status.stdout);
    sync_pair(&a, &b, &intents, &[]);
    assert_eq!(field_value_of(&b, &xb, "note").as_deref(), Some("late"), "propagated next time");
}

/// A `tree_ref` field other than `mfr_path` — a position in a user forest — is
/// synced like any field: placed at the same path on the other side (its
/// out-of-scope parent found or created there), and moved when it moves.
#[test]
fn test_sync_carries_a_user_tree_ref() {
    let (a, _ar) = init_repo("tref_user_a");
    let (b, _br) = init_repo("tref_user_b");
    let music = create_metarecord(&a, &["category:tree_ref=/music"]);
    let jazz =
        create_metarecord(&a, &[&format!("category:tree_ref={music}/jazz"), "kind:string=x"]);
    let intents =
        write_intents("tref_user", &format!("[[intents]]\nrepo = '{a}'\nquery = 'kind = \"x\"'\n"));
    sync_pair(&a, &b, &intents, &[]);

    let jazz_b = query_one(&b, "kind = \"x\"");
    let out = mf(&["-u", &b, "metarecord", "-i", &jazz_b, "get", "--resolve-tree", "category"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "music/jazz", "placed under a `music` root made in B");

    // Renamed on A: the rename follows.
    assert_ok(&mf(&[
        "-u",
        &a,
        "metarecord",
        "-i",
        &jazz,
        "field",
        "set",
        &format!("category:tree_ref={music}/bebop"),
    ]));
    sync_pair(&a, &b, &intents, &[]);
    let out = mf(&["-u", &b, "metarecord", "-i", &jazz_b, "get", "--resolve-tree", "category"]);
    assert_eq!(out.stdout.trim(), "music/bebop");
}

/// A `ref` changed after the first sync is translated too, not only on the
/// first sync.
#[test]
fn test_sync_resync_translates_a_changed_ref() {
    let (a, _adir) = tracked_repo("reref_a", &[("doc.txt", b"x")]);
    let (b, _bdir) = tracked_repo("reref_b", &[]);
    let xa = query_one(&a, "mfr_path:value = \"doc.txt\"");
    let alice = create_metarecord(&a, &["name:string=alice"]);
    let bob = create_metarecord(&a, &["name:string=bob"]);
    let set_author = |who: &str| {
        assert_ok(&mf(&[
            "-u",
            &a,
            "metarecord",
            "-i",
            &xa,
            "field",
            "set",
            &format!("author:ref={who}"),
        ]));
    };
    set_author(&alice);
    let intents = write_intents(
        "reref",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\" OR name IS PRESENT'\n"),
    );
    sync_pair(&a, &b, &intents, &[]);
    let xb = query_one(&b, "mfr_path:value = \"doc.txt\"");
    let alice_b = query_one(&b, "name = \"alice\"");
    assert_eq!(field_value_of(&b, &xb, "author").as_deref(), Some(alice_b.as_str()));

    set_author(&bob);
    sync_pair(&a, &b, &intents, &[]);
    let bob_b = query_one(&b, "name = \"bob\"");
    assert_eq!(field_value_of(&b, &xb, "author").as_deref(), Some(bob_b.as_str()));
}

/// The pair's link states, as `mf sync status --json` reports them.
fn link_states(a: &str, b: &str) -> Vec<String> {
    let out = mf(&["sync", "status", a, b, "--json"]);
    assert_ok(&out);
    let body: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    body["links"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["state"].as_str().unwrap().to_string())
        .collect()
}

/// What the watcher's echo of the run's own file writes would do, without
/// waiting for it: re-read every file's stat fields (reconcile writes only the
/// ones that differ, as the watcher's refresh does).
fn echo(repo: &str) {
    assert_ok(&mf(&["-u", repo, "reconcile", "--no-mime", "--no-metadata"]));
}

/// A file edited on one side after the first sync: the new bytes go to the
/// other side, and the run's own write is not mistaken, afterwards, for a new
/// change of that side (doc "Suppressing sync's echoes").
#[test]
fn test_sync_propagates_a_content_change() {
    let (a, _adir) = tracked_repo("content_a", &[("doc.txt", b"first")]);
    let (b, _bdir) = tracked_repo("content_b", &[]);
    let intents = write_intents(
        "content",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    sync_pair(&a, &b, &intents, &[]);
    echo(&b);
    assert_eq!(link_states(&a, &b), ["in_sync"], "the first copy's echo changed nothing");

    std::fs::write(repo_root_of(&a).join("doc.txt"), b"second, longer").unwrap();
    echo(&a);
    sync_pair(&a, &b, &intents, &[]);
    assert_eq!(std::fs::read(repo_root_of(&b).join("doc.txt")).unwrap(), b"second, longer");
    echo(&b);
    assert_eq!(link_states(&a, &b), ["in_sync"]);
    // The overwritten bytes are in B's trash, not destroyed.
    let trash = mf(&["-u", &b, "trash", "list"]);
    assert!(trash.stdout.contains("sync"), "{}", trash.stdout);
}

/// The watcher flushing between the moment a copy's old entry went to the
/// trash and the moment the copy takes its name: its departure, applied alone,
/// orphaned the linked record, so the run's refresh was refused, the link left
/// uncommitted with both sides changed — a conflict — and the copy arrived as
/// a new record. The run holds the path's events across that window (doc
/// "Running a sync").
#[test]
fn test_sync_a_flush_between_trash_and_rename_keeps_the_linked_record() {
    let (a, _adir) = tracked_repo("window_a", &[("doc.txt", b"first")]);
    let (b, _bdir) = tracked_repo("window_b", &[]);
    let intents = write_intents(
        "window",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    sync_pair(&a, &b, &intents, &[]);
    echo(&b);
    assert_eq!(link_states(&a, &b), ["in_sync"]);
    let linked = query_one(&b, "mfr_path:value = \"doc.txt\"");

    std::fs::write(repo_root_of(&a).join("doc.txt"), b"second, longer").unwrap();
    echo(&a);
    assert_ok(&mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]));

    let daemon = metafolder_cli::client::Client::new(daemon_url());
    fn events_at(daemon: &metafolder_cli::client::Client, repo: &str, path: &str) -> u64 {
        let body = serde_json::json!({"paths": [path]});
        let out =
            daemon.request_daemon("POST", &format!("/repos/{repo}/watch/activity"), Some(&body));
        out.unwrap()["results"][0]["events"].as_u64().unwrap()
    }
    let before = events_at(&daemon, &b, "/doc.txt");
    // Wait, inside the window, until the watcher has received the departure
    // and flushed everything it may: the record is then orphaned — or the
    // path held, so that nothing was applied to it.
    let repo = b.clone();
    let flushed_inside_the_window = move |_: &std::path::Path| {
        let daemon = metafolder_cli::client::Client::new(daemon_url());
        for _ in 0..1500 {
            let watch =
                daemon.request_daemon("GET", &format!("/repos/{repo}/watch"), None).unwrap();
            let received = events_at(&daemon, &repo, "/doc.txt") > before;
            let drained = watch["pending_events"].as_u64() == Some(0);
            let held =
                watch["held_paths"].as_array().is_some_and(|h| h.iter().any(|p| p == "/doc.txt"));
            let at_path = ["-u", &repo, "metarecord", "-q", "mfr_path:value = \"doc.txt\"", "get"];
            let orphaned = mf(&at_path).stdout.trim().is_empty();
            if received && (held || (drained && orphaned)) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("the watcher neither flushed nor held the departure");
    };
    metafolder_core::sync::content::hooks::BETWEEN_TRASH_AND_RENAME
        .with(|h| *h.borrow_mut() = Some(Box::new(flushed_inside_the_window)));
    let ctx = metafolder_core::sync::SyncCtx {
        client: &daemon,
        prompter: &SilentPrompter,
        page_size: 500,
    };
    let ran = metafolder_core::sync::run::run(&ctx, &a, &b, true);
    metafolder_core::sync::content::hooks::BETWEEN_TRASH_AND_RENAME
        .with(|h| *h.borrow_mut() = None);
    ran.expect("run");

    assert_eq!(std::fs::read(repo_root_of(&b).join("doc.txt")).unwrap(), b"second, longer");
    echo(&b);
    assert_eq!(link_states(&a, &b), ["in_sync"]);
    assert_eq!(query_one(&b, "mfr_path:value = \"doc.txt\""), linked, "the same record");
}

/// Both sides edit the same file: a content conflict, decided by policy.
#[test]
fn test_sync_content_conflict_is_decided_by_policy() {
    let (a, _adir) = tracked_repo("cconf_a", &[("doc.txt", b"base")]);
    let (b, _bdir) = tracked_repo("cconf_b", &[]);
    let intents = write_intents(
        "cconf",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    sync_pair(&a, &b, &intents, &[]);
    std::fs::write(repo_root_of(&a).join("doc.txt"), b"from a").unwrap();
    std::fs::write(repo_root_of(&b).join("doc.txt"), b"from b!").unwrap();
    echo(&a);
    echo(&b);

    assert_ok(&mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]));
    let conflicts = plan_ops(&a, &b, "conflict");
    assert_eq!(conflicts.len(), 1, "{conflicts:?}");
    let field =
        conflicts[0]["fields"].as_array().unwrap().iter().find(|f| f["name"] == "plan_field");
    assert_eq!(field.unwrap()["value"]["value"], "mfr_content");

    sync_pair(&a, &b, &intents, &["--on-conflict", &format!("prefer:{b}")]);
    assert_eq!(std::fs::read(repo_root_of(&a).join("doc.txt")).unwrap(), b"from b!");
    echo(&a);
    assert_eq!(link_states(&a, &b), ["in_sync"]);
}

/// A mode changed on one side follows to the other.
#[cfg(unix)]
#[test]
fn test_sync_propagates_a_mode_change() {
    use std::os::unix::fs::PermissionsExt;
    let (a, _adir) = tracked_repo("mode_a", &[("run.sh", b"#!/bin/sh\n")]);
    let (b, _bdir) = tracked_repo("mode_b", &[]);
    let intents = write_intents(
        "mode",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    sync_pair(&a, &b, &intents, &[]);
    let file_a = repo_root_of(&a).join("run.sh");
    std::fs::set_permissions(&file_a, std::fs::Permissions::from_mode(0o750)).unwrap();
    echo(&a);
    sync_pair(&a, &b, &intents, &[]);
    let mode = std::fs::metadata(repo_root_of(&b).join("run.sh")).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o750);
    echo(&b);
    assert_eq!(link_states(&a, &b), ["in_sync"]);
}

/// An empty directory and a symlink reach the other side as what they are —
/// not as nothing, and not as the bytes the link points at.
#[cfg(unix)]
#[test]
fn test_sync_creates_a_directory_and_a_symlink() {
    let (a, adir) = tracked_repo("kinds_a", &[]);
    std::fs::create_dir(adir.join("empty")).unwrap();
    std::fs::write(adir.join("target.txt"), b"t").unwrap();
    std::os::unix::fs::symlink("target.txt", adir.join("link")).unwrap();
    assert_ok(&mf(&["-u", &a, "reconcile"]));
    let (b, _bdir) = tracked_repo("kinds_b", &[]);
    let intents = write_intents(
        "kinds",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type IS PRESENT'\n"),
    );
    sync_pair(&a, &b, &intents, &[]);
    let root_b = repo_root_of(&b);
    assert!(root_b.join("empty").is_dir(), "the empty directory is made");
    let link = std::fs::symlink_metadata(root_b.join("link")).unwrap();
    assert!(link.file_type().is_symlink(), "a symlink stays a symlink");
    assert_eq!(std::fs::read_link(root_b.join("link")).unwrap(), PathBuf::from("target.txt"));
}

/// A link whose two records are both gone is removed by the next sync,
/// whatever the scope (a deleted record matches no query).
#[test]
fn test_sync_drops_a_link_whose_records_are_both_gone() {
    let (a, _ar) = init_repo("dead_a");
    let (b, _br) = init_repo("dead_b");
    let ra = create_metarecord(&a, &["kind:string=x"]);
    let rb = create_metarecord(&b, &["kind:string=x"]);
    assert_ok(&mf(&["sync", "link", &a, &b, &ra, &rb]));
    assert_ok(&mf(&["-u", &a, "metarecord", "-i", &ra, "delete"]));
    assert_ok(&mf(&["-u", &b, "metarecord", "-i", &rb, "delete"]));
    assert_eq!(link_states(&a, &b), ["missing_both"]);

    let intents =
        write_intents("dead", &format!("[[intents]]\nrepo = '{a}'\nquery = 'kind = \"x\"'\n"));
    sync_pair(&a, &b, &intents, &[]);
    assert!(link_states(&a, &b).is_empty(), "the dead link is gone");
}

/// A run commits each link on its own, as soon as it is synced: a stop
/// costs no other link's work (doc "Why a run commits each link on its own").
#[test]
fn test_sync_run_commits_each_link_on_its_own() {
    let (a, _ar) = init_repo("each_a");
    let (b, _br) = init_repo("each_b");
    create_metarecord(&a, &["kind:string=x", "n:int=1"]);
    create_metarecord(&a, &["kind:string=x", "n:int=2"]);
    let intents =
        write_intents("each", &format!("[[intents]]\nrepo = '{a}'\nquery = 'kind = \"x\"'\n"));
    assert_ok(&mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]));
    let commits = std::cell::Cell::new(0);
    let fire = |method: &str, path: &str| {
        if method == "POST" && path.ends_with("/links/commit") {
            commits.set(commits.get() + 1);
        }
        false
    };
    let client = Interloper {
        inner: metafolder_cli::client::Client::new(daemon_url()),
        fire: &fire,
        hook: std::cell::RefCell::new(None),
    };
    let ctx = metafolder_core::sync::SyncCtx {
        client: &client,
        prompter: &SilentPrompter,
        page_size: 500,
    };
    metafolder_core::sync::run::run(&ctx, &a, &b, true).expect("run");
    assert_eq!(commits.get(), 2, "one commit per link");
    assert_eq!(link_states(&a, &b), ["in_sync", "in_sync"]);
}

/// `mf sync plan --host` chooses where the pair's state lives: the plan repo,
/// and the sync database the run creates with the first link.
#[test]
fn test_sync_host_holds_the_sync_database() {
    let (a, _ar) = init_repo("host_a");
    let (b, _br) = init_repo("host_b");
    create_metarecord(&a, &["kind:string=x"]);
    let intents =
        write_intents("host", &format!("[[intents]]\nrepo = '{a}'\nquery = 'kind = \"x\"'\n"));
    // The one that is not canonical A, so the default would be the other.
    let host = if a < b { &b } else { &a };
    sync_pair(&a, &b, &intents, &["--host", host]);
    let client = metafolder_cli::client::Client::new(daemon_url());
    let links = client.request_daemon("GET", &format!("/sync/{a}/{b}/links"), None).unwrap();
    assert_eq!(links["host"], host.as_str(), "{links}");
}

#[test]
fn test_sync_show_renders_plan_status() {
    // After a plan, show lists the ops as green (baselines current); changing a
    // planned record flips its ops to red (will be skipped).
    let (a, _adir) = tracked_repo("show_a", &[("doc.txt", b"x")]);
    let (b, _bdir) = tracked_repo("show_b", &[]);
    let intents = write_intents(
        "show",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    assert_ok(&mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]));

    // Summary lists the op kinds.
    let sum = mf(&["sync", "show", &a, &b, "--summary"]);
    assert_ok(&sum);
    assert!(
        sum.stdout.contains("create-link")
            && sum.stdout.contains("sync")
            && sum.stdout.contains("copy"),
        "summary lists kinds: {}",
        sum.stdout
    );

    // Default view: nothing changed → all will run.
    let def = mf(&["sync", "show", &a, &b]);
    assert_ok(&def);
    assert!(def.stdout.contains("all operations will run"), "all green: {}", def.stdout);

    // Change the source record → its ops turn red.
    let xa = query_one(&a, "mfr_path:value = \"doc.txt\"");
    assert_ok(&mf(&["-u", &a, "metarecord", "-i", &xa, "field", "add", "tag:string=z"]));
    let red = mf(&["sync", "show", &a, &b]);
    assert_ok(&red);
    assert!(
        red.stdout.contains("will be skipped") && red.stdout.contains("[skip]"),
        "reds shown after change: {}",
        red.stdout
    );

    // --files shows only disk ops.
    let files = mf(&["sync", "show", &a, &b, "--files"]);
    assert_ok(&files);
    assert!(
        files.stdout.contains("copy") && !files.stdout.contains("create-link"),
        "files view: {}",
        files.stdout
    );
}

/// A repository the daemon does not know is said to be unknown — not taken
/// for a daemon too old to scan for duplicates, which a 404 *without* an
/// error of the daemon's own means.
#[test]
fn test_a_scan_of_an_unknown_repository_says_so() {
    let unknown = uuid::Uuid::new_v4().as_simple().to_string();
    for args in [&["duplicate", "scan"][..], &["orphan", "relink"][..]] {
        let mut full = vec!["-u", unknown.as_str()];
        full.extend_from_slice(args);
        let out = mf(&full);
        assert_eq!(out.code, 1, "{args:?}: {}", out.stderr);
        assert!(out.stderr.contains("Repository not found"), "{args:?}: {}", out.stderr);
    }
}
