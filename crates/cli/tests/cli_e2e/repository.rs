use super::*;

// ── Repository commands ───────────────────────────────────────────────────────

#[test]
fn test_init_prints_uuid_and_creates_metafolder() {
    let (_, root) = init_repo("init");
    assert!(root.join(".metafolder").join("config.json").exists());
}

#[test]
fn test_init_installs_the_configuration_as_a_git_repository() {
    let (_, root) = init_repo("init_config");
    let config = root.join(".metafolder/config");
    assert!(config.join("metadata-map.toml").is_file());
    assert!(config.join(".git").is_dir());
    // Nothing new since: up to date.
    let out = mf_cfg(&["repo", "update-config", root.join(".metafolder").to_str().unwrap()]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "configuration already up to date");
}

#[test]
fn test_update_config_merges_new_defaults_into_the_repository_edits() {
    let (repo, root) = init_repo("update_config");
    // This test's own defaults: the shared ones, plus a schema.
    let xdg = temp_dir("update_config_xdg");
    install_cli_config(&xdg);
    let defaults = xdg.join("metafolder/daemon/repo-default");
    std::fs::create_dir_all(&defaults).unwrap();
    let map = root.join(".metafolder/config/metadata-map.toml");
    std::fs::copy(&map, defaults.join("metadata-map.toml")).unwrap();
    // The repository edits its map (a line at its end).
    let mine = format!("{}\n# mine\n", std::fs::read_to_string(&map).unwrap());
    std::fs::write(&map, &mine).unwrap();
    std::fs::write(defaults.join("schema.json"), FILM_SCHEMA).unwrap();

    // No daemon involved: the files alone, by the root.
    let out = mf_full(
        &["repo", "update-config", root.to_str().unwrap()],
        None,
        &[("XDG_CONFIG_HOME", xdg.to_str().unwrap())],
        false,
    );
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "configuration updated");
    assert_eq!(std::fs::read_to_string(&map).unwrap(), mine);
    let schema = root.join(".metafolder/config/schema.json");
    assert_eq!(std::fs::read_to_string(schema).unwrap(), FILM_SCHEMA);
    // The loaded repository reads it when asked.
    assert_ok(&mf(&["-u", &repo, "schema", "reload"]));
}

#[test]
fn test_update_config_moves_an_old_layout_into_config() {
    let (repo, root) = init_repo("update_config_old");
    assert_ok(&mf(&["-u", &repo, "repo", "unload"]));
    // A repository from before `.metafolder/config/`.
    let metafolder = root.join(".metafolder");
    std::fs::rename(
        metafolder.join("config/metadata-map.toml"),
        metafolder.join("metadata-map.toml"),
    )
    .unwrap();
    std::fs::remove_dir_all(metafolder.join("config")).unwrap();
    let out = mf(&["repo", "load", root.to_str().unwrap()]);
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(out.stderr.contains("mf repo update-config"), "{}", out.stderr);

    let out = mf_cfg(&["repo", "update-config", metafolder.to_str().unwrap()]);
    assert_ok(&out);
    assert!(out.stdout.contains("moved metadata-map.toml into config/"), "{}", out.stdout);
    assert!(metafolder.join("config/.git").is_dir());
    assert!(!metafolder.join("metadata-map.toml").exists());
    assert_ok(&mf(&["repo", "load", root.to_str().unwrap()]));
}

#[test]
fn test_update_config_names_a_path_that_holds_no_repository() {
    let dir = temp_dir("update_config_none");
    let out = mf_cfg(&["repo", "update-config", dir.to_str().unwrap()]);
    assert_eq!(out.code, 2, "{}", out.stderr);
    assert!(out.stderr.contains("no repository"), "{}", out.stderr);
}

#[test]
fn test_init_with_external_metafolder() {
    let root = temp_dir("init_ext_root");
    let external = temp_dir("init_ext_db");
    let out = mf_cfg(&[
        "repo",
        "init",
        root.to_str().unwrap(),
        "--metafolder",
        external.to_str().unwrap(),
    ]);
    assert_ok(&out);
    assert!(is_hex_uuid(out.stdout.trim()));
    assert!(external.join("config.json").exists());
    assert!(!root.join(".metafolder").exists());
}

/// The name `mf repo list` gives the repository `uuid`.
fn listed_name(uuid: &str) -> Option<String> {
    let out = mf(&["repo", "list"]);
    let repos: serde_json::Value = serde_json::from_str(&out.stdout).expect("JSON");
    let repo = repos.as_array()?.iter().find(|r| r["repo_uuid"] == uuid)?;
    Some(repo["name"].as_str()?.to_string())
}

#[test]
fn test_init_takes_a_name() {
    let root = temp_dir("init_name");
    let name = format!("named-{}", root.file_name().unwrap().to_str().unwrap());
    let out = mf_cfg(&["repo", "init", root.to_str().unwrap(), "--name", &name]);
    assert_ok(&out);
    assert_eq!(listed_name(out.stdout.trim()), Some(name));
}

#[test]
fn test_rename_changes_the_repository_name() {
    let (repo, root) = init_repo("rename");
    let name = format!("renamed-{}", root.file_name().unwrap().to_str().unwrap());
    let out = mf(&["-u", &repo, "repo", "rename", &name]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), repo);
    assert_eq!(listed_name(&repo), Some(name.clone()));
    // The new name selects it.
    assert_ok(&mf(&["-n", &name, "repo", "check"]));

    // A name another loaded repository holds is refused.
    let (other, _other_root) = init_repo("rename_taken");
    let out = mf(&["-u", &other, "repo", "rename", &name]);
    assert_eq!(out.code, 1, "stderr: {}", out.stderr);
    assert!(out.stderr.contains("error:"), "stderr: {}", out.stderr);
}

#[test]
fn test_rename_requires_repo() {
    let out = mf(&["repo", "rename", "x"]);
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
}

#[test]
fn test_load_root_is_idempotent() {
    let (repo, root) = init_repo("load");
    let out = mf(&["repo", "load", root.to_str().unwrap()]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), repo);
}

#[test]
fn test_load_with_metafolder_flag() {
    let root = temp_dir("load_ext_root");
    let external = temp_dir("load_ext_db");
    let out = mf_cfg(&[
        "repo",
        "init",
        root.to_str().unwrap(),
        "--metafolder",
        external.to_str().unwrap(),
    ]);
    assert_ok(&out);
    let repo = out.stdout.trim().to_string();
    let out = mf(&["repo", "load", "--metafolder", external.to_str().unwrap()]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), repo);
}

#[test]
fn test_load_waits_for_warmup_silently_when_stderr_is_piped() {
    // The default load waits for the warmup task; the progress bar is only
    // drawn on a terminal, so a piped stderr stays clean (doc
    // "mf repo load").
    let (repo, root) = init_repo("load_wait");
    let out = mf(&["-u", &repo, "repo", "unload"]);
    assert_ok(&out);
    let out = mf(&["repo", "load", root.to_str().unwrap()]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), repo);
    assert_eq!(out.stderr, "", "no progress noise when stderr is piped");
}

#[test]
fn test_load_no_wait_prints_uuid_immediately() {
    let (repo, root) = init_repo("load_nowait");
    let out = mf(&["-u", &repo, "repo", "unload"]);
    assert_ok(&out);
    let out = mf(&["repo", "load", "--no-wait", root.to_str().unwrap()]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), repo);
}

#[test]
fn test_load_in_memory_holds_the_store_in_memory() {
    // doc "Holding a repository in memory": the listing says what is held.
    let (repo, root) = init_repo("load_mem");
    let out = mf(&["-u", &repo, "repo", "unload"]);
    assert_ok(&out);
    let out = mf(&["repo", "load", "--in-memory", root.to_str().unwrap()]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), repo);
    let out = mf(&["repo", "list"]);
    assert_ok(&out);
    let listed: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    let ours = listed.as_array().unwrap().iter().find(|r| r["repo_uuid"] == repo.as_str()).unwrap();
    assert!(ours["in_memory"]["bytes"].as_u64().unwrap() > 0, "{ours}");
}

/// `mf -n NAME repo load` loads a repository the daemon's configuration
/// knows (doc "Known repositories"); `mf repo known` lists them.
#[test]
fn test_a_known_repository_is_loaded_by_its_name() {
    let (repo, root) = init_repo("known");
    let name = root.file_name().unwrap().to_str().unwrap().to_string();
    assert_ok(&mf(&["-u", &repo, "repo", "unload"]));
    std::fs::write(
        daemon_config_file(),
        format!("{SHIPPED_DAEMON_CONFIG}\n[[known]]\nroot = {:?}\n", root.to_path_buf()),
    )
    .unwrap();

    let out = mf(&["repo", "known"]);
    assert_ok(&out);
    let known: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(known[0]["name"], name.as_str(), "{known}");
    assert_eq!(known[0]["loaded"], false, "{known}");

    let out = mf(&["-n", &name, "repo", "load"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), repo);

    let out = mf(&["-n", "no-such-repository", "repo", "load"]);
    assert_eq!(out.code, 1, "stderr: {}", out.stderr);
    assert!(out.stderr.contains("no-such-repository"), "{}", out.stderr);
}

#[test]
fn test_load_requires_exactly_one_locator() {
    let out = mf(&["repo", "load"]);
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
    let root = temp_dir("load_both");
    let out = mf(&["repo", "load", root.to_str().unwrap(), "--metafolder", root.to_str().unwrap()]);
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
}

#[test]
fn test_repos_lists_loaded_repositories() {
    let (repo, _root) = init_repo("repos");
    let out = mf(&["repo", "list"]);
    assert_ok(&out);
    let parsed: serde_json::Value = serde_json::from_str(&out.stdout).expect("pretty JSON");
    assert!(out.stdout.contains(&repo), "repos output should mention {repo}");
    assert!(parsed.is_array() || parsed.is_object());
}

#[test]
fn test_a_lost_repository_is_listed_as_lost_and_explained() {
    // Its root moved away (doc "When the root moves"): unloaded by the
    // daemon, still listed so the user can tell why it answers nothing.
    let (repo, root) = init_repo("lostroot");
    let elsewhere = temp_dir("lostroot_elsewhere");
    let moved = elsewhere.join("repo");
    std::fs::rename(&root, &moved).unwrap();
    let out = mf(&["-u", &repo, "metarecord", "get"]);
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(out.stderr.contains("mf repo load"), "says how to resume: {}", out.stderr);

    let mut entry = serde_json::Value::Null;
    for _ in 0..100 {
        let listed: serde_json::Value =
            serde_json::from_str(&mf(&["repo", "list"]).stdout).unwrap();
        entry = listed
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["repo_uuid"] == repo.as_str())
            .cloned()
            .unwrap_or_default();
        if entry["lost"] == true {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(entry["lost"], true, "{entry}");

    // Loaded again where it is now: the same repository, no longer lost.
    let out = mf(&["repo", "load", moved.to_str().unwrap()]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), repo);
    let listed: serde_json::Value = serde_json::from_str(&mf(&["repo", "list"]).stdout).unwrap();
    let entry =
        listed.as_array().unwrap().iter().find(|r| r["repo_uuid"] == repo.as_str()).unwrap();
    assert_eq!(entry["lost"], false, "{entry}");
    assert_ok(&mf(&["-u", &repo, "repo", "unload"]));
}

#[test]
fn test_unload_removes_repo_and_allows_reload() {
    let (repo, root) = init_repo("unload");

    // Loaded: it appears in the list.
    assert!(mf(&["repo", "list"]).stdout.contains(&repo));

    // Unload prints the uuid and removes it from the list.
    let out = mf(&["-u", &repo, "repo", "unload"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), repo);
    assert!(!mf(&["repo", "list"]).stdout.contains(&repo), "still listed after unload");

    // Unloading again fails (no longer loaded).
    let out = mf(&["-u", &repo, "repo", "unload"]);
    assert_eq!(out.code, 1, "second unload should fail; stderr: {}", out.stderr);

    // The lock was released: the same root loads again with the same uuid.
    let out = mf(&["repo", "load", root.to_str().unwrap()]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), repo);
}

#[test]
fn test_unload_requires_repo() {
    // Repo-scoped: missing --repo is a usage error (exit 2), no daemon round-trip.
    let out = mf(&["repo", "unload"]);
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
}

// ── Global options and exit codes ─────────────────────────────────────────────

#[test]
fn test_missing_repo_is_usage_error_without_contacting_daemon() {
    // Unreachable daemon URL: exit code 2 proves no HTTP round-trip happened.
    let out = mf_full(&["-p", "1", "metarecord", "get"], None, &[], false);
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
}

#[test]
fn test_invalid_repo_uuid_is_usage_error() {
    let out = mf(&["-u", "not-a-uuid", "metarecord", "get"]);
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
}

/// `-i` names one metarecord: text that is not a UUID is a usage error, never
/// run as a query (that is `-q`'s job) — and never a panic.
#[test]
fn test_metarecord_id_selector_must_be_a_uuid() {
    let (repo, _root) = init_repo("id_not_uuid");
    create_metarecord(&repo, &["genre:string=idq"]);
    let dsl = r#"genre = "idq""#;
    for verb in [
        &["get"][..],
        &["get", "--select", "genre", "--tsv"],
        &["field", "get", "genre"],
        &["field", "set", "genre:string=x"],
        &["delete", "--force"],
    ] {
        let mut args = vec!["-u", &repo, "metarecord", "-i", dsl];
        args.extend_from_slice(verb);
        let out = mf(&args);
        assert_eq!(out.code, 2, "{verb:?}: stdout: {}\nstderr: {}", out.stdout, out.stderr);
        assert!(out.stderr.contains("-i takes a metarecord UUID"), "{verb:?}: {}", out.stderr);
    }
    let out = mf(&["-u", &repo, "metarecord", "-q", dsl, "get"]);
    assert_eq!(out.stdout.lines().count(), 1, "nothing was written or deleted");
    let out = mf(&["-u", &repo, "metarecord", "-q", dsl, "field", "get", "genre"]);
    assert_eq!(out.stdout.trim(), "idq");
}

#[test]
fn test_unreachable_daemon_is_operation_error() {
    let out = mf_full(&["-p", "1", "repo", "list"], None, &[], false);
    assert_eq!(out.code, 1, "stderr: {}", out.stderr);
    assert!(out.stderr.starts_with("error:"), "stderr: {}", out.stderr);
}

// ── CLI config file (doc "CLI configuration") ───────────────────────────

/// A fresh `XDG_CONFIG_HOME` holding the shipped `metafolder/cli/config.toml`
/// with `repo` as its `[repo]` table. The guard comes back with the path: dropping it would take the config dir
/// away before the command under test reads it.
fn xdg_with_cli_config(repo: &str) -> (TempDir, String) {
    let dir = temp_dir("cli_cfg");
    let cli = dir.join("metafolder").join("cli");
    std::fs::create_dir_all(&cli).unwrap();
    let contents = SHIPPED_CLI_CONFIG.replace("[repo]\n", &format!("[repo]\n{repo}\n"));
    std::fs::write(cli.join("config.toml"), contents).unwrap();
    let path = dir.to_str().unwrap().to_string();
    (dir, path)
}

#[test]
fn test_config_default_repo_used_when_no_selector() {
    let (uuid, _root) = init_repo("cfgrepo");
    let (_cfg, xdg) = xdg_with_cli_config(&format!("uuid = \"{uuid}\""));
    // No -u/-n: the selector comes from the config's default [repo].
    let out = mf_full(&["metarecord", "get"], None, &[("XDG_CONFIG_HOME", &xdg)], true);
    assert_ok(&out);
}

/// `--config` reads that file instead of the user's: a script is immune to
/// your settings. The shipped file names no repository → usage error (exit 2).
#[test]
fn test_an_explicit_config_replaces_the_default_repo() {
    let (uuid, _root) = init_repo("cfgrepo_explicit");
    let (_cfg, xdg) = xdg_with_cli_config(&format!("uuid = \"{uuid}\""));
    let shipped = concat!(env!("CARGO_MANIFEST_DIR"), "/default-config/config.toml");
    let out = mf_full(
        &["--config", shipped, "metarecord", "get"],
        None,
        &[("XDG_CONFIG_HOME", &xdg)],
        true,
    );
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
}

/// No built-in copy of the defaults: a missing file stops `mf`, naming it.
#[test]
fn test_a_missing_config_is_an_error_naming_it() {
    let empty = temp_dir("cli_cfg_missing");
    let out =
        mf_full(&["repo", "list"], None, &[("XDG_CONFIG_HOME", empty.to_str().unwrap())], false);
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
    assert!(out.stderr.contains("cli/config.toml"), "stderr: {}", out.stderr);
    assert!(out.stderr.contains("metafolder-sync-config"), "stderr: {}", out.stderr);
}

#[test]
fn test_explicit_selector_overrides_the_config_default_repo() {
    let (uuid, _root) = init_repo("cfgrepo_override");
    // The config points at a bogus repo; an explicit -u must still win.
    let (_cfg, xdg) = xdg_with_cli_config("name = \"does-not-exist\"");
    let out =
        mf_full(&["-u", &uuid, "metarecord", "get"], None, &[("XDG_CONFIG_HOME", &xdg)], true);
    assert_ok(&out);
}

#[test]
fn test_malformed_config_is_usage_error_without_contacting_daemon() {
    let (_cfg, xdg) = xdg_with_cli_config("this is = not = valid toml");
    // Exit 2 before any round-trip, even against an unreachable daemon.
    let out = mf_full(&["-p", "1", "repo", "list"], None, &[("XDG_CONFIG_HOME", &xdg)], false);
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
    assert!(out.stderr.contains("config.toml"), "stderr: {}", out.stderr);
}

#[test]
fn test_env_variables_are_honoured() {
    let (repo, _root) = init_repo("env");
    let out = mf_full(
        &["metarecord", "get"],
        None,
        &[("METAFOLDER_DAEMON_PORT", daemon_port()), ("METAFOLDER_REPO", repo.as_str())],
        false,
    );
    assert_ok(&out);
    assert!(!out.stdout.trim().is_empty());
}

#[test]
fn test_daemon_error_goes_to_stderr() {
    let (repo, _root) = init_repo("daemon_err");
    let missing = "00000000000000000000000000000099";
    let out = mf(&["-u", &repo, "metarecord", "-i", missing, "get"]);
    assert_eq!(out.code, 1);
    assert!(out.stderr.starts_with("error:"), "stderr: {}", out.stderr);
    assert!(out.stdout.is_empty());
}

/// `mf repo borrow` copies a repository named by its (mounted) root to a new
/// directory and loads the copy, printing its uuid; `mf repo give-back` sends
/// it home and prints where. The repository then loads at home again, with
/// what was written in the copy.
#[test]
fn test_repo_borrow_and_give_back() {
    let (repo, root) = init_repo("borrow");
    create_metarecord(&repo, &["note:string=home"]);
    assert_ok(&mf(&["-u", &repo, "repo", "unload"]));
    let here = temp_dir("borrow_here");
    let to = here.join("copy");
    let out = mf(&[
        "repo",
        "borrow",
        root.to_str().unwrap(),
        "--to",
        to.to_str().unwrap(),
        "--name",
        "borrowed-copy",
    ]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), repo, "prints the uuid");
    assert!(out.stderr.contains("mf repo give-back"), "{}", out.stderr);
    create_metarecord(&repo, &["note:string=away"]);

    let out = mf(&["-n", "borrowed-copy", "repo", "give-back"]);
    assert_ok(&out);
    assert!(out.stdout.contains(".metafolder"), "{}", out.stdout);
    assert!(!to.exists(), "the copy is gone");
    assert_ok(&mf(&["repo", "load", root.to_str().unwrap()]));
    let out = mf(&["-u", &repo, "metarecord", "-q", "note IS PRESENT", "get"]);
    assert_ok(&out);
    assert_eq!(out.stdout.lines().count(), 2, "{}", out.stdout);
}
