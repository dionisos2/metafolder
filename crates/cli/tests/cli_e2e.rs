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

/// `XDG_CONFIG_HOME` for a config dir with the shipped grammar installed at
/// `metafolder/core/query-grammar`, so `mf query --simplified` can expand
/// locally (the daemon no longer does).
fn config_xdg() -> &'static str {
    use std::sync::OnceLock;
    static XDG: OnceLock<String> = OnceLock::new();
    XDG.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("mf-cli-cfg-{}", std::process::id()));
        let core = dir.join("metafolder").join("core");
        std::fs::create_dir_all(&core).unwrap();
        std::fs::copy(
            concat!(env!("CARGO_MANIFEST_DIR"), "/../core/default-config/query-grammar"),
            core.join("query-grammar"),
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

fn temp_dir(prefix: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("metafolder_cli_{prefix}_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&path).unwrap();
    path
}

/// Initialises a fresh repository; returns (repo uuid, root path).
fn init_repo(prefix: &str) -> (String, PathBuf) {
    let root = temp_dir(prefix);
    let out = mf(&["repo", "init", root.to_str().unwrap()]);
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
    let out =
        mf(&["repo", "init", root.to_str().unwrap(), "--metafolder", external.to_str().unwrap()]);
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
    let out =
        mf(&["repo", "init", root.to_str().unwrap(), "--metafolder", external.to_str().unwrap()]);
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
    let (repo, _) = init_repo("repos");
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
fn xdg_with_cli_config(contents: &str) -> String {
    let dir = temp_dir("cli_cfg");
    let cli = dir.join("metafolder").join("cli");
    std::fs::create_dir_all(&cli).unwrap();
    std::fs::write(cli.join("config.toml"), contents).unwrap();
    dir.to_str().unwrap().to_string()
}

#[test]
fn test_config_default_repo_used_when_no_selector() {
    let (uuid, _root) = init_repo("cfgrepo");
    let xdg = xdg_with_cli_config(&format!("[repo]\nuuid = \"{uuid}\"\n"));
    // No -u/-n: the selector comes from the config's default [repo].
    let out = mf_full(&["metarecord", "get"], None, &[("XDG_CONFIG_HOME", &xdg)], true);
    assert_ok(&out);
}

#[test]
fn test_no_config_ignores_the_default_repo() {
    let (uuid, _root) = init_repo("cfgrepo_noconf");
    let xdg = xdg_with_cli_config(&format!("[repo]\nuuid = \"{uuid}\"\n"));
    // --no-config skips the file, so there is no selector → usage error (exit 2).
    let out = mf_full(
        &["--no-config", "metarecord", "get"],
        None,
        &[("XDG_CONFIG_HOME", &xdg)],
        true,
    );
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
}

#[test]
fn test_explicit_selector_overrides_the_config_default_repo() {
    let (uuid, _root) = init_repo("cfgrepo_override");
    // The config points at a bogus repo; an explicit -u must still win.
    let xdg = xdg_with_cli_config("[repo]\nname = \"does-not-exist\"\n");
    let out = mf_full(
        &["-u", &uuid, "metarecord", "get"],
        None,
        &[("XDG_CONFIG_HOME", &xdg)],
        true,
    );
    assert_ok(&out);
}

#[test]
fn test_malformed_config_is_usage_error_without_contacting_daemon() {
    let xdg = xdg_with_cli_config("this is = not = valid toml");
    // Exit 2 before any round-trip, even against an unreachable daemon.
    let out = mf_full(&["-p", "1", "repo", "list"], None, &[("XDG_CONFIG_HOME", &xdg)], false);
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
    assert!(out.stderr.contains("config.toml"), "stderr: {}", out.stderr);
}

#[test]
fn test_env_variables_are_honoured() {
    let (repo, _) = init_repo("env");
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
    let (repo, _) = init_repo("daemon_err");
    let missing = "00000000000000000000000000000099";
    let out = mf(&["-u", &repo, "metarecord", "-i", missing, "get"]);
    assert_eq!(out.code, 1);
    assert!(out.stderr.starts_with("error:"), "stderr: {}", out.stderr);
    assert!(out.stdout.is_empty());
}

// ── Entry manipulation ────────────────────────────────────────────────────────

#[test]
fn test_create_and_get_by_uuid() {
    let (repo, _) = init_repo("create");
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
    let (repo, _) = init_repo("create_force");
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
    let (repo, _) = init_repo("get_fields");
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
    let (repo, _) = init_repo("get_pred");
    let jazz = create_metarecord(&repo, &["genre:string=jazz"]);
    let _rock = create_metarecord(&repo, &["genre:string=rock"]);
    let entries = get_entries(&repo, r#"genre = "jazz""#);
    let list = entries.as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["uuid"], serde_json::json!(jazz));
}

#[test]
fn test_get_predicate_with_limit_and_sort() {
    let (repo, _) = init_repo("get_limit_sort");
    create_metarecord(&repo, &["rating:int=1"]);
    create_metarecord(&repo, &["rating:int=2"]);
    create_metarecord(&repo, &["rating:int=3"]);

    let out = mf(&["-u", &repo, "metarecord", "-q", "rating >= 1", "get", "--select", "*", "--sort", "rating:desc", "--limit", "2"]);
    assert_ok(&out);
    let list: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    let arr = list.as_array().unwrap();
    assert_eq!(arr.len(), 2, "--limit must cap the result at 2");

    let rating = |m: &serde_json::Value| -> i64 {
        m["fields"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["name"] == "rating")
            .unwrap()["value"]["value"]
            .as_i64()
            .unwrap()
    };
    // --sort rating:desc → the two highest, in order.
    assert_eq!(rating(&arr[0]), 3);
    assert_eq!(rating(&arr[1]), 2);
}

#[test]
fn test_list_prints_uuids_one_per_line() {
    let (repo, _) = init_repo("list");
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
    let (repo, _) = init_repo("set");
    let uuid = create_metarecord(&repo, &["tag:string=a", "tag:string=b"]);
    let out = mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "set", "tag:string=c"]);
    assert_ok(&out);
    let entries = get_entries(&repo, &uuid);
    let fields = entries[0]["fields"].as_array().unwrap();
    let tags: Vec<&serde_json::Value> =
        fields.iter().filter(|f| f["name"] == "tag").collect();
    assert_eq!(tags.len(), 1, "set_field must replace all rows of the name");
    assert_eq!(tags[0]["value"]["value"], "c");
}

#[test]
fn test_set_with_predicate_prints_updated_count() {
    let (repo, _) = init_repo("set_pred");
    create_metarecord(&repo, &["genre:string=jazz"]);
    create_metarecord(&repo, &["genre:string=jazz"]);
    create_metarecord(&repo, &["genre:string=rock"]);
    let out = mf(&["-u", &repo, "metarecord", "-q", r#"genre = "jazz""#, "field", "set", "rating:int=4"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "2");
    let out = mf(&["-u", &repo, "metarecord", "-q", "rating = 4", "get"]);
    assert_ok(&out);
    assert_eq!(out.stdout.lines().count(), 2);
}

#[test]
fn test_set_reserved_field_requires_force() {
    let (repo, _) = init_repo("set_force");
    let uuid = create_metarecord(&repo, &["x:int=1"]);
    let out = mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "set", "mfr_path:tree_ref=/forced_name"]);
    assert_eq!(out.code, 1, "writing mfr_* without --force must fail");
    assert!(out.stderr.starts_with("error:"));
    let out =
        mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "set", "mfr_path:tree_ref=/forced_name", "--force"]);
    assert_ok(&out);
}

#[test]
fn test_add_appends_multimap_row() {
    let (repo, _) = init_repo("add");
    let uuid = create_metarecord(&repo, &["genre:string=jazz"]);
    let out = mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "add", "genre:string=blues"]);
    assert_ok(&out);
    let entries = get_entries(&repo, &uuid);
    let fields = entries[0]["fields"].as_array().unwrap();
    assert_eq!(fields.iter().filter(|f| f["name"] == "genre").count(), 2);
}

#[test]
fn test_add_with_predicate_appends_to_matches() {
    let (repo, _) = init_repo("add_pred");
    create_metarecord(&repo, &["genre:string=jazz"]);
    create_metarecord(&repo, &["genre:string=jazz"]);
    create_metarecord(&repo, &["genre:string=rock"]);
    let out = mf(&["-u", &repo, "metarecord", "-q", r#"genre = "jazz""#, "field", "add", "tag:string=x"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "2");
    let out = mf(&["-u", &repo, "metarecord", "-q", r#"tag = "x""#, "get"]);
    assert_eq!(out.stdout.lines().count(), 2);
}

#[test]
fn test_remove_by_uuid_drops_only_matching_value_rows() {
    let (repo, _) = init_repo("remove_uuid");
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
    let (repo, _) = init_repo("remove_pred");
    create_metarecord(&repo, &["tag:string=test", "tag:string=keep"]);
    create_metarecord(&repo, &["tag:string=test"]);
    create_metarecord(&repo, &["tag:string=keep"]);
    let out = mf(&["-u", &repo, "metarecord", "-q", "tag IS PRESENT", "field", "delete", "tag:string=test"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "2", "two metarecords carried tag=test");
    assert_eq!(mf(&["-u", &repo, "metarecord", "-q", r#"tag = "test""#, "get"]).stdout.lines().count(), 0);
    assert_eq!(mf(&["-u", &repo, "metarecord", "-q", r#"tag = "keep""#, "get"]).stdout.lines().count(), 2);
}

#[test]
fn test_unset_deletes_single_row_by_id() {
    let (repo, _) = init_repo("unset");
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
    let genres: Vec<&serde_json::Value> =
        fields.iter().filter(|f| f["name"] == "genre").collect();
    assert_eq!(genres.len(), 1);
    assert_eq!(genres[0]["value"]["value"], "blues");
}

#[test]
fn test_delete_by_uuid_prints_count() {
    let (repo, _) = init_repo("delete");
    let uuid = create_metarecord(&repo, &["x:int=1"]);
    let out = mf(&["-u", &repo, "metarecord", "-i", &uuid, "delete"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "1");
    let out = mf(&["-u", &repo, "metarecord", "-i", &uuid, "get"]);
    assert_eq!(out.code, 1);
}

#[test]
fn test_delete_predicate_asks_for_confirmation() {
    let (repo, _) = init_repo("delete_confirm");
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
    let (repo, _) = init_repo("query");
    let high = create_metarecord(&repo, &["rating:int=5"]);
    let _low = create_metarecord(&repo, &["rating:int=1"]);
    let out = mf(&["-u", &repo, "metarecord", "-q", "rating > 3", "get"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), high);
}

#[test]
fn test_query_simplified_expands_before_running() {
    let (repo, _) = init_repo("query_simplified");
    let high = create_metarecord(&repo, &["rating:int=5"]);
    let _low = create_metarecord(&repo, &["rating:int=1"]);
    // `rating=5` expands to `rating = 5` locally via the core grammar.
    let out = mf_cfg(&["-u", &repo, "metarecord", "-q", "rating=5", "-s", "get"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), high);
}

#[test]
fn test_query_simplified_date_macro_filters() {
    let (repo, _) = init_repo("query_date_macro");
    // mfr_btime is reserved, so set it with --force. The datetime field spec
    // parses the ISO string to Unix ms.
    let recent = mf(&["-u", &repo, "metarecord", "add", "mfr_btime:datetime=2024-06-01", "--force"]);
    assert_ok(&recent);
    let recent = recent.stdout.trim().to_string();
    let old = mf(&["-u", &repo, "metarecord", "add", "mfr_btime:datetime=2020-01-01", "--force"]);
    assert_ok(&old);
    // `created since "2023-01-01"` → mfr_btime >= @"2023-01-01": only the recent one.
    let out = mf_cfg(&["-u", &repo, "metarecord", "-q", "created since \"2023-01-01\"", "-s", "get"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), recent);
}

#[test]
fn test_query_select_star_prints_objects() {
    let (repo, _) = init_repo("query_star");
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
    let (repo, _) = init_repo("query_select");
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
    let (repo, _) = init_repo("query_sort");
    let r1 = create_metarecord(&repo, &["rating:int=1", "kind:string=s"]);
    let r3 = create_metarecord(&repo, &["rating:int=3", "kind:string=s"]);
    let r2 = create_metarecord(&repo, &["rating:int=2", "kind:string=s"]);
    let out = mf(&["-u", &repo, "metarecord", "-q", r#"kind = "s""#, "get", "--sort", "rating:desc"]);
    assert_ok(&out);
    let lines: Vec<&str> = out.stdout.lines().collect();
    assert_eq!(lines, vec![r3.as_str(), r2.as_str(), r1.as_str()]);

    let out = mf(&["-u", &repo, "metarecord", "-q", r#"kind = "s""#, "get", "--sort", "rating:asc", "--limit", "2"]);
    assert_ok(&out);
    let lines: Vec<&str> = out.stdout.lines().collect();
    assert_eq!(lines, vec![r1.as_str(), r2.as_str()]);
}

#[test]
fn test_query_bad_dsl_is_usage_error() {
    let (repo, _) = init_repo("query_bad");
    let out = mf(&["-u", &repo, "metarecord", "-q", "a = 1 and b = 2", "get"]);
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
    assert!(out.stderr.starts_with("error:"));
}

#[test]
fn test_query_bad_sort_is_usage_error() {
    let (repo, _) = init_repo("query_bad_sort");
    let out = mf(&["-u", &repo, "metarecord", "-q", "a = 1", "get", "--sort", "rating:sideways"]);
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
}

// ── File tracking ─────────────────────────────────────────────────────────────

#[test]
fn test_track_creates_entry_and_rejects_duplicates() {
    let (repo, root) = init_repo("track");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("sub/file.txt"), b"hello").unwrap();
    let path = root.join("sub/file.txt");

    let out = mf(&["-u", &repo, "track", path.to_str().unwrap()]);
    assert_ok(&out);
    assert!(is_hex_uuid(out.stdout.trim()));

    // Already tracked → operation error.
    let out = mf(&["-u", &repo, "track", path.to_str().unwrap()]);
    assert_eq!(out.code, 1, "stderr: {}", out.stderr);

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

    let out = mf(&["-u", &repo, "metarecord", "-i", &root_uuid, "field", "set", "mf_watch:bool=true"]);
    assert_ok(&out);

    let out = mf(&["-u", &repo, "reconcile"]);
    assert_ok(&out);
    // a.txt + b.txt only: .metafolder is ignored by default (hidden-entry
    // and .metafolder patterns), so config.json under it is not tracked.
    assert!(
        out.stdout.starts_with("created: 2  moved: 0"),
        "unexpected summary: {}",
        out.stdout
    );

    // A second reconcile is a no-op; --json prints the raw body.
    let out = mf(&["-u", &repo, "reconcile", "--json"]);
    assert_ok(&out);
    let parsed: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(parsed["created"], 0);
    assert_eq!(parsed["moved"], 0);
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

    let out = mf(&["-u", &repo, "metarecord", "-i", &dir_uuid, "field", "set", "mf_watch:bool=true"]);
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
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &root_uuid, "field", "set", "mf_watch:bool=true"]));
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
    std::fs::write(root.join("pic.png"), [0x89u8, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0]).unwrap();

    let root_uuid = mf(&["-u", &repo, "metarecord", "get"]).stdout.trim().to_string();
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &root_uuid, "field", "set", "mf_watch:bool=true"]));

    // With --no-mime, no mfr_mime is written.
    assert_ok(&mf(&["-u", &repo, "reconcile", "--no-mime"]));
    let q = mf(&["-u", &repo, "metarecord", "-q", "mfr_mime IS PRESENT", "get"]);
    assert_ok(&q);
    assert!(q.stdout.trim().is_empty(), "no mime expected, got: {}", q.stdout);

    // A default reconcile computes it.
    assert_ok(&mf(&["-u", &repo, "reconcile"]));
    let pic = mf(&["-u", &repo, "metarecord", "-q", "mfr_mime = \"image/png\"", "get", "--select", "mfr_mime"]);
    assert_ok(&pic);
    assert!(pic.stdout.contains("image/png"), "stdout: {}", pic.stdout);
}

// ── Query --values ────────────────────────────────────────────────────────────

#[test]
fn test_query_values_prints_raw_scalars() {
    let (repo, _root) = init_repo("values");
    create_metarecord(&repo, &["type:string=tag", "name:string=jazz"]);
    create_metarecord(&repo, &["type:string=tag", "name:string=rock", "weight:int=3"]);

    let out = mf(&["-u", &repo, "metarecord", "-q", "type = \"tag\"", "get", "--select", "name", "--values"]);
    assert_ok(&out);
    let mut names: Vec<&str> = out.stdout.lines().collect();
    names.sort_unstable();
    assert_eq!(names, vec!["jazz", "rock"]);

    let out = mf(&["-u", &repo, "metarecord", "-q", "name = \"rock\"", "get", "--select", "weight", "--values"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "3");
}

#[test]
fn test_query_values_requires_a_single_selected_field() {
    let (repo, _root) = init_repo("values_usage");
    let out = mf(&["-u", &repo, "metarecord", "-q", "name = \"x\"", "get", "--values"]);
    assert_eq!(out.code, 2, "stdout: {}", out.stdout);
    let out = mf(&["-u", &repo, "metarecord", "-q", "name = \"x\"", "get", "--select", "a,b", "--values"]);
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
    let (repo, _) = init_repo("log_list");
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
    let (repo, _) = init_repo("log_graph");
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
    let (repo, _) = init_repo("log_ops");
    let uuid = create_metarecord(&repo, &["rating:int=3"]);
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "set", "rating:int=5"]));

    let out = mf(&["-u", &repo, "log", "list", "--ops"]);
    assert_ok(&out);
    assert!(out.stdout.contains("set_field(rating)"), "stdout: {}", out.stdout);
    assert!(out.stdout.contains("op "), "stdout: {}", out.stdout);
}

#[test]
fn test_log_show_displays_before_and_after() {
    let (repo, _) = init_repo("log_show");
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
    let (repo, _) = init_repo("log_show_bad");
    let out = mf(&["-u", &repo, "log", "show", "notanumber"]);
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
}

#[test]
fn test_prune_linearize_with_no_branches_removes_nothing() {
    let (repo, _) = init_repo("prune_lin");
    let uuid = create_metarecord(&repo, &["rating:int=3"]);
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "set", "rating:int=5"]));

    // A far-future timestamp resolves to HEAD; with no side branches,
    // linearize removes nothing.
    let out = mf(&["-u", &repo, "log", "prune", "linearize", "--timestamp", "@9999999999999", "--force"]);
    assert_ok(&out);
    assert!(out.stdout.contains("Pruned 0 operations"), "stdout: {}", out.stdout);
    assert!(out.stdout.contains("linearized"), "stdout: {}", out.stdout);
}

#[test]
fn test_prune_before_makes_target_the_root() {
    let (repo, _) = init_repo("prune_before");
    let uuid = create_metarecord(&repo, &["rating:int=3"]);
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "set", "rating:int=5"]));
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "set", "rating:int=7"]));

    // Prune before HEAD: every older operation is removed.
    let out = mf(&["-u", &repo, "log", "prune", "before", "--timestamp", "@9999999999999", "--force"]);
    assert_ok(&out);
    assert!(out.stdout.starts_with("Pruned "), "stdout: {}", out.stdout);
    // History still readable afterwards.
    assert_ok(&mf(&["-u", &repo, "log", "list"]));
}

#[test]
fn test_prune_requires_a_target() {
    let (repo, _) = init_repo("prune_notarget");
    let out = mf(&["-u", &repo, "log", "prune", "before"]);
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
}

#[test]
fn test_rollback_plan_previews_operations() {
    let (repo, _) = init_repo("rbk_plan");
    let uuid = create_metarecord(&repo, &["rating:int=3"]);
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "set", "rating:int=5"]));
    let out = mf(&["-u", &repo, "log", "rollback", "plan"]);
    assert_ok(&out);
    assert!(out.stdout.contains("set_field"), "stdout: {}", out.stdout);
    assert!(out.stdout.contains("operations."), "stdout: {}", out.stdout);
}

#[test]
fn test_rollback_undoes_last_revision_and_releases_lock() {
    let (repo, _) = init_repo("rbk_run");
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
    let (repo, _) = init_repo("rbk_policy");
    let uuid = create_metarecord(&repo, &["rating:int=3"]);
    assert_ok(&mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "set", "rating:int=5"]));
    let out = mf(&["-u", &repo, "log", "rollback", "--on-move-available", "bogus"]);
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
}

#[test]
fn test_prune_without_force_aborts_on_no() {
    let (repo, _) = init_repo("prune_confirm");
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
    let (repo, _) = init_repo("mset");
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
    let (repo, _) = init_repo("fmulti");
    let uuid = create_metarecord(&repo, &["genre:string=jazz"]);

    // Set two values of `tag` at once (multi-map).
    let out = mf(&["-u", &repo, "metarecord", "-i", &uuid, "field", "set", "tag:string=a", "tag:string=b"]);
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
    let (repo, _) = init_repo("fbyid");
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
