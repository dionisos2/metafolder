use super::*;

// ── What the coverage pass left (roadmap "the untested parts") ────────────────

/// `mf repo check` on a store whose derived data was damaged behind the
/// daemon's back: one problem per line, the repair named on stderr, exit 1;
/// `mf repo reindex` makes it whole again.
#[test]
fn test_repo_check_reports_damage_and_reindex_repairs_it() {
    let (repo, root) = init_repo("check_damage");
    create_metarecord(&repo, &["note:string=kept"]);
    assert_ok(&mf(&["-u", &repo, "repo", "unload"]));
    {
        // Every set chunk dropped (the universe, presence, parents), as the
        // daemon's own test of `check` does (kv_derived.rs).
        let kv = root.join(".metafolder/internal/kv");
        // SAFETY: the repository is unloaded (see above), so nothing else
        // maps the file; no WRITEMAP.
        let env =
            unsafe { heed::EnvOpenOptions::new().max_dbs(32).map_size(1 << 30).open(&kv) }.unwrap();
        let r = env.read_txn().unwrap();
        let sets = env
            .open_databases_and_commit(r, |dbo, r| -> heed::Result<_> {
                let sets =
                    dbo.open_database::<heed::types::Bytes, heed::types::Bytes>(r, Some("sets"));
                Ok(heed::CommitOrAbort::<_, ()>::Commit(sets?.unwrap()))
            })
            .unwrap()
            .commit()
            .unwrap();
        let mut w = env.write_txn().unwrap();
        sets.clear(&mut w).unwrap();
        w.commit().unwrap();
    }
    assert_ok(&mf(&["repo", "load", root.to_str().unwrap()]));

    let out = mf(&["-u", &repo, "repo", "check"]);
    assert_eq!(out.code, 1, "problems found: exit 1\nstdout: {}", out.stdout);
    assert!(!out.stdout.trim().is_empty() && !out.stdout.contains("ok\n"), "{}", out.stdout);
    assert!(out.stderr.contains("problem(s)"), "{}", out.stderr);
    assert!(out.stderr.contains("mf repo reindex"), "{}", out.stderr);

    assert_ok(&mf(&["-u", &repo, "repo", "reindex"]));
    let out = mf(&["-u", &repo, "repo", "check"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "ok");
}

/// `mf repo restore --from DIR` puts a named backup back (not the most recent
/// one); `--metafolder DIR` addresses the repository by its `.metafolder`.
#[test]
fn test_repo_restore_from_a_named_backup_and_by_metafolder() {
    let (repo, root) = init_repo("restore_from");
    let first = create_metarecord(&repo, &["note:string=first"]);
    let named = mf(&["-u", &repo, "repo", "backup"]);
    assert_ok(&named);
    let named = named.stdout.trim().to_string();
    create_metarecord(&repo, &["note:string=second"]);
    assert_ok(&mf(&["-u", &repo, "repo", "backup"]));
    create_metarecord(&repo, &["note:string=third"]);

    let out = mf(&["-u", &repo, "repo", "restore", "--from", &named]);
    assert_ok(&out);
    assert!(out.stdout.contains(&format!("restored from {named}")), "{}", out.stdout);
    let out = mf(&["-u", &repo, "metarecord", "-q", "note IS PRESENT", "get"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), first, "the named backup, not the newest one");

    let metafolder = root.join(".metafolder");
    let out = mf(&["repo", "restore", "--metafolder", metafolder.to_str().unwrap()]);
    assert_ok(&out);
    assert!(out.stdout.contains("restored from"), "{}", out.stdout);
    assert!(out.stderr.contains("mf reconcile"), "{}", out.stderr);
}

/// `mf duplicate clear` without `-y`: nothing to clear is said, not asked; a
/// declined prompt leaves the groups in place.
#[test]
fn test_duplicate_clear_asks_and_says_when_there_is_nothing() {
    let (repo, _dir) =
        tracked_repo("dupclear_ask", &[("a.txt", b"same bytes"), ("b.txt", b"same bytes")]);
    let out = mf(&["-u", &repo, "duplicate", "clear"]);
    assert_ok(&out);
    assert!(out.stdout.contains("No duplicate groups to clear."), "{}", out.stdout);

    assert_ok(&mf(&["-u", &repo, "duplicate", "scan"]));
    let out = mf_full(&["-u", &repo, "duplicate", "clear"], Some("n\n"), &[], true);
    assert_ok(&out);
    assert!(out.stdout.contains("Aborted."), "{}", out.stdout);
    let linked = mf(&["-u", &repo, "metarecord", "-q", "mfr_duplicate_group IS PRESENT", "get"]);
    assert_eq!(linked.stdout.split_whitespace().count(), 2, "kept: {}", linked.stdout);
}

/// `--json` on the task commands prints the daemon's body as it is; a
/// repository with no task says so.
#[test]
fn test_task_commands_json_and_no_tasks() {
    let (repo, root) = init_repo("task_json");
    let out = mf(&["-u", &repo, "task", "list"]);
    assert_ok(&out);
    assert_eq!(out.stdout, "no tasks\n");

    std::fs::write(root.join("a.txt"), b"a").unwrap();
    assert_ok(&mf(&["-u", &repo, "reconcile"]));
    let out = mf(&["-u", &repo, "task", "list", "--json"]);
    assert_ok(&out);
    let listed: serde_json::Value = serde_json::from_str(out.stdout.trim()).unwrap();
    let id = listed[0]["id"].as_str().unwrap_or_else(|| panic!("{listed}")).to_string();
    let out = mf(&["-u", &repo, "task", "show", &id, "--json"]);
    assert_ok(&out);
    let shown: serde_json::Value = serde_json::from_str(out.stdout.trim()).unwrap();
    assert_eq!(shown["id"], id.as_str(), "{shown}");
}

/// `mf duplicate scan --no-wait` prints the task id alone; `--json` prints the
/// result's body; `--metarecord` must be a UUID (a usage error, before any request).
#[test]
fn test_duplicate_scan_no_wait_json_and_entry() {
    let (repo, _dir) =
        tracked_repo("dupscan_flags", &[("a.txt", b"same bytes"), ("b.txt", b"same bytes")]);
    let out = mf(&["-u", &repo, "duplicate", "scan", "--no-wait"]);
    assert_ok(&out);
    let task = out.stdout.trim().to_string();
    assert!(is_hex_uuid(&task), "a task id: {}", out.stdout);
    // One scan at a time per repository: the next waits for this one.
    poll_mf(&["-u", &repo, "task", "show", &task], |s| s.contains(" done"));

    let out = mf(&["-u", &repo, "duplicate", "scan", "--json"]);
    assert_ok(&out);
    let body: serde_json::Value = serde_json::from_str(out.stdout.trim()).unwrap();
    assert!(body.is_object(), "{}", out.stdout);

    let out = mf(&["-u", &repo, "duplicate", "scan", "--metarecord", "not-a-uuid"]);
    assert_eq!(out.code, 2, "{}", out.stderr);
    assert!(out.stderr.contains("invalid metarecord UUID"), "{}", out.stderr);
}

/// A stub daemon whose duplicate scan ends in `status`: a failed or cancelled
/// task cannot be had on demand from the real one. Returns its port.
fn stub_task_daemon(status: &'static str) -> String {
    use axum::routing::{get, post};
    let app = axum::Router::new()
        .route(
            "/repos/{repo}/duplicates/scan",
            post(|| async { axum::Json(serde_json::json!({"task_id": "t1"})) }),
        )
        .route(
            "/repos/{repo}/tasks/{id}",
            get(move || async move {
                let mut task = serde_json::json!({"id": "t1", "status": status});
                if status == "failed" {
                    task["error"] = "disk on fire".into();
                }
                axum::Json(task)
            }),
        );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            axum::serve(listener, app).await.unwrap();
        });
    });
    port.to_string()
}

/// A task that ends failed reports the daemon's error; a cancelled one says so;
/// both exit 1.
#[test]
fn test_a_failed_or_cancelled_task_is_an_error() {
    let repo = Uuid::new_v4().as_simple().to_string();
    let port = stub_task_daemon("failed");
    let out = mf_full(&["-p", &port, "-u", &repo, "duplicate", "scan"], None, &[], false);
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(out.stderr.contains("disk on fire"), "{}", out.stderr);

    let port = stub_task_daemon("cancelled");
    let out = mf_full(&["-p", &port, "-u", &repo, "duplicate", "scan"], None, &[], false);
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(out.stderr.contains("duplicates: cancelled"), "{}", out.stderr);
}

/// `--tsv` with `-i`: one row for the record (first value of each selected
/// field); `--select '*'` names no field list, a usage error.
#[test]
fn test_metarecord_get_tsv_with_an_id_selector() {
    let (repo, _root) = init_repo("tsv_id");
    let uuid = create_metarecord(&repo, &["name:string=Coltrane", "lead:bool=true"]);
    let out =
        mf(&["-u", &repo, "metarecord", "-i", &uuid, "get", "--select", "lead,name,x", "--tsv"]);
    assert_ok(&out);
    assert_eq!(out.stdout, "true\tColtrane\t\n");

    let out = mf(&["-u", &repo, "metarecord", "-i", &uuid, "get", "--select", "*", "--tsv"]);
    assert_eq!(out.code, 2, "{}", out.stderr);
    assert!(out.stderr.contains("--tsv requires --select"), "{}", out.stderr);
}

/// `mf order --dry-run` prints the planned positions and writes none of them.
#[test]
fn test_order_dry_run_writes_nothing() {
    let (repo, root) = init_repo("order_dry");
    let album = root.join("album");
    std::fs::create_dir_all(&album).unwrap();
    for f in ["song1.avi", "song2.avi"] {
        std::fs::write(album.join(f), b"x").unwrap();
    }
    let track = |p: std::path::PathBuf| {
        let out = mf(&["-u", &repo, "metarecord", "add", "--path", p.to_str().unwrap()]);
        assert_ok(&out);
        out.stdout.trim().to_string()
    };
    let album_uuid = track(album.clone());
    let song1 = track(album.join("song1.avi"));
    track(album.join("song2.avi"));

    let out = mf(&["-u", &repo, "order", album.to_str().unwrap(), "--dry-run"]);
    assert_ok(&out);
    let lines: Vec<&str> = out.stdout.lines().collect();
    assert_eq!(lines.len(), 2, "{}", out.stdout);
    let first: Vec<&str> = lines[0].split('\t').collect();
    assert_eq!(first[1], "order_file=1", "{}", out.stdout);
    assert_eq!(first[2], "song1.avi", "{}", out.stdout);

    for (uuid, field) in [(&song1, "order_file"), (&album_uuid, "order_numbered")] {
        let out = mf(&["-u", &repo, "metarecord", "-i", uuid, "field", "get", field]);
        assert!(out.stdout.trim().is_empty(), "the dry run wrote {field}: {}", out.stdout);
    }

    // A folder without a metarecord has no children to number.
    let loose = root.join("loose");
    std::fs::create_dir(&loose).unwrap();
    let out = mf(&["-u", &repo, "order", loose.to_str().unwrap()]);
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(out.stderr.contains("no metarecord"), "{}", out.stderr);
}

/// `mf ignore -d` on a directory with no metarecord is an error naming it; a
/// `set` with no preset empties `mf_ignore`, which `list -d` then shows.
#[test]
fn test_ignore_untracked_dir_and_emptied_patterns() {
    let (repo, root) = init_repo("ignore_dir");
    let dir = root.join("sub");
    std::fs::create_dir(&dir).unwrap();
    let out = mf_cfg(&["-u", &repo, "ignore", "add", "node", "-d", dir.to_str().unwrap()]);
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(out.stderr.contains("no metarecord is associated with"), "{}", out.stderr);

    assert_ok(&mf(&["-u", &repo, "metarecord", "add", "--path", dir.to_str().unwrap()]));
    let out = mf_cfg(&["-u", &repo, "ignore", "set", "node", "-d", dir.to_str().unwrap()]);
    assert_ok(&out);
    assert!(out.stdout.contains("node_modules"), "{}", out.stdout);

    let out = mf_cfg(&["-u", &repo, "ignore", "set", "-d", dir.to_str().unwrap()]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "mf_ignore is now empty");

    let out = mf_cfg(&["-u", &repo, "ignore", "list", "-d", dir.to_str().unwrap()]);
    assert_ok(&out);
    assert!(out.stdout.contains("Active patterns on the target:\n  (none)"), "{}", out.stdout);

    // Only `set` may be given no preset.
    let out = mf_cfg(&["-u", &repo, "ignore", "add", "-d", dir.to_str().unwrap()]);
    assert_eq!(out.code, 2, "{}", out.stderr);
}

/// A stub daemon whose `GET /watch` answers `body`: a starved budget, or a
/// platform without inotify, cannot be had on demand. Returns its port.
fn stub_watch_daemon(body: serde_json::Value) -> String {
    use axum::routing::get;
    let app = axum::Router::new()
        .route("/repos/{repo}/watch", get(move || async move { axum::Json(body) }));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            axum::serve(listener, app).await.unwrap();
        });
    });
    port.to_string()
}

/// `watch status` adds the budget lines when the budget is in trouble: a
/// starved one says another program holds the watches; one without a cap
/// (no inotify) is "unlimited", its excluded subtrees still counted.
#[test]
fn test_watch_status_reports_a_starved_or_unlimited_budget() {
    let repo = Uuid::new_v4().as_simple().to_string();
    let port = stub_watch_daemon(serde_json::json!({
        "paused": false, "pending_events": 0, "backend": "inotify", "watched_dirs": 7,
        "watch_budget": {"cap": 100, "limit": 1000, "share": 10, "exceeded_dirs": 0,
                         "starved": true},
    }));
    let out = mf_full(&["-p", &port, "-u", &repo, "watch", "status"], None, &[], false);
    assert_ok(&out);
    assert!(out.stdout.contains("7 of 100 watch(es) used (share 10% of 1000)"), "{}", out.stdout);
    assert!(out.stdout.contains("STARVED"), "{}", out.stdout);
    assert!(!out.stdout.contains("excluded"), "{}", out.stdout);

    let port = stub_watch_daemon(serde_json::json!({
        "paused": false, "pending_events": 0, "backend": "inotify", "watched_dirs": 7,
        "watch_budget": {"exceeded_dirs": 2, "starved": false},
    }));
    let out = mf_full(&["-p", &port, "-u", &repo, "watch", "status"], None, &[], false);
    assert_ok(&out);
    assert!(out.stdout.contains("unlimited on this platform"), "{}", out.stdout);
    assert!(out.stdout.contains("2 subtree(s) left unwatched"), "{}", out.stdout);
    assert!(!out.stdout.contains("STARVED"), "{}", out.stdout);
}

/// `mf script` needs no daemon: it lists the scripts directory of the config
/// and copies a template into it (doc "mf script").
#[test]
fn script_lists_the_scripts_and_copies_a_template() {
    let cfg = common::TempDir::new("cli-script");
    let scripts = cfg.path().join("metafolder").join("scripts");
    std::fs::create_dir_all(scripts.join("templates")).unwrap();
    let shipped = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts");
    std::fs::copy(
        format!("{shipped}/shipped/gui-tag-folder.sh"),
        scripts.join("gui-tag-folder.sh"),
    )
    .unwrap();
    std::fs::copy(
        format!("{shipped}/templates/sort-folder.sh"),
        scripts.join("templates/sort-folder.sh"),
    )
    .unwrap();
    install_cli_config(cfg.path());
    let xdg = cfg.path().to_str().unwrap();
    let env = [("XDG_CONFIG_HOME", xdg)];

    let out = mf_full(&["script"], None, &env, false);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stdout.starts_with(scripts.to_str().unwrap()), "{}", out.stdout);
    assert!(out.stdout.contains("gui-tag-folder.sh"), "{}", out.stdout);
    assert!(out.stdout.contains("sort-folder"), "{}", out.stdout);

    // Not on a terminal: naming the template is required.
    let out = mf_full(&["script", "new"], None, &env, false);
    assert_eq!(out.code, 2, "{}", out.stderr);
    assert!(out.stderr.contains("sort-folder"), "{}", out.stderr);

    let out = mf_full(&["script", "new", "sort-folder"], None, &env, false);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.stdout.trim(), scripts.join("sort-folder.sh").to_str().unwrap());
    // Copied in, it is offered to run like any script.
    let out = mf_full(&["script"], None, &env, false);
    assert!(out.stdout.contains("sort-folder.sh"), "{}", out.stdout);

    let out = mf_full(&["script", "new", "sort-folder"], None, &env, false);
    assert_eq!(out.code, 1, "a second copy must not overwrite the first");
}
