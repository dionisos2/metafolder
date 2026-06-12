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
                let state = std::sync::Arc::new(metafolder_daemon::state::AppState::new());
                let app = metafolder_daemon::routes::build(state);
                axum::serve(listener, app).await.unwrap();
            });
        });
        format!("http://127.0.0.1:{}", addr.port())
    })
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

fn mf_full(args: &[&str], stdin: Option<&str>, envs: &[(&str, &str)], daemon: bool) -> Out {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mf"));
    if daemon {
        cmd.arg("--daemon-url").arg(daemon_url());
    }
    cmd.args(args);
    cmd.env_remove("METAFOLDER_REPO").env_remove("METAFOLDER_DAEMON_URL");
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
    let out = mf(&["init", root.to_str().unwrap()]);
    assert_ok(&out);
    let uuid = out.stdout.trim().to_string();
    assert!(is_hex_uuid(&uuid), "init should print a 32-hex uuid, got: '{uuid}'");
    (uuid, root)
}

/// Creates an entry from field specs; returns its UUID.
fn create_entry(repo: &str, specs: &[&str]) -> String {
    let mut args = vec!["--repo", repo, "create"];
    for spec in specs {
        args.push("--field");
        args.push(spec);
    }
    let out = mf(&args);
    assert_ok(&out);
    let uuid = out.stdout.trim().to_string();
    assert!(is_hex_uuid(&uuid), "create should print a 32-hex uuid, got: '{uuid}'");
    uuid
}

fn get_entries(repo: &str, target: &str) -> serde_json::Value {
    let out = mf(&["--repo", repo, "get", target]);
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
        mf(&["init", root.to_str().unwrap(), "--metafolder", external.to_str().unwrap()]);
    assert_ok(&out);
    assert!(is_hex_uuid(out.stdout.trim()));
    assert!(external.join("config.json").exists());
    assert!(!root.join(".metafolder").exists());
}

#[test]
fn test_load_root_is_idempotent() {
    let (repo, root) = init_repo("load");
    let out = mf(&["load", root.to_str().unwrap()]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), repo);
}

#[test]
fn test_load_with_metafolder_flag() {
    let root = temp_dir("load_ext_root");
    let external = temp_dir("load_ext_db");
    let out =
        mf(&["init", root.to_str().unwrap(), "--metafolder", external.to_str().unwrap()]);
    assert_ok(&out);
    let repo = out.stdout.trim().to_string();
    let out = mf(&["load", "--metafolder", external.to_str().unwrap()]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), repo);
}

#[test]
fn test_load_requires_exactly_one_locator() {
    let out = mf(&["load"]);
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
    let root = temp_dir("load_both");
    let out = mf(&["load", root.to_str().unwrap(), "--metafolder", root.to_str().unwrap()]);
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
}

#[test]
fn test_repos_lists_loaded_repositories() {
    let (repo, _) = init_repo("repos");
    let out = mf(&["repos"]);
    assert_ok(&out);
    let parsed: serde_json::Value = serde_json::from_str(&out.stdout).expect("pretty JSON");
    assert!(out.stdout.contains(&repo), "repos output should mention {repo}");
    assert!(parsed.is_array() || parsed.is_object());
}

// ── Global options and exit codes ─────────────────────────────────────────────

#[test]
fn test_missing_repo_is_usage_error_without_contacting_daemon() {
    // Unreachable daemon URL: exit code 2 proves no HTTP round-trip happened.
    let out = mf_full(&["--daemon-url", "http://127.0.0.1:1", "list"], None, &[], false);
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
}

#[test]
fn test_invalid_repo_uuid_is_usage_error() {
    let out = mf(&["--repo", "not-a-uuid", "list"]);
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
}

#[test]
fn test_unreachable_daemon_is_operation_error() {
    let out = mf_full(&["--daemon-url", "http://127.0.0.1:1", "repos"], None, &[], false);
    assert_eq!(out.code, 1, "stderr: {}", out.stderr);
    assert!(out.stderr.starts_with("error:"), "stderr: {}", out.stderr);
}

#[test]
fn test_env_variables_are_honoured() {
    let (repo, _) = init_repo("env");
    let out = mf_full(
        &["list"],
        None,
        &[("METAFOLDER_DAEMON_URL", daemon_url()), ("METAFOLDER_REPO", repo.as_str())],
        false,
    );
    assert_ok(&out);
    assert!(!out.stdout.trim().is_empty());
}

#[test]
fn test_daemon_error_goes_to_stderr() {
    let (repo, _) = init_repo("daemon_err");
    let missing = "00000000000000000000000000000099";
    let out = mf(&["--repo", &repo, "get", missing]);
    assert_eq!(out.code, 1);
    assert!(out.stderr.starts_with("error:"), "stderr: {}", out.stderr);
    assert!(out.stdout.is_empty());
}

// ── Entry manipulation ────────────────────────────────────────────────────────

#[test]
fn test_create_and_get_by_uuid() {
    let (repo, _) = init_repo("create");
    let uuid = create_entry(&repo, &["rating:int=5", "genre:string=jazz"]);
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
fn test_create_reserved_field_requires_force() {
    let (repo, _) = init_repo("create_force");
    let out = mf(&["--repo", &repo, "create", "--field", "mfr_path:tree_ref=/created_name"]);
    assert_eq!(out.code, 1, "creating with mfr_* without --force must fail");
    assert!(out.stderr.starts_with("error:"), "stderr: {}", out.stderr);

    let out = mf(&[
        "--repo", &repo, "create", "--field", "mfr_path:tree_ref=/created_name", "--force",
    ]);
    assert_ok(&out);
    let uuid = out.stdout.trim().to_string();
    assert!(is_hex_uuid(&uuid));
    let entries = get_entries(&repo, &uuid);
    assert_eq!(entries[0]["fields"][0]["name"], "mfr_path");
}

#[test]
fn test_get_with_fields_filter() {
    let (repo, _) = init_repo("get_fields");
    let uuid = create_entry(&repo, &["rating:int=5", "genre:string=jazz"]);
    let out = mf(&["--repo", &repo, "get", &uuid, "--fields", "genre"]);
    assert_ok(&out);
    let entries: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    let fields = entries[0]["fields"].as_array().unwrap();
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0]["name"], "genre");
}

#[test]
fn test_get_with_predicate() {
    let (repo, _) = init_repo("get_pred");
    let jazz = create_entry(&repo, &["genre:string=jazz"]);
    let _rock = create_entry(&repo, &["genre:string=rock"]);
    let entries = get_entries(&repo, r#"genre = "jazz""#);
    let list = entries.as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["uuid"], serde_json::json!(jazz));
}

#[test]
fn test_list_prints_uuids_one_per_line() {
    let (repo, _) = init_repo("list");
    let a = create_entry(&repo, &["x:int=1"]);
    let b = create_entry(&repo, &["x:int=2"]);
    let out = mf(&["--repo", &repo, "list"]);
    assert_ok(&out);
    let lines: Vec<&str> = out.stdout.lines().collect();
    // Root entry + the two created entries.
    assert_eq!(lines.len(), 3, "stdout: {}", out.stdout);
    assert!(lines.iter().all(|l| is_hex_uuid(l)));
    assert!(lines.contains(&a.as_str()) && lines.contains(&b.as_str()));

    let out = mf(&["--repo", &repo, "list", "--limit", "2"]);
    assert_ok(&out);
    assert_eq!(out.stdout.lines().count(), 2);
}

#[test]
fn test_set_uuid_replaces_all_rows() {
    let (repo, _) = init_repo("set");
    let uuid = create_entry(&repo, &["tag:string=a", "tag:string=b"]);
    let out = mf(&["--repo", &repo, "set", &uuid, "tag:string=c"]);
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
    create_entry(&repo, &["genre:string=jazz"]);
    create_entry(&repo, &["genre:string=jazz"]);
    create_entry(&repo, &["genre:string=rock"]);
    let out = mf(&["--repo", &repo, "set", r#"genre = "jazz""#, "rating:int=4"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "2");
    let out = mf(&["--repo", &repo, "query", "rating = 4"]);
    assert_ok(&out);
    assert_eq!(out.stdout.lines().count(), 2);
}

#[test]
fn test_set_reserved_field_requires_force() {
    let (repo, _) = init_repo("set_force");
    let uuid = create_entry(&repo, &["x:int=1"]);
    let out = mf(&["--repo", &repo, "set", &uuid, "mfr_path:tree_ref=/forced_name"]);
    assert_eq!(out.code, 1, "writing mfr_* without --force must fail");
    assert!(out.stderr.starts_with("error:"));
    let out =
        mf(&["--repo", &repo, "set", &uuid, "mfr_path:tree_ref=/forced_name", "--force"]);
    assert_ok(&out);
}

#[test]
fn test_add_appends_multimap_row() {
    let (repo, _) = init_repo("add");
    let uuid = create_entry(&repo, &["genre:string=jazz"]);
    let out = mf(&["--repo", &repo, "add", &uuid, "genre:string=blues"]);
    assert_ok(&out);
    let entries = get_entries(&repo, &uuid);
    let fields = entries[0]["fields"].as_array().unwrap();
    assert_eq!(fields.iter().filter(|f| f["name"] == "genre").count(), 2);
}

#[test]
fn test_unset_deletes_single_row_by_id() {
    let (repo, _) = init_repo("unset");
    let uuid = create_entry(&repo, &["genre:string=jazz", "genre:string=blues"]);
    let entries = get_entries(&repo, &uuid);
    let fields = entries[0]["fields"].as_array().unwrap();
    let jazz_id = fields
        .iter()
        .find(|f| f["value"]["value"] == "jazz")
        .and_then(|f| f["id"].as_i64())
        .expect("jazz row id");
    let out = mf(&["--repo", &repo, "unset", &uuid, &jazz_id.to_string()]);
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
    let uuid = create_entry(&repo, &["x:int=1"]);
    let out = mf(&["--repo", &repo, "delete", &uuid]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "1");
    let out = mf(&["--repo", &repo, "get", &uuid]);
    assert_eq!(out.code, 1);
}

#[test]
fn test_delete_predicate_asks_for_confirmation() {
    let (repo, _) = init_repo("delete_confirm");
    create_entry(&repo, &["genre:string=del_me"]);
    create_entry(&repo, &["genre:string=del_me"]);

    // Refusing the confirmation aborts without deleting.
    let out = mf_full(
        &["--daemon-url", daemon_url(), "--repo", &repo, "delete", r#"genre = "del_me""#],
        Some("n\n"),
        &[],
        false,
    );
    assert_eq!(out.code, 1, "refused confirmation should exit 1");
    let out = mf(&["--repo", &repo, "query", r#"genre = "del_me""#]);
    assert_eq!(out.stdout.lines().count(), 2, "entries must survive a refused confirmation");

    // --force skips the prompt.
    let out = mf(&["--repo", &repo, "delete", r#"genre = "del_me""#, "--force"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "2");
    let out = mf(&["--repo", &repo, "query", r#"genre = "del_me""#]);
    assert_eq!(out.stdout.trim(), "");
}

// ── Query ─────────────────────────────────────────────────────────────────────

#[test]
fn test_query_prints_matching_uuids() {
    let (repo, _) = init_repo("query");
    let high = create_entry(&repo, &["rating:int=5"]);
    let _low = create_entry(&repo, &["rating:int=1"]);
    let out = mf(&["--repo", &repo, "query", "rating > 3"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), high);
}

#[test]
fn test_query_select_star_prints_objects() {
    let (repo, _) = init_repo("query_star");
    create_entry(&repo, &["rating:int=5", "genre:string=jazz"]);
    let out = mf(&["--repo", &repo, "query", "rating = 5", "--select", "*"]);
    assert_ok(&out);
    let parsed: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    let list = parsed.as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["fields"].as_array().unwrap().len(), 2);
}

#[test]
fn test_query_select_field_list_restricts_fields() {
    let (repo, _) = init_repo("query_select");
    create_entry(&repo, &["rating:int=5", "genre:string=jazz"]);
    let out = mf(&["--repo", &repo, "query", "rating = 5", "--select", "genre"]);
    assert_ok(&out);
    let parsed: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    let fields = parsed[0]["fields"].as_array().unwrap();
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0]["name"], "genre");
}

#[test]
fn test_query_sort_and_limit() {
    let (repo, _) = init_repo("query_sort");
    let r1 = create_entry(&repo, &["rating:int=1", "kind:string=s"]);
    let r3 = create_entry(&repo, &["rating:int=3", "kind:string=s"]);
    let r2 = create_entry(&repo, &["rating:int=2", "kind:string=s"]);
    let out = mf(&["--repo", &repo, "query", r#"kind = "s""#, "--sort", "rating:desc"]);
    assert_ok(&out);
    let lines: Vec<&str> = out.stdout.lines().collect();
    assert_eq!(lines, vec![r3.as_str(), r2.as_str(), r1.as_str()]);

    let out = mf(&[
        "--repo", &repo, "query", r#"kind = "s""#, "--sort", "rating:asc", "--limit", "2",
    ]);
    assert_ok(&out);
    let lines: Vec<&str> = out.stdout.lines().collect();
    assert_eq!(lines, vec![r1.as_str(), r2.as_str()]);
}

#[test]
fn test_query_bad_dsl_is_usage_error() {
    let (repo, _) = init_repo("query_bad");
    let out = mf(&["--repo", &repo, "query", "a = 1 and b = 2"]);
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
    assert!(out.stderr.starts_with("error:"));
}

#[test]
fn test_query_bad_sort_is_usage_error() {
    let (repo, _) = init_repo("query_bad_sort");
    let out = mf(&["--repo", &repo, "query", "a = 1", "--sort", "rating:sideways"]);
    assert_eq!(out.code, 2, "stderr: {}", out.stderr);
}

// ── File tracking ─────────────────────────────────────────────────────────────

#[test]
fn test_track_creates_entry_and_rejects_duplicates() {
    let (repo, root) = init_repo("track");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("sub/file.txt"), b"hello").unwrap();
    let path = root.join("sub/file.txt");

    let out = mf(&["--repo", &repo, "track", path.to_str().unwrap()]);
    assert_ok(&out);
    assert!(is_hex_uuid(out.stdout.trim()));

    // Already tracked → operation error.
    let out = mf(&["--repo", &repo, "track", path.to_str().unwrap()]);
    assert_eq!(out.code, 1, "stderr: {}", out.stderr);

    // Outside the repository root → operation error.
    let outside = temp_dir("track_outside");
    std::fs::write(outside.join("f.txt"), b"x").unwrap();
    let out = mf(&["--repo", &repo, "track", outside.join("f.txt").to_str().unwrap()]);
    assert_eq!(out.code, 1, "stderr: {}", out.stderr);
}

#[test]
fn test_reconcile_reports_created_entries() {
    let (repo, root) = init_repo("reconcile");
    std::fs::write(root.join("a.txt"), b"aaa").unwrap();
    std::fs::write(root.join("b.txt"), b"bbb").unwrap();

    // The repository starts with a single entry: the filesystem root.
    let out = mf(&["--repo", &repo, "list"]);
    assert_ok(&out);
    let root_uuid = out.stdout.trim().to_string();
    assert!(is_hex_uuid(&root_uuid));

    let out = mf(&["--repo", &repo, "set", &root_uuid, "mf_watch:bool=true"]);
    assert_ok(&out);

    let out = mf(&["--repo", &repo, "reconcile"]);
    assert_ok(&out);
    // a.txt + b.txt + .metafolder + config.json (internal/ is excluded).
    assert!(
        out.stdout.starts_with("created: 4  moved: 0"),
        "unexpected summary: {}",
        out.stdout
    );

    // A second reconcile is a no-op; --json prints the raw body.
    let out = mf(&["--repo", &repo, "reconcile", "--json"]);
    assert_ok(&out);
    let parsed: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(parsed["created"], 0);
    assert_eq!(parsed["moved"], 0);
}

#[test]
fn test_reconcile_single_entry() {
    let (repo, root) = init_repo("reconcile_entry");
    std::fs::create_dir_all(root.join("dir")).unwrap();
    std::fs::write(root.join("dir/inside.txt"), b"in").unwrap();

    let out = mf(&["--repo", &repo, "track", root.join("dir").to_str().unwrap()]);
    assert_ok(&out);
    let dir_uuid = out.stdout.trim().to_string();

    let out = mf(&["--repo", &repo, "set", &dir_uuid, "mf_watch:bool=true"]);
    assert_ok(&out);
    let out = mf(&["--repo", &repo, "reconcile", "--entry", &dir_uuid]);
    assert_ok(&out);
    assert!(out.stdout.starts_with("created: 1"), "unexpected summary: {}", out.stdout);
}

// ── Query --values ────────────────────────────────────────────────────────────

#[test]
fn test_query_values_prints_raw_scalars() {
    let (repo, _root) = init_repo("values");
    create_entry(&repo, &["type:string=tag", "name:string=jazz"]);
    create_entry(&repo, &["type:string=tag", "name:string=rock", "weight:int=3"]);

    let out = mf(&["--repo", &repo, "query", "type = \"tag\"", "--select", "name", "--values"]);
    assert_ok(&out);
    let mut names: Vec<&str> = out.stdout.lines().collect();
    names.sort_unstable();
    assert_eq!(names, vec!["jazz", "rock"]);

    let out = mf(&["--repo", &repo, "query", "name = \"rock\"", "--select", "weight", "--values"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "3");
}

#[test]
fn test_query_values_requires_a_single_selected_field() {
    let (repo, _root) = init_repo("values_usage");
    let out = mf(&["--repo", &repo, "query", "name = \"x\"", "--values"]);
    assert_eq!(out.code, 2, "stdout: {}", out.stdout);
    let out = mf(&["--repo", &repo, "query", "name = \"x\"", "--select", "a,b", "--values"]);
    assert_eq!(out.code, 2, "stdout: {}", out.stdout);
}

// ── Path resolution ───────────────────────────────────────────────────────────

#[test]
fn test_path_resolves_tracked_file() {
    let (repo, root) = init_repo("path");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("sub/file.txt"), b"hello").unwrap();

    let out = mf(&["--repo", &repo, "track", root.join("sub/file.txt").to_str().unwrap()]);
    assert_ok(&out);
    let uuid = out.stdout.trim().to_string();

    let out = mf(&["--repo", &repo, "path", &uuid]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), root.join("sub/file.txt").to_str().unwrap());

    let out = mf(&["--repo", &repo, "path", "--relative", &uuid]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "/sub/file.txt");
}

#[test]
fn test_path_of_the_root_entry() {
    let (repo, root) = init_repo("path_root");
    let out = mf(&["--repo", &repo, "list"]);
    assert_ok(&out);
    let root_uuid = out.stdout.trim().to_string();

    let out = mf(&["--repo", &repo, "path", &root_uuid]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), root.to_str().unwrap());

    let out = mf(&["--repo", &repo, "path", "--relative", &root_uuid]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "/");
}

#[test]
fn test_path_fails_on_entry_without_mfr_path() {
    let (repo, _root) = init_repo("path_none");
    let uuid = create_entry(&repo, &["title:string=no path"]);
    let out = mf(&["--repo", &repo, "path", &uuid]);
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
    let bad = create_entry(&repo, &["mf_schema:string=film", "rating:string=oops"]);

    std::fs::write(root.join(".metafolder/schema.json"), FILM_SCHEMA).unwrap();
    let out = mf(&["--repo", &repo, "schema", "reload"]);
    assert_ok(&out);

    let out = mf(&["--repo", &repo, "schema", "show"]);
    assert_ok(&out);
    let parsed: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(parsed["version"], 1);

    // One violation: exit code 1, one line per violation plus the summary.
    let out = mf(&["--repo", &repo, "schema", "check"]);
    assert_eq!(out.code, 1, "violations must yield exit code 1\nstdout: {}", out.stdout);
    assert!(out.stdout.contains(&bad), "violation line should name the entry");
    assert!(out.stdout.contains("Checked 2 entries, 1 violation"), "stdout: {}", out.stdout);

    // Fix the entry: no violations left, exit code 0.
    let out = mf(&["--repo", &repo, "set", &bad, "rating:int=5"]);
    assert_ok(&out);
    let out = mf(&["--repo", &repo, "schema", "check"]);
    assert_ok(&out);
    assert!(out.stdout.contains("0 violations"), "stdout: {}", out.stdout);

    // --json prints the raw response body.
    let out = mf(&["--repo", &repo, "schema", "check", "--json"]);
    assert_ok(&out);
    let parsed: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(parsed["checked"], 2);
}

#[test]
fn test_schema_check_with_predicate() {
    let (repo, root) = init_repo("schema_pred");
    create_entry(&repo, &["mf_schema:string=film", "rating:string=bad"]);
    std::fs::write(root.join(".metafolder/schema.json"), FILM_SCHEMA).unwrap();
    let out = mf(&["--repo", &repo, "schema", "reload"]);
    assert_ok(&out);

    // The predicate restricts the scan to non-matching entries: no violation.
    let out = mf(&["--repo", &repo, "schema", "check", r#"mf_schema = "documentary""#]);
    assert_ok(&out);
    assert!(out.stdout.contains("Checked 0 entries"), "stdout: {}", out.stdout);
}

#[test]
fn test_schema_reload_invalid_file_fails() {
    let (repo, root) = init_repo("schema_invalid");
    std::fs::write(root.join(".metafolder/schema.json"), "{not json").unwrap();
    let out = mf(&["--repo", &repo, "schema", "reload"]);
    assert_eq!(out.code, 1, "stderr: {}", out.stderr);
    assert!(out.stderr.starts_with("error:"));
}
