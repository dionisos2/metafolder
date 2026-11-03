//! End-to-end tests: run the real `mf` binary against an in-process daemon
//! listening on an ephemeral port.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use uuid::Uuid;

#[path = "../common/mod.rs"]
mod common;
use common::{TempDir, TempFile};

// Helpers one group of tests shares with another.
use duplicates::plan_ops;
use log::take_back;
use records::FILM_SCHEMA;
use sync::{field_value_of, plan_name, query_one, repo_root_of, tracked_repo, write_intents};
use trash::{poll, poll_mf, uuids_at};

mod coverage_pass;
mod duplicates;
mod log;
mod query;
mod records;
mod repository;
mod slow;
mod sync;
mod tags;
mod tracking;
mod trash;

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
                // Its config.toml names the known repositories (doc "Known
                // repositories").
                let settings = metafolder_daemon::daemon_config::parse_config(
                    SHIPPED_DAEMON_CONFIG,
                    std::path::Path::new("shipped"),
                )
                .unwrap()
                .settings;
                let app_state = metafolder_daemon::state::AppState::new(settings)
                    .with_config_file(daemon_config_file().to_path_buf());
                let app = metafolder_daemon::routes::build(std::sync::Arc::new(app_state));
                axum::serve(listener, app).await.unwrap();
            });
        });
        format!("http://127.0.0.1:{}", addr.port())
    })
}

const SHIPPED_DAEMON_CONFIG: &str = include_str!("../../../daemon/default-config/config.toml");

/// The shared daemon's `config.toml`, written by the one test of known
/// repositories (no other test may: they run concurrently).
fn daemon_config_file() -> &'static std::path::Path {
    static PATH: OnceLock<std::path::PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let dir = common::tests_root().join(format!("cli-daemon-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, SHIPPED_DAEMON_CONFIG).unwrap();
        path
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
    /// The exact bytes, for output that need not be UTF-8 (a path).
    stdout_bytes: Vec<u8>,
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
        .env_remove("METAFOLDER_DAEMON_PORT")
        .env_remove("METAFOLDER_LABEL")
        .env("XDG_CONFIG_HOME", config_xdg());
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
        stdout_bytes: output.stdout.clone(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn mf(args: &[&str]) -> Out {
    mf_full(args, None, &[], true)
}

const SHIPPED_CLI_CONFIG: &str = include_str!("../../default-config/config.toml");

/// Installs the shipped `cli/config.toml` under the `XDG_CONFIG_HOME` at `xdg`:
/// `mf` requires it (doc "No runtime fallback").
fn install_cli_config(xdg: &std::path::Path) {
    let cli = xdg.join("metafolder").join("cli");
    std::fs::create_dir_all(&cli).unwrap();
    std::fs::write(cli.join("config.toml"), SHIPPED_CLI_CONFIG).unwrap();
}

/// `XDG_CONFIG_HOME` for a config dir with the shipped `cli/` and `core/` config
/// installed: the query-grammar (so `mf metarecord get -q -s` can expand locally)
/// and the ignore-presets (so `mf repo init` / `mf ignore` can apply the
/// `default` preset — the daemon no longer ships ignore patterns); and the
/// repository defaults `mf repo init` installs (doc "Repository
/// configuration"), reduced to the metadata map — the shipped schema would
/// constrain every test's metarecords.
fn config_xdg() -> &'static str {
    use std::sync::OnceLock;
    static XDG: OnceLock<String> = OnceLock::new();
    XDG.get_or_init(|| {
        let dir = common::tests_root().join(format!("cli-cfg-{}", std::process::id()));
        install_cli_config(&dir);
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
        let defaults = dir.join("metafolder/daemon/repo-default");
        std::fs::create_dir_all(&defaults).unwrap();
        std::fs::copy(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../daemon/default-config/repo-default/metadata-map.toml"
            ),
            defaults.join("metadata-map.toml"),
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
