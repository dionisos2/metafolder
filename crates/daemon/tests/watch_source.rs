//! The watch sources end to end (spec-file-tracking "Watch sources and
//! regimes"): the source *choice* at load, and the fanotify source — a
//! stand-in broker on a Unix socket, events on the wire becoming metarecords
//! through the real pipeline (client → ingest → executor).
//!
//! The kernel side is not here: the broker's own tests cover it (including one
//! against the real fanotify), and mount marks need capabilities this harness
//! does not have. What is under test is the daemon's half — selection, the
//! client, and the coverage-regime honour of `mfr_watch_exceeded`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use metafolder_core::metarecord::{Field, Value};
use metafolder_daemon::daemon_config::DaemonSettings;
use metafolder_daemon::db;
use metafolder_daemon::executor;
use metafolder_daemon::log::Writer;
use metafolder_daemon::repo;
use metafolder_daemon::state::RepoState;
use metafolder_daemon::watcher;
use metafolder_watchd::proto::{self, ClientMsg, Event, ServerMsg};
use uuid::Uuid;

mod common;
use common::TempDir;

/// A repository with tracking enabled on the root, loaded with `settings`.
fn setup(name: &str, settings: DaemonSettings) -> (Arc<RepoState>, TempDir) {
    let root = TempDir::new(&format!("wsrc_{name}"));
    let opened = repo::init_repository(&root, None, None, false).unwrap();
    let repo_state = Arc::new(RepoState::from_opened_with(opened, &settings));
    {
        let mut conn = repo_state.conn.lock().unwrap();
        let root_uuid = db::find_tree_child(&conn, "mfr_path", None, "").unwrap().unwrap();
        let mut w = Writer::begin(&mut conn, None).unwrap();
        w.set_field(root_uuid, "mf_watch", Value::Bool(true)).unwrap();
        w.commit().unwrap();
    }
    (repo_state, root)
}

/// A stand-in broker: answers the handshake, then sends `script` at once.
struct FakeBroker {
    socket: PathBuf,
}

impl FakeBroker {
    fn start(socket: PathBuf, script: Vec<ServerMsg>) -> Self {
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut line = String::new();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                reader.read_line(&mut line).unwrap();
                let roots = match proto::decode::<ClientMsg>(&line).unwrap() {
                    ClientMsg::Subscribe { roots } => roots,
                };
                stream
                    .write_all(
                        proto::encode(&ServerMsg::Subscribed { roots, denied: Vec::new() })
                            .as_bytes(),
                    )
                    .unwrap();
                for msg in &script {
                    stream.write_all(proto::encode(msg).as_bytes()).unwrap();
                }
                // Keep the stream open until the test is done with it.
                std::thread::sleep(Duration::from_secs(10));
            }
        });
        Self { socket }
    }
}

impl Drop for FakeBroker {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
    }
}

fn write_file(root: &Path, rel: &str, content: &[u8]) {
    let path = root.join(rel.trim_start_matches('/'));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, content).unwrap();
}

/// The metarecord at a repo-root-relative path, as the executor.rs tests ask
/// for one.
fn resolve(repo: &RepoState, path: &str) -> Option<Uuid> {
    let conn = repo.conn.lock().unwrap();
    let mut cache = repo.cache.lock().unwrap();
    cache.resolve_path(&conn, "mfr_path", path).unwrap()
}

/// Waits for `cond`, so a test reads as its outcome and not as its timing.
fn wait_for(what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for: {what}");
}

/// Lets the pipeline settle (the executor flushes on a quiet period).
fn settle() {
    std::thread::sleep(Duration::from_millis(300));
}

/// Gives `rel` its own metarecord carrying `mfr_watch_exceeded = true` — the
/// one bit of watch vocabulary both regimes honour.
fn mark_exceeded(repo: &RepoState, rel: &str) {
    let mut conn = repo.conn.lock().unwrap();
    let mut cache = repo.cache.lock().unwrap();
    let parent_rel = rel.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
    let parent =
        cache.resolve_path(&conn, "mfr_path", parent_rel).unwrap().expect("parent tracked");
    let name = rel.rsplit('/').next().unwrap().to_string();
    let mut w = Writer::begin(&mut conn, None).unwrap();
    let created = w
        .create_metarecord(vec![
            Field::new(
                "mfr_path",
                Value::TreeRef { parent: Some(parent), name: name.as_str().into() },
            ),
            Field::new("mfr_type", Value::String("dir".into())),
        ])
        .unwrap();
    w.set_field(created.uuid, "mfr_watch_exceeded", Value::Bool(true)).unwrap();
    w.commit().unwrap();
    cache.clear();
}

/// A wire path under the repository root, as the broker would send it.
fn wire(root: &Path, rest: &str) -> String {
    std::fs::canonicalize(root).unwrap().join(rest).display().to_string()
}

fn fanotify_settings(socket: &Path) -> DaemonSettings {
    DaemonSettings { watchd_socket: socket.to_path_buf(), ..DaemonSettings::default() }
}

fn dead_socket() -> PathBuf {
    std::env::temp_dir()
        .join("metafolder-tests")
        .join(format!("mf_no_watchd_{}.sock", Uuid::new_v4()))
}

// ── The choice at load ────────────────────────────────────────────────────────

#[test]
fn test_without_a_broker_the_notify_source_watches_and_says_so() {
    // No option anywhere: the socket is probed, and the fallback is announced
    // (spec-file-tracking "Watch sources and regimes"). The socket name is
    // unique per test, so the message is attributed exactly (the diagnostics
    // feed is process-wide and the tests run in parallel).
    let socket = dead_socket();
    let (repo, root) = setup("fallback", fanotify_settings(&socket));
    let before = metafolder_daemon::diagnostics::read(0, 1000).next_since;
    let executor = executor::spawn(&repo, Duration::from_millis(25));
    let handle = watcher::start(&repo, executor.pinger()).unwrap();
    assert_eq!(handle.backend(), "inotify", "no broker answers → the notify source");
    let needle = socket.display().to_string();
    let said = metafolder_daemon::diagnostics::read(before, 1000)
        .entries
        .into_iter()
        .any(|e| e.scope == "watcher" && e.message.contains(&needle));
    assert!(said, "the fallback names the socket it probed");
    std::fs::remove_dir_all(root).ok();
}

// ── The fanotify source through the pipeline ──────────────────────────────────

#[test]
fn test_events_from_the_broker_become_metarecords() {
    let socket = dead_socket(); // the stand-in binds this name
    let (repo, root) = setup("events", fanotify_settings(&socket));
    write_file(&root, "dir/x", b"hello");

    let _broker = FakeBroker::start(
        socket,
        vec![
            ServerMsg::Event { event: Event::Create { path: wire(&root, "dir/x") } },
            ServerMsg::Event { event: Event::ModifyData { path: wire(&root, "dir/x") } },
        ],
    );

    let executor = executor::spawn(&repo, Duration::from_millis(25));
    let handle = watcher::start(&repo, executor.pinger()).unwrap();
    assert_eq!(handle.backend(), "fanotify");
    wait_for("the broker's event to become a metarecord", || resolve(&repo, "/dir/x").is_some());
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn test_an_excluded_subtree_is_not_recorded_under_the_coverage_source() {
    // `mfr_watch_exceeded` means *leave this subtree uncovered* in every
    // regime (spec-file-tracking "Watch sources and regimes") — under coverage
    // the daemon drops what happens there instead of having no watch on it.
    let socket = dead_socket();
    let (repo, root) = setup("excluded", fanotify_settings(&socket));
    write_file(&root, "dir/x", b"x");
    write_file(&root, "other/y", b"y");
    write_file(&root, "other/moved", b"m");
    mark_exceeded(&repo, "/dir");

    let _broker = FakeBroker::start(
        socket,
        vec![
            ServerMsg::Event { event: Event::Create { path: wire(&root, "dir/x") } },
            ServerMsg::Event { event: Event::Create { path: wire(&root, "other/y") } },
            // A move out of the excluded dark: the daemon may see the arrival,
            // never the departure (the same shape as a move out of the watched
            // tree — a `RenameTo`).
            ServerMsg::Event {
                event: Event::Rename { from: wire(&root, "dir/x"), to: wire(&root, "other/moved") },
            },
        ],
    );

    let executor = executor::spawn(&repo, Duration::from_millis(25));
    let _handle = watcher::start(&repo, executor.pinger()).unwrap();
    wait_for("the visible events to land", || {
        resolve(&repo, "/other/y").is_some() && resolve(&repo, "/other/moved").is_some()
    });
    settle();
    assert!(resolve(&repo, "/dir/x").is_none(), "nothing under the excluded subtree is recorded");
    std::fs::remove_dir_all(root).ok();
}
