use super::*;

// ── File tracking ─────────────────────────────────────────────────────────────

/// `metarecord add --path` records one path, beside the given fields, and
/// refuses a path that already has its metarecord (doc "Recording a path").
#[test]
fn test_metarecord_add_path_records_a_path_once() {
    let (repo, root) = init_repo("addpath");
    std::fs::write(root.join("file.txt"), b"hello").unwrap();
    let path = root.join("file.txt");
    let path = path.to_str().unwrap();

    let out = mf(&["-u", &repo, "metarecord", "add", "--path", path, "rating:int=3"]);
    assert_ok(&out);
    let uuid = out.stdout.trim().to_string();
    assert!(is_hex_uuid(&uuid), "stdout: {}", out.stdout);
    let field = |name: &str| {
        let out = mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "get", name]);
        assert_ok(&out);
        out.stdout.trim().to_string()
    };
    assert_eq!(field("rating"), "3");
    assert_eq!(field("mf_watch"), "false", "recorded, not followed");
    assert_eq!(field("mfr_size"), "5");

    // Adding is not finding: a second add is refused and names the first.
    let out = mf(&["-u", &repo, "metarecord", "add", "--path", path]);
    assert_eq!(out.code, 1, "stdout: {}", out.stdout);
    assert!(out.stderr.contains("already has a metarecord"), "stderr: {}", out.stderr);
    assert!(out.stderr.contains(&uuid), "stderr: {}", out.stderr);

    // Outside the repository root → operation error.
    let outside = temp_dir("addpath_outside");
    std::fs::write(outside.join("f.txt"), b"x").unwrap();
    let f = outside.join("f.txt");
    let out = mf(&["-u", &repo, "metarecord", "add", "--path", f.to_str().unwrap()]);
    assert_eq!(out.code, 1, "stderr: {}", out.stderr);

    // --parents needs --path; without --path, specs are required.
    assert_eq!(mf(&["-u", &repo, "metarecord", "add", "--parents", "a:int=1"]).code, 2);
    assert_eq!(mf(&["-u", &repo, "metarecord", "add"]).code, 2);
}

/// A path whose parent directories have no metarecord is refused with their
/// list, until `--parents` asks for them.
#[test]
fn test_metarecord_add_path_lists_missing_parents() {
    let (repo, root) = init_repo("addpath_parents");
    std::fs::create_dir_all(root.join("docs/notes")).unwrap();
    std::fs::write(root.join("docs/notes/todo.txt"), b"todo").unwrap();
    let path = root.join("docs/notes/todo.txt");
    let path = path.to_str().unwrap();

    let out = mf(&["-u", &repo, "metarecord", "add", "--path", path]);
    assert_eq!(out.code, 1, "stdout: {}", out.stdout);
    assert!(out.stderr.contains("/docs, /docs/notes"), "stderr: {}", out.stderr);
    assert!(out.stderr.contains("--parents"), "stderr: {}", out.stderr);

    let out = mf(&["-u", &repo, "metarecord", "add", "--parents", "--path", path]);
    assert_ok(&out);
    let uuid = out.stdout.trim();
    let notes = query_one(&repo, r#"mfr_path = "/docs/notes""#);
    let out = mf(&["-u", &repo, "metarecord", "-i", &notes, "field", "get", "mf_watch"]);
    assert_eq!(out.stdout.trim(), "", "a created parent inherits mf_watch");
    assert_eq!(query_one(&repo, r#"mfr_path = "/docs/notes/todo.txt""#), uuid);
}

/// `mf uuid <path>` is `mf path`'s inverse: the metarecord standing for a path.
#[test]
fn test_uuid_finds_the_metarecord_at_a_path() {
    let (repo, root) = init_repo("uuidof");
    std::fs::create_dir_all(root.join("dir")).unwrap();
    std::fs::write(root.join("dir/a.txt"), b"a").unwrap();
    let a = root.join("dir/a.txt");
    let a = a.to_str().unwrap();

    let out = mf(&["-u", &repo, "uuid", a]);
    assert_eq!(out.code, 1, "an unrecorded path: stdout {}", out.stdout);
    assert!(out.stderr.contains("no metarecord"), "stderr: {}", out.stderr);

    let made = mf(&["-u", &repo, "metarecord", "add", "--parents", "--path", a]);
    assert_ok(&made);
    let out = mf(&["-u", &repo, "uuid", a]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), made.stdout.trim());

    // The root is a path like any other; a path gone from disk is taken as
    // repo-root-relative when it starts with '/' (a stale mfr_path).
    let out = mf(&["-u", &repo, "uuid", root.to_str().unwrap()]);
    assert_ok(&out);
    let out = mf(&["-u", &repo, "path", "--relative", out.stdout.trim()]);
    assert_eq!(out.stdout.trim(), "/", "the root's own metarecord: {}", out.stdout);
    std::fs::remove_file(root.join("dir/a.txt")).unwrap();
    let out = mf(&["-u", &repo, "uuid", "/dir/a.txt"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), made.stdout.trim());
    let out = mf(&["-u", &repo, "uuid", "not/there"]);
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
}

#[test]
fn test_reconcile_reports_created_entries() {
    let (repo, root) = init_repo("reconcile");
    std::fs::write(root.join("a.txt"), b"aaa").unwrap();
    std::fs::write(root.join("b.txt"), b"bbb").unwrap();

    // The repository starts with a single entry: the filesystem root.
    let out = mf(&["-u", &repo, "metarecord", "get"]);
    assert_ok(&out);
    let root_uuid = out.stdout.trim().to_string();
    assert!(is_hex_uuid(&root_uuid));

    let out =
        mf(&["-u", &repo, "metarecord", "-i", &root_uuid, "field", "set", "mf_watch:bool=true"]);
    assert_ok(&out);

    let out = mf(&["-u", &repo, "reconcile"]);
    assert_ok(&out);
    // a.txt + b.txt only: .metafolder is ignored by default (hidden-entry
    // and .metafolder patterns), so config.json under it is not tracked.
    assert!(out.stdout.starts_with("created: 2  moved: 0"), "unexpected summary: {}", out.stdout);

    // A second reconcile is a no-op; --json prints the raw body.
    let out = mf(&["-u", &repo, "reconcile", "--json"]);
    assert_ok(&out);
    let parsed: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(parsed["created"], 0);
    assert_eq!(parsed["moved"], 0);
}

// ── Ignore presets (doc "Ignore presets") ─────────────────────

/// The root metarecord's mf_ignore rows, one per line.
fn root_ignore(repo: &str) -> String {
    let root_uuid = mf(&["-u", repo, "metarecord", "get"]).stdout.trim().to_string();
    let out = mf(&["-u", repo, "metarecord", "-i", &root_uuid, "field", "get", "mf_ignore"]);
    assert_ok(&out);
    out.stdout
}

#[test]
fn test_repo_init_applies_default_ignore_preset() {
    let (repo, _root) = init_repo("ign_default");
    let ignore = root_ignore(&repo);
    // The default preset lands on the root: cargo build intermediates, git,
    // and the hidden-entry pattern are all present.
    assert!(ignore.contains("incremental"), "cargo pattern missing: {ignore}");
    assert!(ignore.contains(r"\.git"), "git pattern missing: {ignore}");
    assert!(ignore.contains(r"node_modules"), "node pattern missing: {ignore}");
}

#[test]
fn test_repo_init_no_ignore_leaves_empty() {
    let root = temp_dir("ign_none");
    let out = mf_cfg(&["repo", "init", root.to_str().unwrap(), "--no-ignore"]);
    assert_ok(&out);
    let repo = out.stdout.trim().to_string();
    assert!(root_ignore(&repo).trim().is_empty(), "root should have no mf_ignore rows");
}

#[test]
fn test_ignore_list_shows_presets() {
    let out = mf_cfg(&["ignore", "list"]);
    assert_ok(&out);
    assert!(out.stdout.contains("rust-build"), "list: {}", out.stdout);
    assert!(out.stdout.contains("default"), "list: {}", out.stdout);
}

#[test]
fn test_ignore_set_add_remove_on_root() {
    let (repo, _root) = init_repo("ign_ops");

    // set replaces the whole set with just the `node` preset.
    let out = mf_cfg(&["-u", &repo, "ignore", "set", "node"]);
    assert_ok(&out);
    let ignore = root_ignore(&repo);
    assert!(ignore.contains("node_modules"));
    assert!(!ignore.contains("incremental"), "set must have dropped the default patterns");

    // add appends the `python` preset without dropping node.
    assert_ok(&mf_cfg(&["-u", &repo, "ignore", "add", "python"]));
    let ignore = root_ignore(&repo);
    assert!(ignore.contains("node_modules"));
    assert!(ignore.contains("__pycache__"));

    // remove drops exactly the node pattern.
    assert_ok(&mf_cfg(&["-u", &repo, "ignore", "remove", "node"]));
    let ignore = root_ignore(&repo);
    assert!(!ignore.contains("node_modules"), "node removed: {ignore}");
    assert!(ignore.contains("__pycache__"), "python kept: {ignore}");

    // comma-separated names are accepted in one argument.
    assert_ok(&mf_cfg(&["-u", &repo, "ignore", "set", "node,git"]));
    let ignore = root_ignore(&repo);
    assert!(ignore.contains("node_modules") && ignore.contains(r"\.git"));
}

#[test]
fn test_ignore_targets_the_repo_root_given_as_an_explicit_dir() {
    // `-d <repo root>` and no `-d` must name the same target: the root has no
    // parent directory, so the exact-path lookup cannot resolve it.
    let (repo, root) = init_repo("ign_root_dir");
    let out = mf_cfg(&["-u", &repo, "ignore", "set", "node", "-d", root.to_str().unwrap()]);
    assert_ok(&out);
    let ignore = root_ignore(&repo);
    assert!(ignore.contains("node_modules"), "written on the root: {ignore}");

    let out = mf_cfg(&["-u", &repo, "ignore", "list", "-d", root.to_str().unwrap()]);
    assert_ok(&out);
    assert!(out.stdout.contains("node_modules"), "active patterns listed: {}", out.stdout);
}

#[test]
fn test_ignore_add_unknown_preset_is_usage_error() {
    let (repo, _root) = init_repo("ign_bad");
    let out = mf_cfg(&["-u", &repo, "ignore", "add", "does-not-exist"]);
    assert_eq!(out.code, 2, "unknown preset should be a usage error (exit 2): {}", out.stderr);
}

#[test]
fn test_reconcile_no_wait_and_task_commands() {
    let (repo, root) = init_repo("notasks");
    std::fs::write(root.join("a.txt"), b"a").unwrap();

    // --no-wait starts the reconcile and prints just the task id.
    let out = mf(&["-u", &repo, "reconcile", "--no-wait"]);
    assert_ok(&out);
    let task_id = out.stdout.trim().to_string();
    assert!(is_hex_uuid(&task_id), "expected a task id, got: '{}'", out.stdout);

    // mf task <id> shows that task (id + kind on the line).
    let out = mf(&["-u", &repo, "task", "show", &task_id]);
    assert_ok(&out);
    assert!(out.stdout.contains(&task_id), "task line: {}", out.stdout);
    assert!(out.stdout.contains("reconcile"), "task line: {}", out.stdout);

    // mf tasks lists it (retained after completion within the TTL).
    let out = mf(&["-u", &repo, "task", "list"]);
    assert_ok(&out);
    assert!(out.stdout.contains(&task_id), "tasks output: {}", out.stdout);

    // The tiny reconcile has finished, so stopping it is a conflict reported on
    // stderr with a non-zero exit (the happy-path stop is covered by the daemon
    // integration tests; it is racy through the CLI on a trivially small repo).
    let out = mf(&["-u", &repo, "task", "show", &task_id, "--stop"]);
    assert_eq!(out.code, 1, "stopping a finished task should fail; stderr: {}", out.stderr);
    assert!(out.stderr.contains("error:"), "stderr: {}", out.stderr);
}

#[test]
fn test_task_stop_unknown_id_errors() {
    let (repo, _root) = init_repo("stopghost");
    let ghost = uuid::Uuid::new_v4().as_simple().to_string();
    let out = mf(&["-u", &repo, "task", "show", &ghost, "--stop"]);
    assert_eq!(out.code, 1, "stopping an unknown task should fail; stderr: {}", out.stderr);
    assert!(out.stderr.contains("error:"), "stderr: {}", out.stderr);
}

#[test]
fn test_reconcile_single_entry() {
    let (repo, root) = init_repo("reconcile_metarecord");
    std::fs::create_dir_all(root.join("dir")).unwrap();
    std::fs::write(root.join("dir/inside.txt"), b"in").unwrap();

    let out = mf(&["-u", &repo, "metarecord", "add", "--path", root.join("dir").to_str().unwrap()]);
    assert_ok(&out);
    let dir_uuid = out.stdout.trim().to_string();

    let out =
        mf(&["-u", &repo, "metarecord", "-i", &dir_uuid, "field", "set", "mf_watch:bool=true"]);
    assert_ok(&out);
    let out = mf(&["-u", &repo, "reconcile", "--metarecord", &dir_uuid]);
    assert_ok(&out);
    assert!(out.stdout.starts_with("created: 1"), "unexpected summary: {}", out.stdout);
}

#[test]
fn test_reconcile_threshold_yields_similarity_candidate() {
    let (repo, root) = init_repo("reconcile_sim");
    std::fs::create_dir_all(root.join("music")).unwrap();
    std::fs::write(root.join("music/old_song.mp3"), vec![b'a'; 1000]).unwrap();

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

    // Move + modify: different name and size defeat the fingerprint phase.
    std::fs::remove_file(root.join("music/old_song.mp3")).unwrap();
    std::fs::write(root.join("music/old_song_v2.mp3"), vec![b'b'; 1100]).unwrap();

    let out = mf(&["-u", &repo, "reconcile", "--threshold", "0.6", "--json"]);
    assert_ok(&out);
    let parsed: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    let matches = &parsed["candidates"][0]["matches"][0];
    assert_eq!(matches["fingerprint"], "similarity", "body: {}", out.stdout);
    assert!(matches["score"].as_f64().unwrap() >= 0.6);

    // An out-of-range threshold is rejected by the daemon.
    let bad = mf(&["-u", &repo, "reconcile", "--threshold", "2"]);
    assert_eq!(bad.code, 1, "stderr: {}", bad.stderr);
}

#[test]
fn test_reconcile_computes_and_can_disable_mime() {
    let (repo, root) = init_repo("reconcile_mime");
    // PNG magic header → infer detects image/png.
    std::fs::write(root.join("pic.png"), [0x89u8, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0])
        .unwrap();

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

    // With --no-mime, no mfr_mime is written.
    assert_ok(&mf(&["-u", &repo, "reconcile", "--no-mime"]));
    let q = mf(&["-u", &repo, "metarecord", "-q", "mfr_mime IS PRESENT", "get"]);
    assert_ok(&q);
    assert!(q.stdout.trim().is_empty(), "no mime expected, got: {}", q.stdout);

    // A default reconcile computes it.
    assert_ok(&mf(&["-u", &repo, "reconcile"]));
    let pic = mf(&[
        "-u",
        &repo,
        "metarecord",
        "-q",
        "mfr_mime = \"image/png\"",
        "get",
        "--select",
        "mfr_mime",
    ]);
    assert_ok(&pic);
    assert!(pic.stdout.contains("image/png"), "stdout: {}", pic.stdout);
}

// ── Watch check and the watch budget's exclusions ─────────────────────────────

/// A path that leads into a directory whose name is not UTF-8 (here through a
/// symlink, which canonicalisation follows) is checked at its exact bytes —
/// read as text, it named nothing and was "not watched" (doc "Tree names").
#[cfg(unix)]
#[test]
fn test_watch_check_and_uuid_reach_a_name_that_is_not_utf8() {
    use std::os::unix::ffi::OsStrExt;
    let (repo, root) = super::sync::tracked_repo("watchbytes", &[]);
    let raw = root.join(std::ffi::OsStr::from_bytes(b"caf\xe9"));
    std::fs::create_dir(&raw).unwrap();
    std::fs::write(raw.join("x.txt"), b"x").unwrap();
    std::os::unix::fs::symlink(&raw, root.join("link")).unwrap();
    assert_ok(&mf(&["-u", &repo, "reconcile"]));
    let through = root.join("link/x.txt");
    let through = through.to_str().unwrap();

    let out = mf(&["-u", &repo, "watch", "check", through]);
    assert_ok(&out);
    assert!(out.stdout.contains("/caf%E9/x.txt"), "shown escaped: {}", out.stdout);

    let x = query_one(&repo, r#"mfr_path = "/caf%E9/x.txt""#);
    let out = mf(&["-u", &repo, "uuid", through]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), x);
}

/// `mf watch check` answers one line per path, and says why a path is not
/// watched in terms a person can act on; any path not watched makes it exit 1.
#[test]
fn test_watch_check_explains_each_path() {
    let (repo, root) = init_repo("watchcheck");
    std::fs::create_dir_all(root.join("music")).unwrap();
    std::fs::write(root.join("music/a.mp3"), b"a").unwrap();
    std::fs::create_dir_all(root.join(".hidden")).unwrap();
    std::fs::write(root.join(".hidden/x"), b"x").unwrap();
    let a = root.join("music/a.mp3");
    let a = a.to_str().unwrap();

    // Tracking is opt-in: a fresh repository's root says `mf_watch = false`.
    let out = mf(&["-u", &repo, "watch", "check", a]);
    assert_eq!(out.code, 1, "stdout: {}\nstderr: {}", out.stdout, out.stderr);
    assert!(out.stdout.starts_with("NOT watched"), "stdout: {}", out.stdout);
    assert!(out.stdout.contains("/music/a.mp3"), "stdout: {}", out.stdout);
    assert!(out.stdout.contains("mf_watch = false inherited from /\n"), "{}", out.stdout);

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

    // The root itself is shown as `/`, and both paths are watched: exit 0.
    let out = mf(&["-u", &repo, "watch", "check", root.to_str().unwrap(), a]);
    assert_ok(&out);
    let lines: Vec<&str> = out.stdout.lines().collect();
    assert_eq!(lines.len(), 2, "stdout: {}", out.stdout);
    assert!(lines[0].starts_with("watched") && lines[0].contains("\t/\t"), "{}", lines[0]);
    assert!(lines[1].contains("/music/a.mp3") && lines[1].contains("recorded"), "{}", lines[1]);

    // The default preset ignores hidden entries: the pattern and its holder are named.
    let out = mf(&["-u", &repo, "watch", "check", root.join(".hidden/x").to_str().unwrap()]);
    assert_eq!(out.code, 1, "stdout: {}", out.stdout);
    assert!(out.stdout.contains("mf_ignore pattern"), "stdout: {}", out.stdout);

    // An mf_watch = false below the root names where it is inherited from.
    let music = query_one(&repo, r#"mfr_path = "/music""#);
    assert_ok(&mf(&[
        "-u",
        &repo,
        "metarecord",
        "-i",
        &music,
        "field",
        "set",
        "mf_watch:bool=false",
    ]));
    let out = mf(&["-u", &repo, "watch", "check", a]);
    assert_eq!(out.code, 1, "stdout: {}", out.stdout);
    assert!(out.stdout.contains("mf_watch = false inherited from /music\n"), "{}", out.stdout);

    // A path gone from disk is taken as repo-root-relative only with a leading slash.
    let out = mf(&["-u", &repo, "watch", "check", "/music/gone.mp3", "--json"]);
    assert_ok(&out);
    let body: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(body["results"][0]["path"], "/music/gone.mp3", "{body}");
    assert_eq!(body["results"][0]["watched"], false, "{body}");
    let out = mf(&["-u", &repo, "watch", "check", "nowhere/at/all"]);
    assert_eq!(out.code, 2, "a relative path that does not exist is a usage error");
    assert!(out.stderr.contains("must start with '/'"), "stderr: {}", out.stderr);
    let out = mf(&["-u", &repo, "watch", "check"]);
    assert_eq!(out.code, 2, "at least one path is required: {}", out.stderr);
}

/// `mf watch exceeded set` gives a subtree up, `list` shows it, `watch check`
/// says why a path inside is not watched, and `clear` watches it again.
#[test]
fn test_watch_exceeded_set_list_and_clear() {
    let (repo, root) = tracked_repo("watchexceeded", &[("big/f.txt", b"f")]);
    let f = root.join("big/f.txt");
    let f = f.to_str().unwrap();

    let out = mf(&["-u", &repo, "watch", "exceeded"]);
    assert_ok(&out);
    assert!(out.stdout.contains("no subtree is excluded"), "stdout: {}", out.stdout);

    let out = mf(&["-u", &repo, "watch", "exceeded", "set", "/big"]);
    assert_ok(&out);
    assert!(out.stdout.starts_with("/big: no longer watched"), "stdout: {}", out.stdout);

    let out = mf(&["-u", &repo, "watch", "exceeded", "list"]);
    assert_ok(&out);
    assert_eq!(out.stdout.lines().next(), Some("/big"), "stdout: {}", out.stdout);
    let out = mf(&["-u", &repo, "watch", "exceeded", "--json"]);
    assert_ok(&out);
    let body: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(body["exceeded"], serde_json::json!(["/big"]), "{body}");

    let out = mf(&["-u", &repo, "watch", "check", f]);
    assert_eq!(out.code, 1, "stdout: {}", out.stdout);
    assert!(out.stdout.contains("inside /big (mfr_watch_exceeded"), "stdout: {}", out.stdout);

    // The budget regime counts the exclusion in `watch status` too.
    let status: serde_json::Value =
        serde_json::from_str(&mf(&["-u", &repo, "watch", "status", "--json"]).stdout).unwrap();
    let out = mf(&["-u", &repo, "watch", "status"]);
    assert_ok(&out);
    if status["watch_budget"]["exceeded_dirs"].as_i64().unwrap_or(0) > 0 {
        assert!(out.stdout.contains("subtree(s) left unwatched"), "stdout: {}", out.stdout);
    }

    let out = mf(&["-u", &repo, "watch", "exceeded", "clear", "/big"]);
    assert_ok(&out);
    assert!(out.stdout.starts_with("/big: watched again"), "stdout: {}", out.stdout);
    assert_ok(&mf(&["-u", &repo, "watch", "check", f]));

    let out = mf(&["-u", &repo, "watch", "exceeded", "set", "/no/such/dir"]);
    assert_eq!(out.code, 1, "an untracked directory is an operation error");
    assert!(out.stderr.contains("No metarecord at /no/such/dir"), "stderr: {}", out.stderr);
}

// ── Orphans: relink, and the confirmation of clear ────────────────────────────

/// `mf orphan relink` re-homes an orphan onto the file now carrying its
/// content: the orphan keeps its uuid and its metadata.
#[test]
fn test_orphan_relink_re_homes_an_orphan() {
    let (repo, root) = tracked_repo("orphanrelink", &[("song.mp3", b"some audio content")]);
    let song = root.join("song.mp3");
    let original = query_one(&repo, r#"mfr_path = "/song.mp3""#);
    // The fingerprints the duplicate scan would have stored.
    let partial = metafolder_daemon::fingerprint::partial_hash(&song).unwrap();
    let full = metafolder_daemon::fingerprint::full_hash(&song).unwrap();
    for spec in
        [format!("mfr_partial_hash:string={partial}"), format!("mfr_full_hash:string={full}")]
    {
        let out =
            mf(&["-u", &repo, "metarecord", "-i", &original, "field", "set", &spec, "--force"]);
        assert_ok(&out);
    }
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &original, "field", "set", "rating:int=5"]));

    // Renamed while nothing was recorded: the old record is orphaned, and
    // reconcile tracks the new name afresh. (Not `mf track`: it writes
    // `mf_watch = false`, a field relink will not absorb.)
    assert_ok(&mf(&["-u", &repo, "watch", "pause"]));
    std::fs::rename(&song, root.join("renamed.mp3")).unwrap();
    assert_ok(&mf(&["-u", &repo, "orphan", "clear", "-y"]));
    assert_ok(&mf(&["-u", &repo, "reconcile"]));
    let fresh = query_one(&repo, r#"mfr_path = "/renamed.mp3""#);
    assert_ne!(fresh, original);

    let out = mf(&["-u", &repo, "orphan", "relink"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "relinked: 1  conflicts: 0  skipped: 0", "{}", out.stdout);
    let out = mf(&["-u", &repo, "path", &original]);
    assert_ok(&out);
    assert!(out.stdout.trim().ends_with("/renamed.mp3"), "stdout: {}", out.stdout);
    let out = mf(&["-u", &repo, "metarecord", "-i", &original, "field", "get", "rating"]);
    assert_eq!(out.stdout.trim(), "5", "the metadata came back with it");

    // Nothing left to do; --json prints the task's result.
    let out = mf(&["-u", &repo, "orphan", "relink", "--json"]);
    assert_ok(&out);
    let body: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(body["relinked"], 0, "{body}");

    // --no-wait prints the task id alone, which `mf task show` knows.
    let out = mf(&["-u", &repo, "orphan", "relink", "--no-wait"]);
    assert_ok(&out);
    let task = out.stdout.trim().to_string();
    assert!(!task.is_empty() && !task.contains(char::is_whitespace), "stdout: {}", out.stdout);
    assert_ok(&mf(&["-u", &repo, "task", "show", &task]));
}

/// Without `-y`, `mf orphan clear` lists what it would orphan on stderr and
/// asks; a refusal leaves the records alone.
#[test]
fn test_orphan_clear_asks_before_clearing() {
    let (repo, root) = init_repo("orphanask");
    let file = root.join("gone.txt");
    std::fs::write(&file, b"data").unwrap();
    let uuid = mf(&["-u", &repo, "metarecord", "add", "--path", file.to_str().unwrap()])
        .stdout
        .trim()
        .to_string();

    let out = mf(&["-u", &repo, "orphan", "clear"]);
    assert_ok(&out);
    assert!(out.stdout.contains("No orphans to clear."), "stdout: {}", out.stdout);

    std::fs::remove_file(&file).unwrap();
    let clear = |answer: &str| {
        mf_full(&["-p", daemon_port(), "-u", &repo, "orphan", "clear"], Some(answer), &[], false)
    };
    let out = clear("n\n");
    assert_ok(&out);
    assert!(out.stdout.contains("Aborted."), "stdout: {}", out.stdout);
    assert!(out.stderr.contains(&uuid) && out.stderr.contains("/gone.txt"), "{}", out.stderr);
    assert!(out.stderr.contains("Orphan 1 metarecord(s)"), "stderr: {}", out.stderr);
    let out = mf(&["-u", &repo, "orphan", "list"]);
    assert!(out.stdout.contains(&uuid), "a refusal clears nothing: {}", out.stdout);

    let out = clear("y\n");
    assert_ok(&out);
    assert!(out.stdout.contains("cleared 1"), "stdout: {}", out.stdout);
}
