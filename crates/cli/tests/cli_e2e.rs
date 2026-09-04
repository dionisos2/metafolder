//! End-to-end tests: run the real `mf` binary against an in-process daemon
//! listening on an ephemeral port.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use uuid::Uuid;

// ── Harness ───────────────────────────────────────────────────────────────────

static DAEMON_URL: OnceLock<String> = OnceLock::new();

/// Starts one shared daemon for the whole test binary. The listener is bound
/// before the server thread starts, so connections are queued (no race).
fn daemon_url() -> &'static str {
    DAEMON_URL.get_or_init(|| {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                // Expansion is client-side now: the daemon needs no grammar.
                let app_state = metafolder_daemon::state::AppState::new();
                let app = metafolder_daemon::routes::build(std::sync::Arc::new(app_state));
                axum::serve(listener, app).await.unwrap();
            });
        });
        format!("http://127.0.0.1:{}", addr.port())
    })
}

/// The shared daemon's port, as a string (the CLI addresses it with `-p`).
fn daemon_port() -> &'static str {
    static PORT: OnceLock<String> = OnceLock::new();
    PORT.get_or_init(|| daemon_url().rsplit(':').next().unwrap().to_string())
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

fn mf_full(args: &[&str], stdin: Option<&str>, envs: &[(&str, &str)], daemon: bool) -> Out {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mf"));
    if daemon {
        cmd.arg("-p").arg(daemon_port());
    }
    cmd.args(args);
    cmd.env_remove("METAFOLDER_REPO")
        .env_remove("METAFOLDER_REPO_NAME")
        .env_remove("METAFOLDER_DAEMON_PORT");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
    let mut child = cmd.spawn().unwrap();
    if let Some(input) = stdin {
        child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    }
    let output = child.wait_with_output().unwrap();
    Out {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn mf(args: &[&str]) -> Out {
    mf_full(args, None, &[], true)
}

/// `XDG_CONFIG_HOME` for a config dir with the shipped `core/` config
/// installed: the query-grammar (so `mf query --simplified` can expand locally)
/// and the ignore-presets (so `mf repo init` / `mf ignore` can apply the
/// `default` preset — the daemon no longer ships ignore patterns).
fn config_xdg() -> &'static str {
    use std::sync::OnceLock;
    static XDG: OnceLock<String> = OnceLock::new();
    XDG.get_or_init(|| {
        let dir = common::tests_root().join(format!("cli-cfg-{}", std::process::id()));
        let core = dir.join("metafolder").join("core");
        std::fs::create_dir_all(&core).unwrap();
        std::fs::copy(
            concat!(env!("CARGO_MANIFEST_DIR"), "/../core/default-config/query-grammar"),
            core.join("query-grammar"),
        )
        .unwrap();
        std::fs::copy(
            concat!(env!("CARGO_MANIFEST_DIR"), "/../core/default-config/ignore-presets.toml"),
            core.join("ignore-presets.toml"),
        )
        .unwrap();
        dir.to_str().unwrap().to_string()
    })
}

/// Like `mf`, but with a config dir holding the grammar (for `--simplified`).
fn mf_cfg(args: &[&str]) -> Out {
    mf_full(args, None, &[("XDG_CONFIG_HOME", config_xdg())], true)
}

fn assert_ok(out: &Out) {
    assert_eq!(out.code, 0, "expected success.\nstdout: {}\nstderr: {}", out.stdout, out.stderr);
}

fn is_hex_uuid(s: &str) -> bool {
    s.len() == 32 && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn temp_dir(prefix: &str) -> TempDir {
    TempDir::new(&format!("cli_{prefix}"))
}

/// Initialises a fresh repository; returns (repo uuid, root path). Runs with the
/// hermetic config dir so `mf repo init` applies the `default` ignore preset
/// (the daemon no longer writes default ignores itself).
fn init_repo(prefix: &str) -> (String, TempDir) {
    let root = temp_dir(prefix);
    let out = mf_cfg(&["repo", "init", root.to_str().unwrap()]);
    assert_ok(&out);
    let uuid = out.stdout.trim().to_string();
    assert!(is_hex_uuid(&uuid), "init should print a 32-hex uuid, got: '{uuid}'");
    (uuid, root)
}

/// Creates an entry from field specs; returns its UUID.
fn create_metarecord(repo: &str, specs: &[&str]) -> String {
    let mut args = vec!["-u", repo, "metarecord", "add"];
    args.extend_from_slice(specs);
    let out = mf(&args);
    assert_ok(&out);
    let uuid = out.stdout.trim().to_string();
    assert!(is_hex_uuid(&uuid), "create should print a 32-hex uuid, got: '{uuid}'");
    uuid
}

fn get_entries(repo: &str, target: &str) -> serde_json::Value {
    // -i for a uuid selector (one object), -q for a query (the matching array);
    // `--select '*'` yields the full JSON objects in both cases.
    let flag = if is_hex_uuid(target) { "-i" } else { "-q" };
    let out = mf(&["-u", repo, "metarecord", flag, target, "get", "--select", "*"]);
    assert_ok(&out);
    serde_json::from_str(&out.stdout).expect("mf get should print JSON")
}

// ── Repository commands ───────────────────────────────────────────────────────

#[test]
fn test_init_prints_uuid_and_creates_metafolder() {
    let (_, root) = init_repo("init");
    assert!(root.join(".metafolder").join("config.json").exists());
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
    // drawn on a terminal, so a piped stderr stays clean (spec-main
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

#[test]
fn test_unreachable_daemon_is_operation_error() {
    let out = mf_full(&["-p", "1", "repo", "list"], None, &[], false);
    assert_eq!(out.code, 1, "stderr: {}", out.stderr);
    assert!(out.stderr.starts_with("error:"), "stderr: {}", out.stderr);
}

// ── CLI config file (spec-config "cli/config.toml") ───────────────────────────

/// A fresh `XDG_CONFIG_HOME` holding `metafolder/cli/config.toml` = `contents`.
/// The guard comes back with the path: dropping it would take the config dir
/// away before the command under test reads it.
fn xdg_with_cli_config(contents: &str) -> (TempDir, String) {
    let dir = temp_dir("cli_cfg");
    let cli = dir.join("metafolder").join("cli");
    std::fs::create_dir_all(&cli).unwrap();
    std::fs::write(cli.join("config.toml"), contents).unwrap();
    let path = dir.to_str().unwrap().to_string();
    (dir, path)
}

#[test]
fn test_config_default_repo_used_when_no_selector() {
    let (uuid, _root) = init_repo("cfgrepo");
    let (_cfg, xdg) = xdg_with_cli_config(&format!("[repo]\nuuid = \"{uuid}\"\n"));
    // No -u/-n: the selector comes from the config's default [repo].
    let out = mf_full(&["metarecord", "get"], None, &[("XDG_CONFIG_HOME", &xdg)], true);
    assert_ok(&out);
}

#[test]
fn test_no_config_ignores_the_default_repo() {
    let (uuid, _root) = init_repo("cfgrepo_noconf");
    let (_cfg, xdg) = xdg_with_cli_config(&format!("[repo]\nuuid = \"{uuid}\"\n"));
    // --no-config skips the file, so there is no selector → usage error (exit 2).
    let out =
        mf_full(&["--no-config", "metarecord", "get"], None, &[("XDG_CONFIG_HOME", &xdg)], true);
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
}

#[test]
fn test_explicit_selector_overrides_the_config_default_repo() {
    let (uuid, _root) = init_repo("cfgrepo_override");
    // The config points at a bogus repo; an explicit -u must still win.
    let (_cfg, xdg) = xdg_with_cli_config("[repo]\nname = \"does-not-exist\"\n");
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

// ── Entry manipulation ────────────────────────────────────────────────────────

#[test]
fn test_create_and_get_by_uuid() {
    let (repo, _root) = init_repo("create");
    let uuid = create_metarecord(&repo, &["rating:int=5", "genre:string=jazz"]);
    let entries = get_entries(&repo, &uuid);
    let list = entries.as_array().expect("a JSON array");
    assert_eq!(list.len(), 1);
    let entry = &list[0];
    assert_eq!(entry["uuid"], serde_json::json!(uuid));
    let fields = entry["fields"].as_array().unwrap();
    assert_eq!(fields.len(), 2);
    let rating = fields.iter().find(|f| f["name"] == "rating").expect("rating field");
    assert_eq!(rating["value"]["type"], "int");
    assert_eq!(rating["value"]["value"], 5);
    assert!(rating["id"].is_i64(), "mf get must include field ids");
}

#[test]
fn test_retype_converts_field_type() {
    let (repo, _root) = init_repo("retype");
    let uuid = create_metarecord(&repo, &["rating:int=5"]);

    let out = mf(&["-u", &repo, "retype", "rating", "string"]);
    assert_ok(&out);

    // The value now reads back as a string.
    let entries = get_entries(&repo, &uuid);
    let entry = &entries.as_array().unwrap()[0];
    let rating = entry["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "rating")
        .expect("rating field");
    assert_eq!(rating["value"]["type"], "string");
    assert_eq!(rating["value"]["value"], "5");

    // A conflicting Int write to the now-String field is rejected (exit != 0).
    let out = mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "set", "rating:int=9"]);
    assert_ne!(out.code, 0, "a conflicting-type write must fail: {}", out.stderr);
}

#[test]
fn test_field_list_enumerates_names_and_types() {
    let (repo, _root) = init_repo("field_list");
    create_metarecord(&repo, &["rating:int=5", "genre:string=jazz"]);
    create_metarecord(&repo, &["rating:int=3"]);

    // Unfiltered: one "name\ttype" line per distinct field name, deduplicated.
    let out = mf(&["-u", &repo, "field", "list"]);
    assert_ok(&out);
    let lines: Vec<&str> = out.stdout.lines().collect();
    assert!(lines.contains(&"rating\tint"), "got: {}", out.stdout);
    assert!(lines.contains(&"genre\tstring"), "got: {}", out.stdout);
    // The init-time root metarecord contributes these.
    assert!(lines.contains(&"mfr_path\ttree_ref"), "got: {}", out.stdout);
    // `rating` appears once despite two metarecords carrying it.
    assert_eq!(lines.iter().filter(|l| l.starts_with("rating\t")).count(), 1);

    // `list` is the group's default: bare `mf field` lists too.
    let bare = mf(&["-u", &repo, "field"]);
    assert_ok(&bare);
    assert_eq!(bare.stdout, out.stdout, "bare `field` must equal `field list`");

    // Filtered by type.
    let out = mf(&["-u", &repo, "field", "list", "--type", "tree_ref"]);
    assert_ok(&out);
    assert!(out.stdout.lines().all(|l| l.ends_with("\ttree_ref")), "got: {}", out.stdout);
    assert!(out.stdout.lines().any(|l| l == "mfr_path\ttree_ref"), "got: {}", out.stdout);
    assert!(!out.stdout.contains("rating"), "type filter must exclude int fields: {}", out.stdout);
}

#[test]
fn test_create_reserved_field_requires_force() {
    let (repo, _root) = init_repo("create_force");
    let out = mf(&["-u", &repo, "metarecord", "add", "mfr_path:tree_ref=/created_name"]);
    assert_eq!(out.code, 1, "creating with mfr_* without --force must fail");
    assert!(out.stderr.starts_with("error:"), "stderr: {}", out.stderr);

    let out = mf(&["-u", &repo, "metarecord", "add", "mfr_path:tree_ref=/created_name", "--force"]);
    assert_ok(&out);
    let uuid = out.stdout.trim().to_string();
    assert!(is_hex_uuid(&uuid));
    let entries = get_entries(&repo, &uuid);
    assert_eq!(entries[0]["fields"][0]["name"], "mfr_path");
}

#[test]
fn test_get_with_fields_filter() {
    let (repo, _root) = init_repo("get_fields");
    let uuid = create_metarecord(&repo, &["rating:int=5", "genre:string=jazz"]);
    let out = mf(&["-u", &repo, "metarecord", "-i", &uuid, "get", "--select", "genre"]);
    assert_ok(&out);
    let entries: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    let fields = entries[0]["fields"].as_array().unwrap();
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0]["name"], "genre");
}

#[test]
fn test_get_with_predicate() {
    let (repo, _root) = init_repo("get_pred");
    let jazz = create_metarecord(&repo, &["genre:string=jazz"]);
    let _rock = create_metarecord(&repo, &["genre:string=rock"]);
    let entries = get_entries(&repo, r#"genre = "jazz""#);
    let list = entries.as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["uuid"], serde_json::json!(jazz));
}

#[test]
fn test_get_predicate_with_limit_and_sort() {
    let (repo, _root) = init_repo("get_limit_sort");
    create_metarecord(&repo, &["rating:int=1"]);
    create_metarecord(&repo, &["rating:int=2"]);
    create_metarecord(&repo, &["rating:int=3"]);

    let out = mf(&[
        "-u",
        &repo,
        "metarecord",
        "-q",
        "rating >= 1",
        "get",
        "--select",
        "*",
        "--sort",
        "rating:desc",
        "--limit",
        "2",
    ]);
    assert_ok(&out);
    let list: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    let arr = list.as_array().unwrap();
    assert_eq!(arr.len(), 2, "--limit must cap the result at 2");

    let rating = |m: &serde_json::Value| -> i64 {
        m["fields"].as_array().unwrap().iter().find(|f| f["name"] == "rating").unwrap()["value"]
            ["value"]
            .as_i64()
            .unwrap()
    };
    // --sort rating:desc → the two highest, in order.
    assert_eq!(rating(&arr[0]), 3);
    assert_eq!(rating(&arr[1]), 2);
}

#[test]
fn test_list_prints_uuids_one_per_line() {
    let (repo, _root) = init_repo("list");
    let a = create_metarecord(&repo, &["x:int=1"]);
    let b = create_metarecord(&repo, &["x:int=2"]);
    let out = mf(&["-u", &repo, "metarecord", "get"]);
    assert_ok(&out);
    let lines: Vec<&str> = out.stdout.lines().collect();
    // Root entry + the two created entries.
    assert_eq!(lines.len(), 3, "stdout: {}", out.stdout);
    assert!(lines.iter().all(|l| is_hex_uuid(l)));
    assert!(lines.contains(&a.as_str()) && lines.contains(&b.as_str()));

    let out = mf(&["-u", &repo, "metarecord", "get", "--limit", "2"]);
    assert_ok(&out);
    assert_eq!(out.stdout.lines().count(), 2);
}

#[test]
fn test_set_uuid_replaces_all_rows() {
    let (repo, _root) = init_repo("set");
    let uuid = create_metarecord(&repo, &["tag:string=a", "tag:string=b"]);
    let out = mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "set", "tag:string=c"]);
    assert_ok(&out);
    let entries = get_entries(&repo, &uuid);
    let fields = entries[0]["fields"].as_array().unwrap();
    let tags: Vec<&serde_json::Value> = fields.iter().filter(|f| f["name"] == "tag").collect();
    assert_eq!(tags.len(), 1, "set_field must replace all rows of the name");
    assert_eq!(tags[0]["value"]["value"], "c");
}

#[test]
fn test_set_with_predicate_prints_updated_count() {
    let (repo, _root) = init_repo("set_pred");
    create_metarecord(&repo, &["genre:string=jazz"]);
    create_metarecord(&repo, &["genre:string=jazz"]);
    create_metarecord(&repo, &["genre:string=rock"]);
    let out =
        mf(&["-u", &repo, "metarecord", "-q", r#"genre = "jazz""#, "field", "set", "rating:int=4"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "2");
    let out = mf(&["-u", &repo, "metarecord", "-q", "rating = 4", "get"]);
    assert_ok(&out);
    assert_eq!(out.stdout.lines().count(), 2);
}

#[test]
fn test_set_reserved_field_requires_force() {
    let (repo, _root) = init_repo("set_force");
    let uuid = create_metarecord(&repo, &["x:int=1"]);
    let out = mf(&[
        "-u",
        &repo,
        "metarecord",
        "-i",
        &uuid,
        "field",
        "set",
        "mfr_path:tree_ref=/forced_name",
    ]);
    assert_eq!(out.code, 1, "writing mfr_* without --force must fail");
    assert!(out.stderr.starts_with("error:"));
    let out = mf(&[
        "-u",
        &repo,
        "metarecord",
        "-i",
        &uuid,
        "field",
        "set",
        "mfr_path:tree_ref=/forced_name",
        "--force",
    ]);
    assert_ok(&out);
}

#[test]
fn test_add_appends_multimap_row() {
    let (repo, _root) = init_repo("add");
    let uuid = create_metarecord(&repo, &["genre:string=jazz"]);
    let out = mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "add", "genre:string=blues"]);
    assert_ok(&out);
    let entries = get_entries(&repo, &uuid);
    let fields = entries[0]["fields"].as_array().unwrap();
    assert_eq!(fields.iter().filter(|f| f["name"] == "genre").count(), 2);
}

#[test]
fn test_add_with_predicate_appends_to_matches() {
    let (repo, _root) = init_repo("add_pred");
    create_metarecord(&repo, &["genre:string=jazz"]);
    create_metarecord(&repo, &["genre:string=jazz"]);
    create_metarecord(&repo, &["genre:string=rock"]);
    let out =
        mf(&["-u", &repo, "metarecord", "-q", r#"genre = "jazz""#, "field", "add", "tag:string=x"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "2");
    let out = mf(&["-u", &repo, "metarecord", "-q", r#"tag = "x""#, "get"]);
    assert_eq!(out.stdout.lines().count(), 2);
}

#[test]
fn test_remove_by_uuid_drops_only_matching_value_rows() {
    let (repo, _root) = init_repo("remove_uuid");
    let uuid = create_metarecord(&repo, &["tag:string=test", "tag:string=keep"]);
    let out = mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "delete", "tag:string=test"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "1");
    let entries = get_entries(&repo, &uuid);
    let fields = entries[0]["fields"].as_array().unwrap();
    let tags: Vec<&serde_json::Value> = fields.iter().filter(|f| f["name"] == "tag").collect();
    assert_eq!(tags.len(), 1, "only the matching-value row is removed");
    assert_eq!(tags[0]["value"]["value"], "keep");
}

#[test]
fn test_remove_by_predicate_prints_changed_count() {
    let (repo, _root) = init_repo("remove_pred");
    create_metarecord(&repo, &["tag:string=test", "tag:string=keep"]);
    create_metarecord(&repo, &["tag:string=test"]);
    create_metarecord(&repo, &["tag:string=keep"]);
    let out = mf(&[
        "-u",
        &repo,
        "metarecord",
        "-q",
        "tag IS PRESENT",
        "field",
        "delete",
        "tag:string=test",
    ]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "2", "two metarecords carried tag=test");
    assert_eq!(
        mf(&["-u", &repo, "metarecord", "-q", r#"tag = "test""#, "get"]).stdout.lines().count(),
        0
    );
    assert_eq!(
        mf(&["-u", &repo, "metarecord", "-q", r#"tag = "keep""#, "get"]).stdout.lines().count(),
        2
    );
}

#[test]
fn test_unset_deletes_single_row_by_id() {
    let (repo, _root) = init_repo("unset");
    let uuid = create_metarecord(&repo, &["genre:string=jazz", "genre:string=blues"]);
    let entries = get_entries(&repo, &uuid);
    let fields = entries[0]["fields"].as_array().unwrap();
    let jazz_id = fields
        .iter()
        .find(|f| f["value"]["value"] == "jazz")
        .and_then(|f| f["id"].as_i64())
        .expect("jazz row id");
    let out = mf(&["-u", &repo, "field", "delete", &jazz_id.to_string()]);
    assert_ok(&out);
    let entries = get_entries(&repo, &uuid);
    let fields = entries[0]["fields"].as_array().unwrap();
    let genres: Vec<&serde_json::Value> = fields.iter().filter(|f| f["name"] == "genre").collect();
    assert_eq!(genres.len(), 1);
    assert_eq!(genres[0]["value"]["value"], "blues");
}

#[test]
fn test_delete_by_uuid_prints_count() {
    let (repo, _root) = init_repo("delete");
    let uuid = create_metarecord(&repo, &["x:int=1"]);
    let out = mf(&["-u", &repo, "metarecord", "-i", &uuid, "delete"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "1");
    let out = mf(&["-u", &repo, "metarecord", "-i", &uuid, "get"]);
    assert_eq!(out.code, 1);
}

#[test]
fn test_delete_predicate_asks_for_confirmation() {
    let (repo, _root) = init_repo("delete_confirm");
    create_metarecord(&repo, &["genre:string=del_me"]);
    create_metarecord(&repo, &["genre:string=del_me"]);

    // Refusing the confirmation aborts without deleting.
    let out = mf_full(
        &["-p", daemon_port(), "-u", &repo, "metarecord", "-q", r#"genre = "del_me""#, "delete"],
        Some("n\n"),
        &[],
        false,
    );
    assert_eq!(out.code, 1, "refused confirmation should exit 1");
    let out = mf(&["-u", &repo, "metarecord", "-q", r#"genre = "del_me""#, "get"]);
    assert_eq!(out.stdout.lines().count(), 2, "entries must survive a refused confirmation");

    // --force skips the prompt.
    let out = mf(&["-u", &repo, "metarecord", "-q", r#"genre = "del_me""#, "delete", "--force"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "2");
    let out = mf(&["-u", &repo, "metarecord", "-q", r#"genre = "del_me""#, "get"]);
    assert_eq!(out.stdout.trim(), "");
}

// ── Query ─────────────────────────────────────────────────────────────────────

#[test]
fn test_query_prints_matching_uuids() {
    let (repo, _root) = init_repo("query");
    let high = create_metarecord(&repo, &["rating:int=5"]);
    let _low = create_metarecord(&repo, &["rating:int=1"]);
    let out = mf(&["-u", &repo, "metarecord", "-q", "rating > 3", "get"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), high);
}

// A bare UUID atom in the DSL (spec-query "Query DSL"): the whole point is
// that a UUID copied out of the GUI runs as a query and composes with the rest.
#[test]
fn test_query_bare_uuid_atom_selects_that_metarecord() {
    let (repo, _root) = init_repo("query_uuid_atom");
    let a = create_metarecord(&repo, &["rating:int=5"]);
    let b = create_metarecord(&repo, &["rating:int=1"]);

    let out = mf(&["-u", &repo, "metarecord", "-q", &a, "get"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), a);

    // Two pasted UUIDs fold into one set membership.
    let out = mf(&["-u", &repo, "metarecord", "-q", &format!("{a} OR {b}"), "get"]);
    assert_ok(&out);
    let mut got: Vec<&str> = out.stdout.lines().collect();
    got.sort_unstable();
    let mut want = vec![a.as_str(), b.as_str()];
    want.sort_unstable();
    assert_eq!(got, want);

    // ... and compose with an ordinary predicate.
    let out = mf(&["-u", &repo, "metarecord", "-q", &format!("{a} AND rating > 3"), "get"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), a);
    let out = mf(&["-u", &repo, "metarecord", "-q", &format!("{b} AND rating > 3"), "get"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "");

    // The shipped simplified grammar passes a pasted UUID through, so -s works
    // too (mf_cfg installs the shipped core config).
    let out = mf_cfg(&["-u", &repo, "metarecord", "-q", &a, "-s", "get"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), a);
    let out = mf_cfg(&["-u", &repo, "metarecord", "-q", &format!("{a} rating>3"), "-s", "get"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), a);

    // The typed flag decides the output shape (spec-data-model "mf metarecord
    // [-i|-q] get"): the same UUID through -i is the full JSON object.
    let out = mf(&["-u", &repo, "metarecord", "-i", &a, "get"]);
    assert_ok(&out);
    let json: serde_json::Value =
        serde_json::from_str(&out.stdout).expect("-i get should print JSON");
    assert_eq!(json[0]["uuid"], serde_json::Value::String(a.clone()));
}

#[test]
fn test_query_simplified_expands_before_running() {
    let (repo, _root) = init_repo("query_simplified");
    let high = create_metarecord(&repo, &["rating:int=5"]);
    let _low = create_metarecord(&repo, &["rating:int=1"]);
    // `rating=5` expands to `rating = 5` locally via the core grammar.
    let out = mf_cfg(&["-u", &repo, "metarecord", "-q", "rating=5", "-s", "get"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), high);
}

#[test]
fn test_query_simplified_date_macro_filters() {
    let (repo, _root) = init_repo("query_date_macro");
    // mfr_btime is reserved, so set it with --force. The datetime field spec
    // parses the ISO string to Unix ms.
    let recent =
        mf(&["-u", &repo, "metarecord", "add", "mfr_btime:datetime=2024-06-01", "--force"]);
    assert_ok(&recent);
    let recent = recent.stdout.trim().to_string();
    let old = mf(&["-u", &repo, "metarecord", "add", "mfr_btime:datetime=2020-01-01", "--force"]);
    assert_ok(&old);
    // `created since "2023-01-01"` → mfr_btime >= @"2023-01-01": only the recent one.
    let out =
        mf_cfg(&["-u", &repo, "metarecord", "-q", "created since \"2023-01-01\"", "-s", "get"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), recent);
}

#[test]
fn test_query_select_star_prints_objects() {
    let (repo, _root) = init_repo("query_star");
    create_metarecord(&repo, &["rating:int=5", "genre:string=jazz"]);
    let out = mf(&["-u", &repo, "metarecord", "-q", "rating = 5", "get", "--select", "*"]);
    assert_ok(&out);
    let parsed: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    let list = parsed.as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["fields"].as_array().unwrap().len(), 2);
}

#[test]
fn test_query_select_field_list_restricts_fields() {
    let (repo, _root) = init_repo("query_select");
    create_metarecord(&repo, &["rating:int=5", "genre:string=jazz"]);
    let out = mf(&["-u", &repo, "metarecord", "-q", "rating = 5", "get", "--select", "genre"]);
    assert_ok(&out);
    let parsed: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    let fields = parsed[0]["fields"].as_array().unwrap();
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0]["name"], "genre");
}

#[test]
fn test_query_sort_and_limit() {
    let (repo, _root) = init_repo("query_sort");
    let r1 = create_metarecord(&repo, &["rating:int=1", "kind:string=s"]);
    let r3 = create_metarecord(&repo, &["rating:int=3", "kind:string=s"]);
    let r2 = create_metarecord(&repo, &["rating:int=2", "kind:string=s"]);
    let out =
        mf(&["-u", &repo, "metarecord", "-q", r#"kind = "s""#, "get", "--sort", "rating:desc"]);
    assert_ok(&out);
    let lines: Vec<&str> = out.stdout.lines().collect();
    assert_eq!(lines, vec![r3.as_str(), r2.as_str(), r1.as_str()]);

    let out = mf(&[
        "-u",
        &repo,
        "metarecord",
        "-q",
        r#"kind = "s""#,
        "get",
        "--sort",
        "rating:asc",
        "--limit",
        "2",
    ]);
    assert_ok(&out);
    let lines: Vec<&str> = out.stdout.lines().collect();
    assert_eq!(lines, vec![r1.as_str(), r2.as_str()]);
}

#[test]
fn test_query_bad_dsl_is_usage_error() {
    let (repo, _root) = init_repo("query_bad");
    let out = mf(&["-u", &repo, "metarecord", "-q", "a = 1 and b = 2", "get"]);
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
    assert!(out.stderr.starts_with("error:"));
}

#[test]
fn test_query_bad_sort_is_usage_error() {
    let (repo, _root) = init_repo("query_bad_sort");
    let out = mf(&["-u", &repo, "metarecord", "-q", "a = 1", "get", "--sort", "rating:sideways"]);
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
}

// ── File tracking ─────────────────────────────────────────────────────────────

#[test]
fn test_track_creates_entry_and_is_idempotent() {
    let (repo, root) = init_repo("track");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("sub/file.txt"), b"hello").unwrap();
    let path = root.join("sub/file.txt");

    let out = mf(&["-u", &repo, "track", path.to_str().unwrap()]);
    assert_ok(&out);
    let uuid = out.stdout.trim().to_string();
    assert!(is_hex_uuid(&uuid));

    // Already tracked → idempotent: returns the existing uuid (POST /track is
    // idempotent, spec-file-tracking).
    let out = mf(&["-u", &repo, "track", path.to_str().unwrap()]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), uuid, "re-track returns the existing uuid");

    // Outside the repository root → operation error.
    let outside = temp_dir("track_outside");
    std::fs::write(outside.join("f.txt"), b"x").unwrap();
    let out = mf(&["-u", &repo, "track", outside.join("f.txt").to_str().unwrap()]);
    assert_eq!(out.code, 1, "stderr: {}", out.stderr);
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

// ── Ignore presets (spec-file-tracking "Ignore presets") ─────────────────────

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

    let out = mf(&["-u", &repo, "track", root.join("dir").to_str().unwrap()]);
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

// ── Query --values ────────────────────────────────────────────────────────────

#[test]
fn test_query_values_prints_raw_scalars() {
    let (repo, _root) = init_repo("values");
    create_metarecord(&repo, &["mf_schema:string=tag", "name:string=jazz"]);
    create_metarecord(&repo, &["mf_schema:string=tag", "name:string=rock", "weight:int=3"]);

    let out = mf(&[
        "-u",
        &repo,
        "metarecord",
        "-q",
        "mf_schema = \"tag\"",
        "get",
        "--select",
        "name",
        "--values",
    ]);
    assert_ok(&out);
    let mut names: Vec<&str> = out.stdout.lines().collect();
    names.sort_unstable();
    assert_eq!(names, vec!["jazz", "rock"]);

    let out = mf(&[
        "-u",
        &repo,
        "metarecord",
        "-q",
        "name = \"rock\"",
        "get",
        "--select",
        "weight",
        "--values",
    ]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "3");
}

#[test]
fn test_query_values_requires_a_single_selected_field() {
    let (repo, _root) = init_repo("values_usage");
    let out = mf(&["-u", &repo, "metarecord", "-q", "name = \"x\"", "get", "--values"]);
    assert_eq!(out.code, 2, "stdout: {}", out.stdout);
    let out = mf(&[
        "-u",
        &repo,
        "metarecord",
        "-q",
        "name = \"x\"",
        "get",
        "--select",
        "a,b",
        "--values",
    ]);
    assert_eq!(out.code, 2, "stdout: {}", out.stdout);
}

// ── Path resolution ───────────────────────────────────────────────────────────

#[test]
fn test_path_resolves_tracked_file() {
    let (repo, root) = init_repo("path");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("sub/file.txt"), b"hello").unwrap();

    let out = mf(&["-u", &repo, "track", root.join("sub/file.txt").to_str().unwrap()]);
    assert_ok(&out);
    let uuid = out.stdout.trim().to_string();

    let out = mf(&["-u", &repo, "path", &uuid]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), root.join("sub/file.txt").to_str().unwrap());

    let out = mf(&["-u", &repo, "path", "--relative", &uuid]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "/sub/file.txt");
}

#[test]
fn test_path_of_the_root_entry() {
    let (repo, root) = init_repo("path_root");
    let out = mf(&["-u", &repo, "metarecord", "get"]);
    assert_ok(&out);
    let root_uuid = out.stdout.trim().to_string();

    let out = mf(&["-u", &repo, "path", &root_uuid]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), root.to_str().unwrap());

    let out = mf(&["-u", &repo, "path", "--relative", &root_uuid]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "/");
}

#[test]
fn test_path_fails_on_entry_without_mfr_path() {
    let (repo, _root) = init_repo("path_none");
    let uuid = create_metarecord(&repo, &["title:string=no path"]);
    let out = mf(&["-u", &repo, "path", &uuid]);
    assert_eq!(out.code, 1, "stdout: {}", out.stdout);
    assert!(out.stderr.contains("mfr_path"), "stderr: {}", out.stderr);
}

// ── Schema ────────────────────────────────────────────────────────────────────

const FILM_SCHEMA: &str = r#"{
  "version": 1,
  "groups": [
    {"targets": ["film"],
     "constraints": [{"field": "rating", "type": "int"}]}
  ]
}"#;

#[test]
fn test_schema_workflow() {
    let (repo, root) = init_repo("schema");
    // Violating entry created before any schema exists (delta validation
    // would reject it afterwards).
    let bad = create_metarecord(&repo, &["mf_schema:string=film", "rating:string=oops"]);

    std::fs::write(root.join(".metafolder/schema.json"), FILM_SCHEMA).unwrap();
    let out = mf(&["-u", &repo, "schema", "reload"]);
    assert_ok(&out);

    let out = mf(&["-u", &repo, "schema", "show"]);
    assert_ok(&out);
    let parsed: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(parsed["version"], 1);

    // One violation: exit code 1, one line per violation plus the summary.
    let out = mf(&["-u", &repo, "schema", "check"]);
    assert_eq!(out.code, 1, "violations must yield exit code 1\nstdout: {}", out.stdout);
    assert!(out.stdout.contains(&bad), "violation line should name the entry");
    assert!(out.stdout.contains("Checked 2 metarecords, 1 violation"), "stdout: {}", out.stdout);

    // Fix the wrong-typed field: under the one-value-type-per-field invariant a
    // String field cannot be set to an Int directly — `retype` is the way to
    // change an established type (the un-parsable "oops" falls back to 0).
    let out = mf(&["-u", &repo, "retype", "rating", "int"]);
    assert_ok(&out);
    let out = mf(&["-u", &repo, "schema", "check"]);
    assert_ok(&out);
    assert!(out.stdout.contains("0 violations"), "stdout: {}", out.stdout);

    // --json prints the raw response body.
    let out = mf(&["-u", &repo, "schema", "check", "--json"]);
    assert_ok(&out);
    let parsed: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(parsed["checked"], 2);
}

#[test]
fn test_schema_check_with_predicate() {
    let (repo, root) = init_repo("schema_pred");
    create_metarecord(&repo, &["mf_schema:string=film", "rating:string=bad"]);
    std::fs::write(root.join(".metafolder/schema.json"), FILM_SCHEMA).unwrap();
    let out = mf(&["-u", &repo, "schema", "reload"]);
    assert_ok(&out);

    // The predicate restricts the scan to non-matching entries: no violation.
    let out = mf(&["-u", &repo, "schema", "check", r#"mf_schema = "documentary""#]);
    assert_ok(&out);
    assert!(out.stdout.contains("Checked 0 metarecords"), "stdout: {}", out.stdout);
}

#[test]
fn test_schema_reload_invalid_file_fails() {
    let (repo, root) = init_repo("schema_invalid");
    std::fs::write(root.join(".metafolder/schema.json"), "{not json").unwrap();
    let out = mf(&["-u", &repo, "schema", "reload"]);
    assert_eq!(out.code, 1, "stderr: {}", out.stderr);
    assert!(out.stderr.starts_with("error:"));
}

// ── Event log: mf log / mf log show / mf prune (spec-event-log) ─────────────────

#[test]
fn test_log_lists_revisions_most_recent_first() {
    let (repo, _root) = init_repo("log_list");
    let uuid = create_metarecord(&repo, &["rating:int=3"]);
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "set", "rating:int=5"]));

    let out = mf(&["-u", &repo, "log", "list"]);
    assert_ok(&out);
    // HEAD is marked and is the first (most recent) line.
    assert!(out.stdout.contains("\u{2190} HEAD"), "stdout: {}", out.stdout);
    let first = out.stdout.lines().next().unwrap();
    assert!(first.starts_with('>') && first.contains("\u{2190} HEAD"), "first line: {first}");
    assert!(out.stdout.contains("rev "), "stdout: {}", out.stdout);
}

#[test]
fn test_log_graph_renders_branches_default_hides_them() {
    let (repo, _root) = init_repo("log_graph");
    let uuid = create_metarecord(&repo, &["rating:int=1"]);
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "set", "rating:int=2"]));
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "set", "rating:int=3"]));
    // Roll back the last write, then write again: this forks a new branch,
    // leaving the rating=3 revision on a divergent branch.
    assert_ok(&mf(&["-u", &repo, "log", "rollback", "--silent"]));
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "set", "rating:int=9"]));

    // The graph draws every branch: a divergent column and its convergence.
    let graph = mf(&["-u", &repo, "log", "list", "--graph"]);
    assert_ok(&graph);
    assert!(graph.stdout.contains("\u{2190} HEAD"), "stdout: {}", graph.stdout);
    assert!(graph.stdout.contains("|/"), "expected a convergence: {}", graph.stdout);

    // The default (active line) hides the divergent branch: fewer revisions.
    let active = mf(&["-u", &repo, "log", "list"]);
    assert_ok(&active);
    let count = |s: &str| s.matches("rev ").count();
    assert!(
        count(&active.stdout) < count(&graph.stdout),
        "active {} should show fewer revisions than graph {}",
        active.stdout,
        graph.stdout
    );
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
    assert!(out.stdout.starts_with("Revision "), "stdout: {}", out.stdout);
    assert!(out.stdout.contains("set_field(rating)"), "stdout: {}", out.stdout);
    assert!(out.stdout.contains("before:  3"), "stdout: {}", out.stdout);
    assert!(out.stdout.contains("after:   5"), "stdout: {}", out.stdout);

    // --raw prints JSON with the revision object.
    let raw = mf(&["-u", &repo, "log", "show", "HEAD", "--raw"]);
    assert_ok(&raw);
    let parsed: serde_json::Value = serde_json::from_str(&raw.stdout).unwrap();
    assert!(parsed["revision"]["is_head"].as_bool().unwrap());
}

#[test]
fn test_log_show_rejects_bad_target() {
    let (repo, _root) = init_repo("log_show_bad");
    let out = mf(&["-u", &repo, "log", "show", "notanumber"]);
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
}

#[test]
fn test_prune_linearize_with_no_branches_removes_nothing() {
    let (repo, _root) = init_repo("prune_lin");
    let uuid = create_metarecord(&repo, &["rating:int=3"]);
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "set", "rating:int=5"]));

    // A far-future timestamp resolves to HEAD; with no side branches,
    // linearize removes nothing.
    let out =
        mf(&["-u", &repo, "log", "prune", "linearize", "--timestamp", "@9999999999999", "--force"]);
    assert_ok(&out);
    assert!(out.stdout.contains("Pruned 0 operations"), "stdout: {}", out.stdout);
    assert!(out.stdout.contains("linearized"), "stdout: {}", out.stdout);
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
fn test_rollback_plan_previews_operations() {
    let (repo, _root) = init_repo("rbk_plan");
    let uuid = create_metarecord(&repo, &["rating:int=3"]);
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "set", "rating:int=5"]));
    let out = mf(&["-u", &repo, "log", "rollback", "plan"]);
    assert_ok(&out);
    assert!(out.stdout.contains("set_field"), "stdout: {}", out.stdout);
    assert!(out.stdout.contains("operations."), "stdout: {}", out.stdout);
}

#[test]
fn test_rollback_undoes_last_revision_and_releases_lock() {
    let (repo, _root) = init_repo("rbk_run");
    let uuid = create_metarecord(&repo, &["rating:int=3"]);
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "set", "rating:int=5"]));

    let out = mf(&["-u", &repo, "log", "rollback", "--silent"]);
    assert_ok(&out);

    // The last set was undone.
    let entries = get_entries(&repo, &uuid);
    let rating =
        entries[0]["fields"].as_array().unwrap().iter().find(|f| f["name"] == "rating").unwrap();
    assert_eq!(rating["value"]["value"], 3, "rating should revert to 3");

    // The lock was released: a subsequent write succeeds.
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "set", "rating:int=9"]));
}

#[test]
fn test_rollback_bad_move_policy_is_usage_error() {
    let (repo, _root) = init_repo("rbk_policy");
    let uuid = create_metarecord(&repo, &["rating:int=3"]);
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "set", "rating:int=5"]));
    let out = mf(&["-u", &repo, "log", "rollback", "--on-move-available", "bogus"]);
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

// ── Verb-tree additions (spec-data-model "* CLI") ─────────────────────────────

#[test]
fn test_repo_selected_by_name() {
    // The repo's name is derived from its (unique) directory basename; -n
    // resolves it to the uuid through GET /repos.
    let (uuid, root) = init_repo("by_name");
    let name = root.file_name().unwrap().to_str().unwrap().to_string();

    let by_name = mf(&["-n", &name, "metarecord", "add", "tag:string=x"]);
    assert_ok(&by_name);
    // The record is visible when addressing the same repo by uuid.
    let listed = mf(&["-u", &uuid, "metarecord", "get"]);
    assert_ok(&listed);
    assert!(listed.stdout.contains(by_name.stdout.trim()));

    // An unknown name is an operation error.
    let missing = mf(&["-n", "no-such-repo-xyz", "metarecord", "get"]);
    assert_eq!(missing.code, 1, "stderr: {}", missing.stderr);
}

#[test]
fn test_metarecord_set_overwrites_whole_record_and_needs_force() {
    let (repo, _root) = init_repo("mset");
    let uuid = create_metarecord(&repo, &["a:int=1", "b:string=keep"]);

    // Without -f it refuses and changes nothing.
    let no_force = mf(&["-u", &repo, "metarecord", "-i", &uuid, "set", "c:int=9"]);
    assert_eq!(no_force.code, 2, "stderr: {}", no_force.stderr);

    let out = mf(&["-u", &repo, "metarecord", "-i", &uuid, "set", "c:int=9", "-f"]);
    assert_ok(&out);
    let entries = get_entries(&repo, &uuid);
    let entry = &entries.as_array().unwrap()[0];
    let names: Vec<&str> =
        entry["fields"].as_array().unwrap().iter().map(|f| f["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["c"], "old fields dropped, only the new set remains");
}

#[test]
fn test_field_multi_value_set_and_unset() {
    let (repo, _root) = init_repo("fmulti");
    let uuid = create_metarecord(&repo, &["genre:string=jazz"]);

    // Set two values of `tag` at once (multi-map).
    let out = mf(&[
        "-u",
        &repo,
        "metarecord",
        "-i",
        &uuid,
        "field",
        "set",
        "tag:string=a",
        "tag:string=b",
    ]);
    assert_ok(&out);
    let count_tags = |target: &str| -> usize {
        let entries = get_entries(&repo, target);
        let entry = &entries.as_array().unwrap()[0];
        entry["fields"].as_array().unwrap().iter().filter(|f| f["name"] == "tag").count()
    };
    assert_eq!(count_tags(&uuid), 2);

    // Unset removes the whole field.
    let out = mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "unset", "tag"]);
    assert_ok(&out);
    assert_eq!(count_tags(&uuid), 0);
}

#[test]
fn test_field_by_id_get_set_delete() {
    let (repo, _root) = init_repo("fbyid");
    let uuid = create_metarecord(&repo, &["rating:int=5"]);
    let entries = get_entries(&repo, &uuid);
    let id = entries.as_array().unwrap()[0]["fields"][0]["id"].as_i64().unwrap().to_string();

    // get by id
    let got = mf(&["-u", &repo, "field", "get", &id]);
    assert_ok(&got);
    assert!(got.stdout.contains("rating"));

    // set by id: rename + revalue, keeping the id
    let set = mf(&["-u", &repo, "field", "set", &id, "score:int=9"]);
    assert_ok(&set);
    let entries = get_entries(&repo, &uuid);
    let entry = &entries.as_array().unwrap()[0];
    assert_eq!(entry["fields"][0]["id"].as_i64().unwrap().to_string(), id);
    assert_eq!(entry["fields"][0]["name"], "score");

    // delete by id
    let del = mf(&["-u", &repo, "field", "delete", &id]);
    assert_ok(&del);
    let entries = get_entries(&repo, &uuid);
    assert_eq!(entries.as_array().unwrap()[0]["fields"].as_array().unwrap().len(), 0);
}

// ── mf trash ────────────────────────────────────────────────────────────────

use metafolder_cli::trash::{Reason, TrashDir};

mod common;
use common::{TempDir, TempFile};

/// The repo's `internal/trash/` directory (matches the daemon-reported path).
fn repo_trash(root: &std::path::Path) -> TrashDir {
    TrashDir::new(root.join(".metafolder").join("internal").join("trash"))
}

// `mf trash list/restore/prune` over the real daemon: the CLI discovers the
// trash via `GET /repos/:repo` (internal_dir) and acts on it — no daemon
// endpoint is involved (spec-trash.org).
#[test]
fn test_trash_list_restore_and_prune() {
    let (repo, root) = init_repo("trash");
    let trash = repo_trash(&root);

    // Seed the trash as a rollback overwrite would.
    let victim = root.join("victim.txt");
    std::fs::write(&victim, b"precious").unwrap();
    let e1 = trash.trash_path(&victim, Reason::Rollback, Some(7), None, None).unwrap();
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
}

// `mf trash -f <file>` trashes a tracked file, recording its metarecord; it
// errors (before touching the file) when the file has no metarecord or the
// daemon is unreachable.
#[test]
fn test_trash_add_moves_a_tracked_file() {
    let (repo, root) = init_repo("trashadd");
    let file = root.join("doc.txt");
    std::fs::write(&file, b"data").unwrap();
    let uuid = mf(&["-u", &repo, "track", file.to_str().unwrap()]).stdout.trim().to_string();
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
    let uuid = mf(&["-u", &repo, "track", file.to_str().unwrap()]).stdout.trim().to_string();
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
    let uuid = mf(&["-u", &repo, "track", dir.to_str().unwrap()]).stdout.trim().to_string();
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
// metarecord: every orphaned descendant is put back where it was (spec-trash),
// so no metarecord is left orphaned and no duplicate is created.
#[test]
fn test_trash_restore_relinks_a_directory_subtree() {
    let (repo, root) = init_repo("trashdirsubtree");
    let dir = root.join("folder");
    std::fs::create_dir_all(dir.join("nested")).unwrap();
    std::fs::write(dir.join("nested/b.txt"), b"bb").unwrap();
    // Track the whole subtree (folder, nested, b.txt): track ensures parents.
    let b_uuid = mf(&["-u", &repo, "track", dir.join("nested/b.txt").to_str().unwrap()])
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
        "mfr_path = \"folder\"",
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
    let m1 = mf(&["-u", &repo, "track", file.to_str().unwrap()]).stdout.trim().to_string();
    assert_ok(&mf(&["-u", &repo, "trash", "-f", file.to_str().unwrap()]));

    // Orphan the original, then let a *different* metarecord claim A.txt's slot.
    assert_ok(&mf(&[
        "-u",
        &repo,
        "metarecord",
        "-i",
        &m1,
        "field",
        "unset",
        "mfr_path",
        "--force",
    ]));
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
    // m1 stays orphaned (skipped); m2 keeps the position.
    assert_ne!(mf(&["-u", &repo, "path", &m1]).code, 0, "m1 left orphaned");
    assert!(mf(&["-u", &repo, "path", &m2]).stdout.contains("A.txt"), "m2 keeps the slot");
}

/// Polls `f` every 200 ms up to `tries` times; returns whether it ever held.
fn poll(tries: u32, f: impl Fn() -> bool) -> bool {
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
fn uuids_at(repo: &str, name: &str) -> Vec<String> {
    let out = mf(&["-u", repo, "metarecord", "-q", &format!("mfr_path = {name:?}"), "get"]);
    out.stdout.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect()
}

// Regression (spec-trash "Restore"): trashing a file through the metarecord path
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
    let b =
        mf(&["-u", &repo, "track", dir.join("B.txt").to_str().unwrap()]).stdout.trim().to_string();
    let a =
        mf(&["-u", &repo, "metarecord", "-q", "mfr_path = \"A\"", "get"]).stdout.trim().to_string();
    assert!(is_hex_uuid(&b) && is_hex_uuid(&a));

    // Trash the file (captures ancestor A while live), then delete A's
    // metarecord and orphan B — as sweeping orphans would.
    assert_ok(&mf(&["-u", &repo, "trash", "-f", dir.join("B.txt").to_str().unwrap()]));
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &a, "delete"]));
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &b, "field", "unset", "mfr_path", "--force"]));

    let id = repo_trash(&root).entries().unwrap()[0].id.clone();
    let out = mf(&["-u", &repo, "trash", "restore", &id]);
    assert_ok(&out); // the unavailable parent is skipped, not a hard error
    assert_eq!(std::fs::read(dir.join("B.txt")).unwrap(), b"b");
}

// Restoring an entry whose recorded metarecord was deleted meanwhile (e.g. the
// user swept orphans) must still bring the bytes back — re-linking a gone
// metarecord is simply skipped (the watcher makes a fresh one), not an error.
#[test]
fn test_trash_restore_tolerates_a_deleted_metarecord() {
    let (repo, root) = init_repo("trashgone");
    let file = root.join("f.txt");
    std::fs::write(&file, b"data").unwrap();
    let uuid = mf(&["-u", &repo, "track", file.to_str().unwrap()]).stdout.trim().to_string();
    assert!(is_hex_uuid(&uuid));
    assert_ok(&mf(&["-u", &repo, "trash", "-f", file.to_str().unwrap()]));
    // The user deletes the (now stale) metarecord.
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &uuid, "delete"]));

    let entry_id = repo_trash(&root).entries().unwrap()[0].id.clone();
    let out = mf(&["-u", &repo, "trash", "restore", &entry_id]);
    assert_ok(&out); // not a hard error just because the metarecord is gone
    assert_eq!(std::fs::read(&file).unwrap(), b"data");
}

// Restoring a nested file whose parent directory was *also* trashed re-links
// the original ancestor directory metarecords too (captured at trash time), so
// the recreated parent directory is tracked by the original metarecord rather
// than left orphaned for the watcher to duplicate (spec-trash "Restore").
#[test]
fn test_trash_restore_relinks_ancestors_of_a_nested_file() {
    let (repo, root) = init_repo("trashancestor");
    let dir = root.join("A");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("B.txt"), b"b").unwrap();
    // Track B (ensures the A and B metarecords).
    let b_uuid =
        mf(&["-u", &repo, "track", dir.join("B.txt").to_str().unwrap()]).stdout.trim().to_string();
    let a_uuid =
        mf(&["-u", &repo, "metarecord", "-q", "mfr_path = \"A\"", "get"]).stdout.trim().to_string();
    assert!(is_hex_uuid(&b_uuid) && is_hex_uuid(&a_uuid));

    // Trash the file (captures its ancestor A while A is still live), then the
    // directory; orphan both metarecords as the watcher's cascade would.
    assert_ok(&mf(&["-u", &repo, "trash", "-f", dir.join("B.txt").to_str().unwrap()]));
    assert_ok(&mf(&[
        "-u",
        &repo,
        "metarecord",
        "-i",
        &b_uuid,
        "field",
        "unset",
        "mfr_path",
        "--force",
    ]));
    assert_ok(&mf(&["-u", &repo, "trash", "-f", dir.to_str().unwrap()]));
    assert_ok(&mf(&[
        "-u",
        &repo,
        "metarecord",
        "-i",
        &a_uuid,
        "field",
        "unset",
        "mfr_path",
        "--force",
    ]));
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
// dead-end when part of a directory was restored first (spec-trash "Restore").
#[test]
fn test_trash_restore_merges_a_directory_into_an_existing_one() {
    let (repo, root) = init_repo("trashmerge");
    let dir = root.join("A");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("c.txt"), b"c").unwrap();
    assert_ok(&mf(&["-u", &repo, "track", dir.join("c.txt").to_str().unwrap()]));

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
    let uuid = mf(&["-u", &repo, "track", file.to_str().unwrap()]).stdout.trim().to_string();
    assert!(is_hex_uuid(&uuid));

    assert_ok(&mf(&["-u", &repo, "trash", "-f", file.to_str().unwrap()]));
    // Orphan the metarecord (mfr_path unset), mimicking a watched-repo deletion.
    assert_ok(&mf(&[
        "-u",
        &repo,
        "metarecord",
        "-i",
        &uuid,
        "field",
        "unset",
        "mfr_path",
        "--force",
    ]));
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
    let uuid = mf(&["-u", &repo, "track", file.to_str().unwrap()]).stdout.trim().to_string();
    assert!(is_hex_uuid(&uuid));

    // The filesystem root metarecord (the only directory) is the expected parent.
    let root_uuid = mf(&["-u", &repo, "metarecord", "-q", "mfr_type = \"dir\"", "get"])
        .stdout
        .trim()
        .to_string();
    assert!(is_hex_uuid(&root_uuid), "one dir (the fs root), got: {root_uuid}");

    assert_ok(&mf(&["-u", &repo, "trash", "-f", file.to_str().unwrap()]));
    // Orphan the metarecord (mfr_path unset), mimicking a watched-repo deletion.
    assert_ok(&mf(&[
        "-u",
        &repo,
        "metarecord",
        "-i",
        &uuid,
        "field",
        "unset",
        "mfr_path",
        "--force",
    ]));
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
fn poll_mf(args: &[&str], pred: impl Fn(&str) -> bool) -> String {
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
    let uuid = mf(&["-u", &repo, "track", file.to_str().unwrap()]).stdout.trim().to_string();

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

// End to end: in a watched repo, `mf trash -f` produces a file_deleted; a
// rollback then auto-restores the exact file from the trash (spec-trash
// "rollback auto-restore").
#[test]
fn test_rollback_auto_restores_from_trash() {
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
    let uuid =
        poll_mf(&["-u", &repo, "metarecord", "-q", "mfr_path = \"doc.txt\"", "get"], is_hex_uuid);

    // Trash it; wait for the watcher to record the deletion (mfr_path → Nothing).
    assert_ok(&mf(&["-u", &repo, "trash", "-f", file.to_str().unwrap()]));
    poll_mf(&["-u", &repo, "metarecord", "-q", "mfr_path = \"doc.txt\"", "get"], |s| s.is_empty());
    assert!(!file.exists(), "the file is in the trash");

    // Roll back the deletion → the file is auto-restored from the trash.
    let out = mf(&["-u", &repo, "log", "rollback"]);
    assert_ok(&out);
    assert_eq!(std::fs::read(&file).unwrap(), b"precious", "the file is back");
    // The metadata is restored too: the metarecord is at doc.txt again.
    let back =
        poll_mf(&["-u", &repo, "metarecord", "-q", "mfr_path = \"doc.txt\"", "get"], |s| s == uuid);
    assert_eq!(back, uuid);
    // The trash entry was consumed.
    assert!(repo_trash(&root).entries().unwrap().is_empty(), "the entry is consumed");
}

// Rolling back a trashed *directory* must restore the whole subtree's
// metarecords, not just the top one. The bytes come back via the trash, and the
// metarecords are restored by the rollback itself (navigation — no new
// revision); a descendant left orphaned would be re-tracked as a duplicate.
#[test]
fn test_rollback_restores_a_trashed_directory_subtree() {
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
    let b_uuid =
        poll_mf(&["-u", &repo, "metarecord", "-q", "mfr_path = \"B.txt\"", "get"], is_hex_uuid);
    let a_uuid =
        poll_mf(&["-u", &repo, "metarecord", "-q", "mfr_path = \"A\"", "get"], is_hex_uuid);

    // Trash the directory; wait for the watcher to cascade the deletion.
    assert_ok(&mf(&["-u", &repo, "trash", "-f", dir.to_str().unwrap()]));
    poll_mf(&["-u", &repo, "metarecord", "-q", "mfr_path = \"B.txt\"", "get"], |s| s.is_empty());
    assert!(!dir.exists(), "the directory is in the trash");

    // Roll back the deletion.
    assert_ok(&mf(&["-u", &repo, "log", "rollback"]));
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

// ── Cross-repo sync: utility subcommands (spec-sync "Utility subcommands") ─────

#[test]
fn test_sync_link_status_unlink_roundtrip() {
    let (repo_a, _ra) = init_repo("synca");
    let (repo_b, _rb) = init_repo("syncb");
    let rec_a = create_metarecord(&repo_a, &["tag:string=x"]);
    let rec_b = create_metarecord(&repo_b, &["tag:string=y"]);

    // Link the two records (URL/positional order need not be canonical).
    let out = mf(&["sync", "link", &repo_a, &repo_b, &rec_a, &rec_b]);
    assert_ok(&out);
    let link = out.stdout.trim().to_string();
    assert!(is_hex_uuid(&link), "link should print the link uuid, got: '{}'", out.stdout);

    // status lists the link in the never-synced state.
    let out = mf(&["sync", "status", &repo_a, &repo_b]);
    assert_ok(&out);
    assert!(out.stdout.contains(&link), "status should list the link: {}", out.stdout);
    assert!(out.stdout.contains("never_synced"), "state: {}", out.stdout);

    // status --json exposes the structured per-link states.
    let out = mf(&["sync", "status", &repo_b, &repo_a, "--json"]);
    assert_ok(&out);
    let body: serde_json::Value = serde_json::from_str(&out.stdout).expect("json");
    assert_eq!(body["links"][0]["state"], "never_synced");

    // unlink removes it.
    let out = mf(&["sync", "unlink", &repo_a, &repo_b, &link]);
    assert_ok(&out);
    let out = mf(&["sync", "status", &repo_a, &repo_b]);
    assert_ok(&out);
    assert!(!out.stdout.contains(&link), "the link is gone: {}", out.stdout);
}

#[test]
fn test_sync_link_missing_record_errors() {
    let (repo_a, _ra) = init_repo("synce_a");
    let (repo_b, _rb) = init_repo("synce_b");
    let rec_a = create_metarecord(&repo_a, &["tag:string=x"]);
    let ghost = Uuid::new_v4().as_simple().to_string();
    let out = mf(&["sync", "link", &repo_a, &repo_b, &rec_a, &ghost]);
    assert_eq!(out.code, 1, "missing endpoint is an Op error: {}", out.stderr);
}

#[test]
fn test_sync_unlink_with_endpoint_deletes_record() {
    let (repo_a, _ra) = init_repo("syncw_a");
    let (repo_b, _rb) = init_repo("syncw_b");
    let rec_a = create_metarecord(&repo_a, &["tag:string=x"]);
    let rec_b = create_metarecord(&repo_b, &["tag:string=y"]);
    let out = mf(&["sync", "link", &repo_a, &repo_b, &rec_a, &rec_b]);
    assert_ok(&out);
    let link = out.stdout.trim().to_string();

    // Drop the link plus the endpoint record in the first-named repo.
    let out = mf(&["sync", "unlink", &repo_a, &repo_b, &link, "--with-endpoint", "a"]);
    assert_ok(&out);
    // rec_a is gone …
    let out = mf(&["-u", &repo_a, "metarecord", "-i", &rec_a, "get"]);
    assert_eq!(out.code, 1, "record A should be deleted: {}{}", out.stdout, out.stderr);
    // … rec_b remains.
    let out = mf(&["-u", &repo_b, "metarecord", "-i", &rec_b, "get"]);
    assert_ok(&out);
}

// ── Cross-repo sync: mf sync plan (plan-repo lifecycle) ───────────────────────

#[test]
fn test_sync_plan_creates_hidden_plan_repo() {
    let (a, _ra) = init_repo("plan_a");
    let (b, _rb) = init_repo("plan_b");
    let dir = temp_dir("plan_intents");
    let intents = dir.join("i.toml");
    std::fs::write(&intents, format!("[[intents]]\nrepo = '{a}'\nquery = 'mf_watch = true'\n"))
        .unwrap();

    let out = mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]);
    assert_ok(&out);

    // The plan repo is hidden from the default listing …
    let list = mf(&["repo", "list"]);
    assert_ok(&list);
    assert!(!list.stdout.contains("plan-"), "plan repo must be hidden: {}", list.stdout);

    // … but visible with --all, as exactly one system repo for this pair.
    // (The shared daemon may hold other pairs' plan repos, so scope by name.)
    let name = plan_name(&a, &b);
    assert_eq!(count_repos_named(&name), 1, "one plan repo for this pair");

    // Re-running recreates it without error (only the latest plan exists).
    assert_ok(&mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]));
    assert_eq!(count_repos_named(&name), 1, "still exactly one plan repo after re-plan");
}

/// The plan repo name for a pair (canonical order = sorted simple-hex UUIDs).
fn plan_name(a: &str, b: &str) -> String {
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    format!("plan-{lo}-{hi}")
}

/// Number of loaded repos (system included) with the given exact name.
fn count_repos_named(name: &str) -> usize {
    let all = mf(&["repo", "list", "--all"]);
    assert_ok(&all);
    let repos: serde_json::Value = serde_json::from_str(&all.stdout).expect("repo list json");
    repos
        .as_array()
        .map(|a| a.iter().filter(|r| r["name"].as_str() == Some(name)).count())
        .unwrap_or(0)
}

/// Extracts the `plan repo: <uuid>` line printed by `mf sync plan`.
fn plan_repo_uuid(out: &Out) -> String {
    out.stdout
        .lines()
        .find_map(|l| l.strip_prefix("plan repo: "))
        .expect("plan should print 'plan repo: <uuid>'")
        .trim()
        .to_string()
}

/// Inits a repo, writes `files` (relative path → bytes), enables tracking on the
/// root and reconciles, so file records get realistic `mfr_path` children.
/// Returns the repo uuid *and* the directory guard — the caller has to keep the
/// latter, or the files go away with it.
fn tracked_repo(prefix: &str, files: &[(&str, &[u8])]) -> (String, TempDir) {
    let (repo, root) = init_repo(prefix);
    for (rel, content) in files {
        let p = root.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, content).unwrap();
    }
    // A fresh repo has one entry: the filesystem root.
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
    (repo, root)
}

/// The single uuid matching a DSL query in a repo (asserts exactly one).
fn query_one(repo: &str, dsl: &str) -> String {
    let out = mf(&["-u", repo, "metarecord", "-q", dsl, "get"]);
    assert_ok(&out);
    let uuids: Vec<&str> = out.stdout.split_whitespace().collect();
    assert_eq!(uuids.len(), 1, "expected one match for {dsl:?}, got: {}", out.stdout);
    uuids[0].to_string()
}

/// Writes an intents TOML to a fresh temp file. The returned guard *is* the
/// path (it derefs to one) and removes the file with its directory when the
/// test ends — so keep it bound: a temporary would take the file with it.
fn write_intents(prefix: &str, content: &str) -> TempFile {
    TempFile::new(prefix, content.as_bytes())
}

#[test]
fn test_sync_plan_exact_match_writes_create_link() {
    // Same relative path on both sides, *different bytes* — so a match can only
    // be by TreeRef identity (path), never by content.
    let (a, _adir) = tracked_repo("plan_ex_a", &[("song.mp3", b"aaa")]);
    let (b, _bdir) = tracked_repo("plan_ex_b", &[("song.mp3", b"bbb")]);
    let rec_a = query_one(&a, "mfr_path = \"song.mp3\"");
    let rec_b = query_one(&b, "mfr_path = \"song.mp3\"");

    let intents = write_intents(
        "plan_ex",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    let out = mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]);
    assert_ok(&out);
    assert!(out.stdout.contains("operations: 1"), "one create-link op: {}", out.stdout);
    let plan = plan_repo_uuid(&out);

    // The plan repo holds one create-link op linking both file records by path.
    let got = mf(&[
        "-u",
        &plan,
        "metarecord",
        "-q",
        "plan_kind = \"create-link\"",
        "get",
        "--select",
        "*",
    ]);
    assert_ok(&got);
    let ops: serde_json::Value = serde_json::from_str(&got.stdout).unwrap();
    let ops = ops.as_array().unwrap();
    assert_eq!(ops.len(), 1, "one create-link op: {}", got.stdout);
    let endpoints = op_endpoints(&ops[0]);
    assert!(
        endpoints.contains(&rec_a) && endpoints.contains(&rec_b),
        "op must link both records: {endpoints:?}"
    );
    // No link was actually created — plan is read-only w.r.t. the synced repos.
    let status = mf(&["sync", "status", &a, &b]);
    assert!(status.stderr.contains("no links") || status.stdout.trim().is_empty());
}

/// The two endpoint metarecord UUIDs of a plan op (`plan_a`, `plan_b`).
fn op_endpoints(op: &serde_json::Value) -> Vec<String> {
    let fields = op["fields"].as_array().unwrap();
    ["plan_a", "plan_b"]
        .iter()
        .filter_map(|name| {
            fields.iter().find(|f| f["name"] == *name)?.get("value")?["value"]["metarecord"]
                .as_str()
                .map(String::from)
        })
        .collect()
}

#[test]
fn test_sync_plan_no_match_allocates_bare_record() {
    // In scope in A, with no counterpart at the same path in B → bare record.
    let (a, _adir) = tracked_repo("plan_bare_a", &[("lonely.mp3", b"x")]);
    let (b, _bdir) = tracked_repo("plan_bare_b", &[]);
    let rec_a = query_one(&a, "mfr_path = \"lonely.mp3\"");
    let intents = write_intents(
        "plan_bare",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n"),
    );

    let out = mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]);
    assert_ok(&out);
    // A bare *file* link → create-link + sync (placement) + copy (content) + chmod (mode).
    assert!(
        out.stdout.contains("operations: 4"),
        "create-link + sync + copy + chmod: {}",
        out.stdout
    );
    let plan = plan_repo_uuid(&out);

    // A copy op sources the content from the existing side (plan_from = a|b).
    let cp = mf(&["-u", &plan, "metarecord", "-q", "plan_kind = \"copy\"", "get", "--select", "*"]);
    assert_ok(&cp);
    let copies: serde_json::Value = serde_json::from_str(&cp.stdout).unwrap();
    let copy = copies.as_array().unwrap();
    assert_eq!(copy.len(), 1, "one copy op: {}", cp.stdout);
    assert!(op_endpoints(&copy[0]).contains(&rec_a), "copy references the source file");
    let from = copy[0]["fields"].as_array().unwrap().iter().find(|f| f["name"] == "plan_from");
    assert!(from.is_some(), "copy op records plan_from: {}", cp.stdout);
    // And a chmod op, from the same source side, to set the new file's mode.
    let chm =
        mf(&["-u", &plan, "metarecord", "-q", "plan_kind = \"chmod\"", "get", "--select", "*"]);
    assert_ok(&chm);
    let chmods: serde_json::Value = serde_json::from_str(&chm.stdout).unwrap();
    assert_eq!(chmods.as_array().unwrap().len(), 1, "one chmod op: {}", chm.stdout);
    let cfrom = chmods.as_array().unwrap()[0]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "plan_from");
    assert!(cfrom.is_some(), "chmod op records plan_from: {}", chm.stdout);

    let got = mf(&[
        "-u",
        &plan,
        "metarecord",
        "-q",
        "plan_kind = \"create-link\"",
        "get",
        "--select",
        "*",
    ]);
    assert_ok(&got);
    let ops: serde_json::Value = serde_json::from_str(&got.stdout).unwrap();
    let op = ops.as_array().unwrap()[0].clone();
    // One endpoint is the existing record; the other is a fresh bare UUID.
    let endpoints = op_endpoints(&op);
    assert!(endpoints.contains(&rec_a), "existing endpoint present: {endpoints:?}");
    let bare = endpoints.iter().find(|u| **u != rec_a).expect("a bare endpoint");
    assert!(is_hex_uuid(bare), "bare endpoint is a uuid: {bare}");
    // The bare side carries NO baseline (the record does not exist yet).
    let fields = op["fields"].as_array().unwrap();
    let bare_is_a = fields
        .iter()
        .any(|f| f["name"] == "plan_a" && f["value"]["value"]["metarecord"].as_str() == Some(bare));
    let bare_version_field = if bare_is_a { "plan_version_a" } else { "plan_version_b" };
    assert!(
        !fields.iter().any(|f| f["name"] == bare_version_field),
        "bare side must have no {bare_version_field}: {op}"
    );
    // The existing side does carry its baseline.
    let existing_version_field = if bare_is_a { "plan_version_b" } else { "plan_version_a" };
    assert!(
        fields.iter().any(|f| f["name"] == existing_version_field),
        "existing side keeps its baseline: {op}"
    );
}

#[test]
fn test_sync_plan_closes_over_no_identity_ref_target() {
    // A file X (in scope) refs an abstract, out-of-scope, identity-less record.
    let (a, _adir) = tracked_repo("clos_a", &[("doc.txt", b"x")]);
    let (b, _bdir) = tracked_repo("clos_b", &[("doc.txt", b"y")]);
    let x = query_one(&a, "mfr_path = \"doc.txt\"");
    let person = create_metarecord(&a, &["name:string=alice"]); // no tree_ref → no identity
    assert_ok(&mf(&[
        "-u",
        &a,
        "metarecord",
        "-i",
        &x,
        "field",
        "add",
        &format!("author:ref={person}"),
    ]));

    // Scope selects files only → `person` is out of scope.
    let intents = write_intents(
        "clos",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    let out = mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]);
    assert_ok(&out);
    let plan = plan_repo_uuid(&out);

    // Two links: X↔X_B (by path) and person↔bare (referential closure).
    let got = mf(&[
        "-u",
        &plan,
        "metarecord",
        "-q",
        "plan_kind = \"create-link\"",
        "get",
        "--select",
        "*",
    ]);
    assert_ok(&got);
    let ops: serde_json::Value = serde_json::from_str(&got.stdout).unwrap();
    let ops = ops.as_array().unwrap();
    assert_eq!(ops.len(), 2, "two create-link ops");
    // One op links `person` to a freshly allocated bare record.
    let person_op = ops
        .iter()
        .find(|o| op_endpoints(o).contains(&person))
        .expect("a link for the referenced record");
    let endpoints = op_endpoints(person_op);
    let bare = endpoints.iter().find(|u| **u != person).expect("bare counterpart");
    assert!(is_hex_uuid(bare) && *bare != person);
}

#[test]
fn test_sync_plan_case0_field_equality_links() {
    // No-identity records (no tree_ref) with equal fields link by the case-0
    // heuristic — the typical tombstone-style match.
    let (a, _ar) = init_repo("case0_a");
    let (b, _br) = init_repo("case0_b");
    let rec_a = create_metarecord(&a, &["tag:string=alice", "rating:int=5"]);
    let rec_b = create_metarecord(&b, &["tag:string=alice", "rating:int=5"]);
    // A distractor in B with different fields must not match.
    create_metarecord(&b, &["tag:string=bob"]);

    let intents =
        write_intents("case0", &format!("[[intents]]\nrepo = '{a}'\nquery = 'tag = \"alice\"'\n"));
    let out = mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]);
    assert_ok(&out);
    assert!(out.stdout.contains("operations: 1"), "one field-equality link: {}", out.stdout);
    let plan = plan_repo_uuid(&out);

    let got = mf(&[
        "-u",
        &plan,
        "metarecord",
        "-q",
        "plan_kind = \"create-link\"",
        "get",
        "--select",
        "*",
    ]);
    assert_ok(&got);
    let ops: serde_json::Value = serde_json::from_str(&got.stdout).unwrap();
    let endpoints = op_endpoints(&ops.as_array().unwrap()[0]);
    assert!(
        endpoints.contains(&rec_a) && endpoints.contains(&rec_b),
        "must link the two field-equal records: {endpoints:?}"
    );
}

#[test]
fn test_sync_plan_case0_ambiguous_stays_bare() {
    // Two identical B candidates → ambiguous → no field match → bare record.
    let (a, _ar) = init_repo("case0amb_a");
    let (b, _br) = init_repo("case0amb_b");
    let rec_a = create_metarecord(&a, &["tag:string=dup"]);
    create_metarecord(&b, &["tag:string=dup"]);
    create_metarecord(&b, &["tag:string=dup"]);

    let intents =
        write_intents("case0amb", &format!("[[intents]]\nrepo = '{a}'\nquery = 'tag = \"dup\"'\n"));
    let out = mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]);
    assert_ok(&out);
    // Bare link (ambiguous → no match) → create-link + sync.
    assert!(out.stdout.contains("operations: 2"), "create-link + sync: {}", out.stdout);
    let plan = plan_repo_uuid(&out);
    let got = mf(&[
        "-u",
        &plan,
        "metarecord",
        "-q",
        "plan_kind = \"create-link\"",
        "get",
        "--select",
        "*",
    ]);
    assert_ok(&got);
    let ops: serde_json::Value = serde_json::from_str(&got.stdout).unwrap();
    let endpoints = op_endpoints(&ops.as_array().unwrap()[0]);
    // rec_a is linked to a fresh bare record, not to either ambiguous candidate.
    assert!(endpoints.contains(&rec_a));
    let other = endpoints.iter().find(|u| **u != rec_a).unwrap();
    assert!(is_hex_uuid(other));
}

#[test]
fn test_sync_plan_writes_sync_op_on_field_diff() {
    // Same file (matched by path) but a user field on one side only → a `sync`
    // op propagates it, alongside the create-link.
    let (a, _adir) = tracked_repo("syncop_a", &[("doc.txt", b"same")]);
    let (b, _bdir) = tracked_repo("syncop_b", &[("doc.txt", b"same")]);
    let x = query_one(&a, "mfr_path = \"doc.txt\"");
    assert_ok(&mf(&["-u", &a, "metarecord", "-i", &x, "field", "add", "tag:string=jazz"]));

    let intents = write_intents(
        "syncop",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    let out = mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]);
    assert_ok(&out);
    assert!(out.stdout.contains("operations: 2"), "create-link + sync: {}", out.stdout);
    let plan = plan_repo_uuid(&out);

    // Exactly one sync op, referencing the linked pair.
    let got =
        mf(&["-u", &plan, "metarecord", "-q", "plan_kind = \"sync\"", "get", "--select", "*"]);
    assert_ok(&got);
    let ops: serde_json::Value = serde_json::from_str(&got.stdout).unwrap();
    assert_eq!(ops.as_array().unwrap().len(), 1, "one sync op: {}", got.stdout);
    assert!(op_endpoints(&ops.as_array().unwrap()[0]).contains(&x));
}

#[test]
fn test_sync_plan_no_sync_op_when_fields_equal() {
    // Matched files with no user-field difference → no sync op (only create-link).
    let (a, _adir) = tracked_repo("nosync_a", &[("doc.txt", b"aaa")]);
    let (b, _bdir) = tracked_repo("nosync_b", &[("doc.txt", b"aaa")]);
    let intents = write_intents(
        "nosync",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    let out = mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]);
    assert_ok(&out);
    assert!(out.stdout.contains("operations: 1"), "only create-link: {}", out.stdout);
    let plan = plan_repo_uuid(&out);
    let got = mf(&["-u", &plan, "metarecord", "-q", "plan_kind = \"sync\"", "get"]);
    assert_ok(&got);
    assert!(got.stdout.trim().is_empty(), "no sync op: {}", got.stdout);
}

#[test]
fn test_sync_plan_conflict_resolved_by_on_conflict() {
    // Matched files, same field with different values on each side → a conflict,
    // resolved non-interactively by --on-conflict prefer:<repo_a>.
    let (a, _adir) = tracked_repo("conf_a", &[("doc.txt", b"same")]);
    let (b, _bdir) = tracked_repo("conf_b", &[("doc.txt", b"same")]);
    let xa = query_one(&a, "mfr_path = \"doc.txt\"");
    let xb = query_one(&b, "mfr_path = \"doc.txt\"");
    assert_ok(&mf(&["-u", &a, "metarecord", "-i", &xa, "field", "add", "tag:string=jazz"]));
    assert_ok(&mf(&["-u", &b, "metarecord", "-i", &xb, "field", "add", "tag:string=rock"]));

    let intents = write_intents(
        "conf",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    let out = mf(&[
        "sync",
        "plan",
        &a,
        &b,
        "--intents",
        intents.to_str().unwrap(),
        "--on-conflict",
        &format!("prefer:{a}"),
    ]);
    assert_ok(&out);
    let plan = plan_repo_uuid(&out);

    // One conflict op on `tag`, resolved to a canonical side, with both values.
    let got =
        mf(&["-u", &plan, "metarecord", "-q", "plan_kind = \"conflict\"", "get", "--select", "*"]);
    assert_ok(&got);
    let ops: serde_json::Value = serde_json::from_str(&got.stdout).unwrap();
    let ops = ops.as_array().unwrap();
    assert_eq!(ops.len(), 1, "one conflict op: {}", got.stdout);
    let f = |name: &str| {
        ops[0]["fields"].as_array().unwrap().iter().find(|x| x["name"] == name).cloned()
    };
    assert_eq!(f("plan_field").unwrap()["value"]["value"], "tag");
    // prefer:repo_a → resolved to repo_a's canonical side (plan_a/plan_b are canonical).
    let expect_side = if a < b { "a" } else { "b" };
    let resolve = f("plan_resolve").unwrap()["value"]["value"].as_str().unwrap().to_string();
    assert_eq!(resolve, expect_side);
    // The resolved (repo_a) side holds "jazz"; the other holds "rock".
    let fields = ops[0]["fields"].as_array().unwrap();
    let val = |name: &str| {
        fields
            .iter()
            .filter(|x| x["name"] == name)
            .map(|x| x["value"]["value"].clone())
            .collect::<Vec<_>>()
    };
    let (resolved_field, other_field) = if resolve == "a" {
        ("plan_value_a", "plan_value_b")
    } else {
        ("plan_value_b", "plan_value_a")
    };
    assert_eq!(val(resolved_field), vec!["jazz"], "repo_a's value wins");
    assert_eq!(val(other_field), vec!["rock"]);
}

#[test]
fn test_sync_plan_resyncs_existing_link() {
    // A pre-existing link (as if from a prior sync) is re-synced, not recreated.
    let (a, _adir) = tracked_repo("resync_a", &[("doc.txt", b"same")]);
    let (b, _bdir) = tracked_repo("resync_b", &[("doc.txt", b"same")]);
    let xa = query_one(&a, "mfr_path = \"doc.txt\"");
    let xb = query_one(&b, "mfr_path = \"doc.txt\"");
    assert_ok(&mf(&["sync", "link", &a, &b, &xa, &xb]));
    // A user field appears on A only since the link was made.
    assert_ok(&mf(&["-u", &a, "metarecord", "-i", &xa, "field", "add", "tag:string=x"]));

    let intents = write_intents(
        "resync",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    let out = mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]);
    assert_ok(&out);
    let plan = plan_repo_uuid(&out);

    // No new link — the pair was already linked.
    let creates = mf(&["-u", &plan, "metarecord", "-q", "plan_kind = \"create-link\"", "get"]);
    assert_ok(&creates);
    assert!(creates.stdout.trim().is_empty(), "no create-link: {}", creates.stdout);
    // One re-sync op for the existing link.
    let syncs =
        mf(&["-u", &plan, "metarecord", "-q", "plan_kind = \"sync\"", "get", "--select", "*"]);
    assert_ok(&syncs);
    let ops: serde_json::Value = serde_json::from_str(&syncs.stdout).unwrap();
    assert_eq!(ops.as_array().unwrap().len(), 1, "one re-sync op: {}", syncs.stdout);
    assert!(op_endpoints(&ops.as_array().unwrap()[0]).contains(&xa));
}

#[test]
fn test_sync_plan_keeps_out_of_scope_link() {
    // An existing link whose endpoints are out of the current scope is left
    // untouched (persistent state) — never dropped.
    let (a, _adir) = tracked_repo("keep_a", &[("doc.txt", b"x")]);
    let (b, _bdir) = tracked_repo("keep_b", &[("doc.txt", b"x")]);
    let xa = query_one(&a, "mfr_path = \"doc.txt\"");
    let xb = query_one(&b, "mfr_path = \"doc.txt\"");
    let out = mf(&["sync", "link", &a, &b, &xa, &xb]);
    assert_ok(&out);
    let link = out.stdout.trim().to_string();

    // A scope that matches nothing → the doc.txt link is out of scope.
    let intents =
        write_intents("keep", &format!("[[intents]]\nrepo = '{a}'\nquery = 'tag = \"none\"'\n"));
    let out = mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]);
    assert_ok(&out);
    let plan = plan_repo_uuid(&out);

    // No drop-link op, and the link is still in the sync database.
    let drops = mf(&["-u", &plan, "metarecord", "-q", "plan_kind = \"drop-link\"", "get"]);
    assert_ok(&drops);
    assert!(drops.stdout.trim().is_empty(), "no drop-link op: {}", drops.stdout);
    let status = mf(&["sync", "status", &a, &b]);
    assert_ok(&status);
    assert!(status.stdout.contains(&link), "the link is kept: {}", status.stdout);
}

#[test]
fn test_sync_plan_move_op_on_diverged_path() {
    // Two linked files whose positions diverge (a.txt ↔ b.txt) → a move op. This
    // is the state after a rename on one side, or a manual cross-path link.
    let (a, _adir) = tracked_repo("move_a", &[("a.txt", b"content")]);
    let (b, _bdir) = tracked_repo("move_b", &[("b.txt", b"content")]);
    let xa = query_one(&a, "mfr_path = \"a.txt\"");
    let xb = query_one(&b, "mfr_path = \"b.txt\"");
    assert_ok(&mf(&["sync", "link", &a, &b, &xa, &xb]));

    let intents = write_intents(
        "move",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    let out = mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]);
    assert_ok(&out);
    let plan = plan_repo_uuid(&out);

    let got =
        mf(&["-u", &plan, "metarecord", "-q", "plan_kind = \"move\"", "get", "--select", "*"]);
    assert_ok(&got);
    let ops: serde_json::Value = serde_json::from_str(&got.stdout).unwrap();
    assert_eq!(ops.as_array().unwrap().len(), 1, "one move op: {}", got.stdout);
    assert!(op_endpoints(&ops.as_array().unwrap()[0]).contains(&xa));
}

#[test]
fn test_sync_plan_delete_op_on_deleted_endpoint() {
    // A linked record deleted on side A → a delete op removing the surviving B.
    let (a, _adir) = tracked_repo("del_a", &[("doc.txt", b"x")]);
    let (b, _bdir) = tracked_repo("del_b", &[("doc.txt", b"x")]);
    let xa = query_one(&a, "mfr_path = \"doc.txt\"");
    let xb = query_one(&b, "mfr_path = \"doc.txt\"");
    assert_ok(&mf(&["sync", "link", &a, &b, &xa, &xb]));
    // Delete A's metarecord.
    assert_ok(&mf(&["-u", &a, "metarecord", "-i", &xa, "delete"]));

    // Scope selects the surviving side (B).
    let intents = write_intents(
        "del",
        &format!("[[intents]]\nrepo = '{b}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    let out = mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]);
    assert_ok(&out);
    let plan = plan_repo_uuid(&out);

    let got =
        mf(&["-u", &plan, "metarecord", "-q", "plan_kind = \"delete\"", "get", "--select", "*"]);
    assert_ok(&got);
    let ops: serde_json::Value = serde_json::from_str(&got.stdout).unwrap();
    let ops = ops.as_array().unwrap();
    assert_eq!(ops.len(), 1, "one delete op: {}", got.stdout);
    // plan_side names the surviving side (repo b, where xb lives) in canonical terms.
    let expect_side = if a < b { "b" } else { "a" };
    let side =
        ops[0]["fields"].as_array().unwrap().iter().find(|f| f["name"] == "plan_side").unwrap();
    assert_eq!(side["value"]["value"], expect_side);
    assert!(op_endpoints(&ops[0]).contains(&xb), "references the survivor");
}

#[test]
fn test_sync_plan_conflict_query_scoped_rule() {
    // A [[conflict]] rule scoped by a query (matching one endpoint) resolves the
    // conflict without --on-conflict.
    let (a, _adir) = tracked_repo("cq_a", &[("doc.txt", b"same")]);
    let (b, _bdir) = tracked_repo("cq_b", &[("doc.txt", b"same")]);
    let xa = query_one(&a, "mfr_path = \"doc.txt\"");
    let xb = query_one(&b, "mfr_path = \"doc.txt\"");
    assert_ok(&mf(&["-u", &a, "metarecord", "-i", &xa, "field", "add", "tag:string=jazz"]));
    assert_ok(&mf(&["-u", &b, "metarecord", "-i", &xb, "field", "add", "tag:string=rock"]));

    // Rule: for records matching `tag = "jazz"` (A's side), prefer repo A.
    let content = format!(
        "[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n\n[[conflict]]\nquery = 'tag = \"jazz\"'\npolicy = 'prefer:{a}'\n"
    );
    let intents = write_intents("cq", &content);
    let out = mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]);
    assert_ok(&out);
    let plan = plan_repo_uuid(&out);

    let got =
        mf(&["-u", &plan, "metarecord", "-q", "plan_kind = \"conflict\"", "get", "--select", "*"]);
    assert_ok(&got);
    let ops: serde_json::Value = serde_json::from_str(&got.stdout).unwrap();
    let ops = ops.as_array().unwrap();
    assert_eq!(ops.len(), 1, "one conflict op: {}", got.stdout);
    // Resolved to repo_a's canonical side (which holds "jazz").
    let expect_side = if a < b { "a" } else { "b" };
    let f = |name: &str| {
        ops[0]["fields"].as_array().unwrap().iter().find(|x| x["name"] == name).cloned()
    };
    assert_eq!(f("plan_resolve").unwrap()["value"]["value"], expect_side);
}

#[test]
fn test_sync_run_creates_file_in_target() {
    // plan then run: a file present only in A is materialised in B (record + bytes).
    let (a, _adir) = tracked_repo("run_a", &[("hello.txt", b"world")]);
    let (b, _bdir) = tracked_repo("run_b", &[]);
    let intents = write_intents(
        "run",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    assert_ok(&mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]));

    let out = mf(&["sync", "run", &a, &b, "--yes"]);
    assert_ok(&out);

    // B now has a metarecord at hello.txt …
    let rec_b = query_one(&b, "mfr_path = \"hello.txt\"");
    assert!(is_hex_uuid(&rec_b), "B has a record at hello.txt: {rec_b}");
    // … and the file on disk with the right content.
    let root_b = repo_root_of(&b);
    let content = std::fs::read(root_b.join("hello.txt")).expect("file exists in B");
    assert_eq!(content, b"world", "content transferred");

    // The link is now in sync, and a second run does nothing.
    let status = mf(&["sync", "status", &a, &b]);
    assert!(status.stdout.contains("in_sync"), "link in sync: {}", status.stdout);
    let again = mf(&["sync", "run", &a, &b, "--yes"]);
    assert_ok(&again);
    assert!(again.stdout.contains("nothing to run"), "second run is a no-op: {}", again.stdout);
}

/// The filesystem root of a loaded repo (from `mf repo list`).
fn repo_root_of(repo: &str) -> PathBuf {
    let list = mf(&["repo", "list", "--all"]);
    assert_ok(&list);
    let repos: serde_json::Value = serde_json::from_str(&list.stdout).unwrap();
    let root = repos
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["repo_uuid"] == repo)
        .and_then(|r| r["root"].as_str())
        .expect("repo root");
    PathBuf::from(root)
}

#[test]
fn test_sync_run_propagates_deletion() {
    // A linked record deleted in A → run trashes B's file and removes B's record
    // and the link. Nothing is destroyed (the file lands in B's trash).
    let (a, _adir) = tracked_repo("rundel_a", &[("doc.txt", b"x")]);
    let (b, _bdir) = tracked_repo("rundel_b", &[("doc.txt", b"x")]);
    let xa = query_one(&a, "mfr_path = \"doc.txt\"");
    let xb = query_one(&b, "mfr_path = \"doc.txt\"");
    assert_ok(&mf(&["sync", "link", &a, &b, &xa, &xb]));
    assert_ok(&mf(&["-u", &a, "metarecord", "-i", &xa, "delete"]));

    let intents = write_intents(
        "rundel",
        &format!("[[intents]]\nrepo = '{b}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    assert_ok(&mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]));
    assert_ok(&mf(&["sync", "run", &a, &b, "--yes"]));

    // B's record is gone …
    let got = mf(&["-u", &b, "metarecord", "-i", &xb, "get"]);
    assert_eq!(got.code, 1, "B's record deleted: {}{}", got.stdout, got.stderr);
    // … the file is off disk but in the trash (not destroyed) …
    let root_b = repo_root_of(&b);
    assert!(!root_b.join("doc.txt").exists(), "file removed from B");
    let trash = mf(&["-u", &b, "trash", "list"]);
    assert_ok(&trash);
    assert!(trash.stdout.contains("sync"), "file is in the trash (reason sync): {}", trash.stdout);
    // … and the link is gone.
    let status = mf(&["sync", "status", &a, &b]);
    assert!(status.stderr.contains("no links") || status.stdout.trim().is_empty(), "link removed");
}

#[test]
fn test_sync_run_resync_propagates_field() {
    // First sync links the pair and commits a snapshot; then a field added on A
    // propagates to B on the next plan+run (re-sync direction).
    let (a, _adir) = tracked_repo("rerun_a", &[("doc.txt", b"x")]);
    let (b, _bdir) = tracked_repo("rerun_b", &[("doc.txt", b"x")]);
    let intents = write_intents(
        "rerun",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    assert_ok(&mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]));
    assert_ok(&mf(&["sync", "run", &a, &b, "--yes"]));

    // Add a field on A, re-plan, re-run.
    let xa = query_one(&a, "mfr_path = \"doc.txt\"");
    assert_ok(&mf(&["-u", &a, "metarecord", "-i", &xa, "field", "add", "tag:string=jazz"]));
    assert_ok(&mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]));
    assert_ok(&mf(&["sync", "run", &a, &b, "--yes"]));

    // B's record now carries tag=jazz.
    let xb = query_one(&b, "mfr_path = \"doc.txt\"");
    let got = mf(&["-u", &b, "metarecord", "-i", &xb, "get", "--select", "*"]);
    assert_ok(&got);
    let m: serde_json::Value = serde_json::from_str(&got.stdout).unwrap();
    let tag = m[0]["fields"].as_array().unwrap().iter().find(|f| f["name"] == "tag");
    assert_eq!(
        tag.and_then(|f| f["value"]["value"].as_str()),
        Some("jazz"),
        "tag propagated: {}",
        got.stdout
    );
}

/// A record's first value for `field` (as a string), or None.
fn field_value_of(repo: &str, uuid: &str, field: &str) -> Option<String> {
    let got = mf(&["-u", repo, "metarecord", "-i", uuid, "get", "--select", "*"]);
    assert_ok(&got);
    let m: serde_json::Value = serde_json::from_str(&got.stdout).ok()?;
    m[0]["fields"]
        .as_array()?
        .iter()
        .find(|f| f["name"] == field)
        .and_then(|f| f["value"]["value"].as_str())
        .map(String::from)
}

#[test]
fn test_sync_run_applies_conflict_resolution() {
    // After a first sync, both sides change the same field → conflict resolved by
    // --on-conflict prefer:<repo_a>, so repo_a's value wins on both sides at run.
    let (a, _adir) = tracked_repo("cfr_a", &[("doc.txt", b"x")]);
    let (b, _bdir) = tracked_repo("cfr_b", &[("doc.txt", b"x")]);
    let intents = write_intents(
        "cfr",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    assert_ok(&mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]));
    assert_ok(&mf(&["sync", "run", &a, &b, "--yes"]));

    let xa = query_one(&a, "mfr_path = \"doc.txt\"");
    let xb = query_one(&b, "mfr_path = \"doc.txt\"");
    assert_ok(&mf(&["-u", &a, "metarecord", "-i", &xa, "field", "add", "tag:string=jazz"]));
    assert_ok(&mf(&["-u", &b, "metarecord", "-i", &xb, "field", "add", "tag:string=rock"]));

    assert_ok(&mf(&[
        "sync",
        "plan",
        &a,
        &b,
        "--intents",
        intents.to_str().unwrap(),
        "--on-conflict",
        &format!("prefer:{a}"),
    ]));
    assert_ok(&mf(&["sync", "run", &a, &b, "--yes"]));

    // repo_a's value (jazz) wins on both sides.
    assert_eq!(field_value_of(&a, &xa, "tag").as_deref(), Some("jazz"), "A keeps jazz");
    assert_eq!(field_value_of(&b, &xb, "tag").as_deref(), Some("jazz"), "B took jazz");
}

#[test]
fn test_sync_run_external_divergence_reported() {
    // A file to create where the target subtree is external → metafolder writes no
    // file; the metadata still syncs and the divergence is reported.
    let (a, _adir) = tracked_repo("ext_a", &[("doc.txt", b"aaa")]);
    let (b, _bdir) = tracked_repo("ext_b", &[]);
    let b_root = mf(&["-u", &b, "metarecord", "get"]).stdout.trim().to_string();
    assert_ok(&mf(&[
        "-u",
        &b,
        "metarecord",
        "-i",
        &b_root,
        "field",
        "set",
        "mf_sync:string=external",
        "--force",
    ]));

    let intents = write_intents(
        "ext",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    assert_ok(&mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]));
    let out = mf(&["sync", "run", &a, &b, "--yes"]);
    assert_ok(&out);

    // The divergence is reported (aggregated), and no file was written in B …
    assert!(out.stderr.contains("external divergences"), "reported: {}", out.stderr);
    assert!(!repo_root_of(&b).join("doc.txt").exists(), "external file not created by metafolder");
    // … but the metadata record was placed (metadata syncs normally).
    let rec_b = query_one(&b, "mfr_path = \"doc.txt\"");
    assert!(is_hex_uuid(&rec_b), "record placed in B: {rec_b}");
}

#[test]
fn test_sync_run_translates_ref() {
    // A file X refs an abstract record; both are materialised in B and X_B's ref
    // is translated to person_B (the linked counterpart), not left dangling.
    let (a, _adir) = tracked_repo("tref_a", &[("doc.txt", b"x")]);
    let (b, _bdir) = tracked_repo("tref_b", &[]);
    let xa = query_one(&a, "mfr_path = \"doc.txt\"");
    let person_a = create_metarecord(&a, &["name:string=alice"]);
    assert_ok(&mf(&[
        "-u",
        &a,
        "metarecord",
        "-i",
        &xa,
        "field",
        "add",
        &format!("author:ref={person_a}"),
    ]));

    let intents = write_intents(
        "tref",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    assert_ok(&mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]));
    assert_ok(&mf(&["sync", "run", &a, &b, "--yes"]));

    // B has person (name=alice) and X_B whose author ref points to it.
    let person_b = query_one(&b, r#"name = "alice""#);
    let xb = query_one(&b, "mfr_path = \"doc.txt\"");
    let author = field_value_of(&b, &xb, "author");
    assert_eq!(author.as_deref(), Some(person_b.as_str()), "author ref translated to person_b");
    assert_ne!(person_b, person_a, "distinct local uuids");
}

#[test]
fn test_sync_does_not_materialise_a_duplicate_group() {
    // `mfr_duplicate_group` is content-derived, so the metadata diff never
    // writes it and each repository computes its own groups. The referential
    // closure must agree: closing over it would plant a bare, empty group
    // record in B for no purpose (spec-duplicates "Cross-repo sync").
    let (a, _adir) = tracked_repo("dupclosure_a", &[("doc.txt", b"x")]);
    let (b, _bdir) = tracked_repo("dupclosure_b", &[]);
    let xa = query_one(&a, "mfr_path = \"doc.txt\"");
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

    let xb = query_one(&b, "mfr_path = \"doc.txt\"");
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
    // Two files linked across different paths (a.txt ↔ b.txt) — the state after a
    // rename. Never synced → A wins; run moves the canonical-B file and record to
    // the canonical-A path. Nothing is destroyed.
    let (a, _adir) = tracked_repo("mvrun_a", &[("a.txt", b"content")]);
    let (b, _bdir) = tracked_repo("mvrun_b", &[("b.txt", b"content")]);
    let xa = query_one(&a, "mfr_path = \"a.txt\"");
    let xb = query_one(&b, "mfr_path = \"b.txt\"");
    assert_ok(&mf(&["sync", "link", &a, &b, &xa, &xb]));

    let intents = write_intents(
        "mvrun",
        &format!("[[intents]]\nrepo = '{a}'\nquery = 'mfr_type = \"file\"'\n"),
    );
    assert_ok(&mf(&["sync", "plan", &a, &b, "--intents", intents.to_str().unwrap()]));
    assert_ok(&mf(&["sync", "run", &a, &b, "--yes"]));

    // The winner is the canonical-A record's path; both records/files converge there.
    let winner = if a < b { "a.txt" } else { "b.txt" };
    assert_eq!(query_one(&a, &format!("mfr_path = \"{winner}\"")), xa, "A at winner path");
    assert_eq!(query_one(&b, &format!("mfr_path = \"{winner}\"")), xb, "B moved to winner path");
    assert!(repo_root_of(&a).join(winner).exists(), "A file at winner path");
    assert!(repo_root_of(&b).join(winner).exists(), "B file at winner path");
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
    let xa = query_one(&a, "mfr_path = \"doc.txt\"");
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

// ── mf order (folder child numbering) ─────────────────────────────────────────

#[test]
fn test_order_numbers_folder_children() {
    let (repo, root) = init_repo("order");
    let album = root.join("album");
    std::fs::create_dir_all(album.join("extra")).unwrap();
    for f in ["song0.avi", "song1.avi", "song3.avi", "README.md"] {
        std::fs::write(album.join(f), b"x").unwrap();
    }
    // Track each child (creates the metarecord + ancestor chain, with mfr_type).
    let track = |p: std::path::PathBuf| {
        let out = mf(&["-u", &repo, "track", p.to_str().unwrap()]);
        assert_ok(&out);
        out.stdout.trim().to_string()
    };
    let album_uuid = track(album.clone());
    let song0 = track(album.join("song0.avi"));
    let song1 = track(album.join("song1.avi"));
    let song3 = track(album.join("song3.avi"));
    let readme = track(album.join("README.md"));
    let extra = track(album.join("extra"));

    // song0 carries an ordering metadata (a custom field, so no reserved-force).
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &song0, "field", "set", "track_no:int=3"]));

    let out = mf(&["-u", &repo, "order", album.to_str().unwrap(), "--meta", "track_no"]);
    assert_ok(&out);

    let pos = |uuid: &str, field: &str| -> String {
        let out = mf(&["-u", &repo, "metarecord", "-i", uuid, "field", "get", field]);
        assert_ok(&out);
        out.stdout.trim().to_string()
    };
    // song0 pinned by its metadata (anchor), then the "song*.avi" name cluster,
    // then README by date; the sub-directory is numbered separately.
    assert_eq!(pos(&song0, "order_position_file"), "1");
    assert_eq!(pos(&song1, "order_position_file"), "2");
    assert_eq!(pos(&song3, "order_position_file"), "4");
    assert_eq!(pos(&readme, "order_position_file"), "5");
    assert_eq!(pos(&extra, "order_position_dir"), "1");

    // The folder itself is marked as numbered, so processed folders can be told
    // apart from untreated ones.
    assert_eq!(pos(&album_uuid, "order_numbered"), "true");

    // A second run never overwrites an existing position: nothing is written.
    let again = mf(&["-u", &repo, "order", album.to_str().unwrap(), "--meta", "track_no"]);
    assert_ok(&again);
    assert_eq!(again.stdout.trim(), "0", "second run must write nothing");
    assert_eq!(pos(&song0, "order_position_file"), "1");
    assert_eq!(pos(&album_uuid, "order_numbered"), "true", "the marker survives a re-run");
}

// ── CLI primitives: --eq, --tsv, --resolve ────────────────────────────────────

#[test]
fn test_cli_primitives_eq_tsv_resolve() {
    let (repo, _root) = init_repo("prim");
    let coltrane =
        create_metarecord(&repo, &["type:string=person", "name:string=Coltrane", "lead:bool=true"]);
    let davis = create_metarecord(&repo, &["type:string=person", "name:string=Davis"]);
    let rec = create_metarecord(
        &repo,
        &[&format!("author:ref={coltrane}"), &format!("author:ref={davis}")],
    );

    // --eq: safe exact match (no DSL interpolation) → the Coltrane uuid only.
    let out =
        mf(&["-u", &repo, "metarecord", "--eq", "type=person", "--eq", "name=Coltrane", "get"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), coltrane);

    // --tsv: name<TAB>lead, one row per person (absent field = empty).
    let out = mf(&[
        "-u",
        &repo,
        "metarecord",
        "--eq",
        "type=person",
        "get",
        "--select",
        "name,lead",
        "--tsv",
    ]);
    assert_ok(&out);
    let mut lines: Vec<&str> = out.stdout.lines().collect();
    lines.sort();
    assert_eq!(lines, vec!["Coltrane\ttrue", "Davis\t"]);

    // --resolve on a string target field: the `author` refs → their `name`, in
    // one round-trip (the tree-aware path form is covered by the tag test).
    let out =
        mf(&["-u", &repo, "metarecord", "-i", &rec, "field", "get", "author", "--resolve", "name"]);
    assert_ok(&out);
    let mut names: Vec<&str> = out.stdout.lines().collect();
    names.sort();
    assert_eq!(names, vec!["Coltrane", "Davis"]);
}

#[test]
fn test_metarecord_get_resolve_tree_lists_paths() {
    let (repo, root) = init_repo("resolve_tree");
    std::fs::create_dir_all(root.join("a/b")).unwrap();
    std::fs::write(root.join("a/f.txt"), b"x").unwrap();
    // Track the two directories and one file (track builds the parent chain).
    assert_ok(&mf(&["-u", &repo, "track", root.join("a").to_str().unwrap()]));
    assert_ok(&mf(&["-u", &repo, "track", root.join("a/b").to_str().unwrap()]));
    let f = mf(&["-u", &repo, "track", root.join("a/f.txt").to_str().unwrap()]);
    assert_ok(&f);
    let f_uuid = f.stdout.trim().to_string();

    // Bulk (query selector): every directory's mfr_path resolved to its
    // repo-root-relative path, one per line (leading-"/"-rooted, the DSL form —
    // paste-able straight into an `mfr_path -> "…"` query) — the one round-trip
    // that lets a GUI script offer folder completions.
    let out = mf(&[
        "-u",
        &repo,
        "metarecord",
        "-q",
        "mfr_type = \"dir\"",
        "get",
        "--resolve-tree",
        "mfr_path",
    ]);
    assert_ok(&out);
    let lines: Vec<&str> = out.stdout.lines().collect();
    assert!(lines.contains(&"/a"), "got: {}", out.stdout);
    assert!(lines.contains(&"/a/b"), "got: {}", out.stdout);

    // Direct selector (-i): the single record's resolved path.
    let out = mf(&["-u", &repo, "metarecord", "-i", &f_uuid, "get", "--resolve-tree", "mfr_path"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "/a/f.txt");

    // No selector is a usage error (exit 2, no HTTP round-trip).
    let out = mf(&["-u", &repo, "metarecord", "get", "--resolve-tree", "mfr_path"]);
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
}

// ── mf tag (hierarchical tags: subsumption, exclusivity) ──────────────────────

#[test]
fn test_mf_tag_subsumption_exclusivity_deny_list() {
    let (repo, _root) = init_repo("tag");
    let rec = create_metarecord(&repo, &["note:string=target"]);
    // Vocabulary as a TreeRef forest on `path`: each tag is a node whose parent
    // is another tag entry. jazz is exclusive among musique's children.
    let mk_tag = |specs: &[&str]| create_metarecord(&repo, specs);
    let musique = mk_tag(&["mf_schema:string=tag", "path:tree_ref=/musique"]);
    let jazz = mk_tag(&[
        "mf_schema:string=tag",
        format!("path:tree_ref={musique}/jazz").as_str(),
        "exclusive:bool=true",
    ]);
    let _rock = mk_tag(&["mf_schema:string=tag", format!("path:tree_ref={musique}/rock").as_str()]);
    let _bebop = mk_tag(&["mf_schema:string=tag", format!("path:tree_ref={jazz}/bebop").as_str()]);
    let admin = mk_tag(&["mf_schema:string=tag", "path:tree_ref=/administratif"]);
    let _impots =
        mk_tag(&["mf_schema:string=tag", format!("path:tree_ref={admin}/impots").as_str()]);

    let tag = |args: &[&str]| {
        let mut v: Vec<&str> = vec!["-u", repo.as_str(), "tag"];
        v.extend_from_slice(args);
        assert_ok(&mf(&v));
    };
    // Read the record's tag refs as their resolved hierarchy paths (ref → tag →
    // `path` TreeRef, resolved by the tree-aware `--resolve`).
    let names = |field: &str| -> Vec<String> {
        let out = mf(&[
            "-u",
            &repo,
            "metarecord",
            "-i",
            &rec,
            "field",
            "get",
            field,
            "--resolve",
            "path",
        ]);
        assert_ok(&out);
        let mut v: Vec<String> = out.stdout.lines().map(String::from).collect();
        v.sort();
        v
    };

    tag(&["-i", &rec, "add", "musique/rock"]);
    assert_eq!(names("tag"), vec!["musique/rock"]);

    // jazz is exclusive → adding it drops the sibling rock.
    tag(&["-i", &rec, "add", "musique/jazz"]);
    assert_eq!(names("tag"), vec!["musique/jazz"]);

    // bebop's ancestor jazz is present → dropped on add; add is idempotent.
    tag(&["-i", &rec, "add", "musique/jazz/bebop"]);
    tag(&["-i", &rec, "add", "musique/jazz/bebop"]);
    assert_eq!(names("tag"), vec!["musique/jazz/bebop"]);

    // A path absent from the vocabulary is auto-created as a node chain
    // (cinema → cinema/thriller) by `ensure_tag_entry`. It is unrelated to bebop,
    // so both tags coexist (add only drops ancestors and exclusive siblings).
    tag(&["-i", &rec, "add", "cinema/thriller"]);
    assert_eq!(names("tag"), vec!["cinema/thriller", "musique/jazz/bebop"]);

    // deny: a generic negative subsumes (drops) its specific descendant.
    tag(&["-i", &rec, "deny", "administratif/impots"]);
    assert_eq!(names("negative_tag"), vec!["administratif/impots"]);
    tag(&["-i", &rec, "deny", "administratif"]);
    assert_eq!(names("negative_tag"), vec!["administratif"]);

    // list = the vocabulary TSV with 0/1 flags (auto-created cinema tags present).
    let out = mf(&["-u", &repo, "tag", "list"]);
    assert_ok(&out);
    assert!(out.stdout.lines().any(|l| l == "musique/jazz\t0\t1"), "list:\n{}", out.stdout);
    assert!(out.stdout.lines().any(|l| l == "administratif\t0\t0"), "list:\n{}", out.stdout);
    assert!(out.stdout.lines().any(|l| l == "cinema/thriller\t0\t0"), "list:\n{}", out.stdout);
}

#[test]
fn test_watch_status_pause_and_resume() {
    let (repo, _root) = init_repo("watchpause");

    // A freshly loaded repository ingests filesystem events.
    let out = mf(&["-u", &repo, "watch"]);
    assert_ok(&out);
    assert!(out.stdout.starts_with("running"), "stdout: {}", out.stdout);
    assert!(out.stdout.contains("0 event(s) waiting"), "stdout: {}", out.stdout);

    let out = mf(&["-u", &repo, "watch", "pause"]);
    assert_ok(&out);
    assert!(out.stdout.starts_with("paused"), "stdout: {}", out.stdout);

    // The pause is repository state, not a property of the call that set it.
    let out = mf(&["-u", &repo, "watch", "status", "--json"]);
    assert_ok(&out);
    assert!(out.stdout.contains("\"paused\": true"), "stdout: {}", out.stdout);

    let out = mf(&["-u", &repo, "watch", "resume"]);
    assert_ok(&out);
    assert!(out.stdout.starts_with("running"), "stdout: {}", out.stdout);
}

#[test]
fn test_mount_list_and_forget() {
    let (repo, root) = init_repo("mount");
    let dir = root.join("photos");
    std::fs::create_dir(&dir).unwrap();
    let uuid = mf(&["-u", &repo, "track", dir.to_str().unwrap()]).stdout.trim().to_string();
    assert!(is_hex_uuid(&uuid));

    // A repository with no removable volume declares no mount point.
    let out = mf(&["-u", &repo, "mount"]);
    assert_ok(&out);
    assert!(out.stdout.contains("No mount points"), "stdout: {}", out.stdout);

    // Declare one by hand (an mfr_* write, hence --force): the directory is an
    // ordinary one, so it is exactly what an unplugged volume looks like.
    let out = mf(&[
        "-u",
        &repo,
        "metarecord",
        "-i",
        &uuid,
        "field",
        "add",
        "mfr_mount:string=label:PHOTOS",
        "--force",
    ]);
    assert_ok(&out);

    let out = mf(&["-u", &repo, "mount", "list"]);
    assert_ok(&out);
    assert!(out.stdout.contains("offline"), "stdout: {}", out.stdout);
    assert!(out.stdout.contains("/photos"), "stdout: {}", out.stdout);
    assert!(out.stdout.contains("label:PHOTOS"), "stdout: {}", out.stdout);

    // `forget` un-declares it: the directory becomes an ordinary one again.
    let out = mf(&["-u", &repo, "mount", "forget", &uuid, "-y"]);
    assert_ok(&out);
    let out = mf(&["-u", &repo, "mount"]);
    assert_ok(&out);
    assert!(out.stdout.contains("No mount points"), "stdout: {}", out.stdout);
}
