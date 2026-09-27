//! Disposable directories for the integration tests.
//!
//! Two things, for one reason: the suites used to scatter their throwaway
//! repositories directly in `$TMPDIR` and remove them with an explicit
//! `remove_dir_all` at the end of each test — which never runs when the test
//! panics or is interrupted. Thousands of runs later, 31 000 stale
//! repositories filled the disk, and a full disk is not a quiet failure: SQLite
//! answers "database or disk is full", the watcher's flush fails, and (before
//! its failure budget) retried that batch for ever.
//!
//! So: every test directory lives under one parent, and each one removes itself
//! when its guard goes out of scope — panic included.

#![allow(dead_code)] // each test binary uses its own subset

pub mod engines;
pub mod kv;
pub mod sqlcost;

/// The two watch sources a repository can run on (spec-file-tracking "Watch
/// sources and regimes"): the inotify source, and the fanotify broker
/// (`metafolder-watchd`) when one answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Regime {
    Inotify,
    Fanotify,
}

impl Regime {
    /// What `GET /repos/:repo/watch` reports as `backend` under this regime.
    pub fn backend(self) -> &'static str {
        match self {
            Regime::Inotify => "inotify",
            Regime::Fanotify => "fanotify",
        }
    }
}

/// Whether a fanotify broker answers at the daemon's default socket — the
/// condition for the `fanotify::` variants to run rather than skip.
pub fn broker_available() -> bool {
    std::os::unix::net::UnixStream::connect(metafolder_daemon::daemon_config::DEFAULT_WATCHD_SOCKET)
        .is_ok()
}

/// A daemon state for the tests that drive the live watcher, on `regime`.
///
/// The watch source is chosen, not inherited from the machine: the daemon takes
/// the broker whenever one answers at `watchd-socket`, so with the shipped
/// default a developer running `metafolder-watchd` would test fanotify and
/// everyone else inotify, under the same test names. `Inotify` points the
/// socket at a path where nothing listens; `Fanotify` keeps the default.
///
/// The quiet period is the 500 ms these suites were written and timed against,
/// not the shipped default (2 s, `DEFAULT_WATCH_QUIET_PERIOD_MS`). What they
/// check is what the watcher records, not how long it waits first — and their
/// settle budgets (a few seconds per step) leave the 2 s default no margin on
/// a loaded machine.
pub fn watching_state_on(regime: Regime) -> metafolder_daemon::state::AppState {
    let defaults = metafolder_daemon::daemon_config::DaemonSettings::default();
    let watchd_socket = match regime {
        Regime::Inotify => tests_root().join("no-broker-here.sock"),
        Regime::Fanotify => defaults.watchd_socket.clone(),
    };
    let settings = metafolder_daemon::daemon_config::DaemonSettings {
        watch_quiet_period_ms: 500,
        watchd_socket,
        ..defaults
    };
    metafolder_daemon::state::AppState::new().with_settings(settings)
}

/// [`watching_state_on`] the inotify source: the deterministic default for a
/// suite that does not run under both regimes.
pub fn watching_state() -> metafolder_daemon::state::AppState {
    watching_state_on(Regime::Inotify)
}

/// Runs each listed `async fn name(regime: Regime)` under both watch sources:
/// `inotify::name` always, `fanotify::name` when a broker answers — and a
/// skip, announced on stderr (`--nocapture` shows it), when none does.
#[macro_export]
macro_rules! on_both_regimes {
    ($($name:ident),* $(,)?) => {
        mod inotify {
            $(
                #[tokio::test(flavor = "multi_thread")]
                async fn $name() {
                    super::$name(super::common::Regime::Inotify).await
                }
            )*
        }
        mod fanotify {
            $(
                #[tokio::test(flavor = "multi_thread")]
                async fn $name() {
                    if !super::common::broker_available() {
                        eprintln!(
                            "SKIP fanotify::{}: no broker at {}",
                            stringify!($name),
                            metafolder_daemon::daemon_config::DEFAULT_WATCHD_SOCKET,
                        );
                        return;
                    }
                    super::$name(super::common::Regime::Fanotify).await
                }
            )*
        }
    };
}

use std::path::{Path, PathBuf};

use uuid::Uuid;

/// The single parent of every test directory, so whatever a crashed run leaves
/// behind is one `rm -rf "$TMPDIR/metafolder-tests"` away.
pub fn tests_root() -> PathBuf {
    std::env::temp_dir().join("metafolder-tests")
}

/// A directory that removes itself when dropped — at the end of the test, and
/// just as much when the test panics halfway through.
///
/// Derefs to [`Path`], so it is used exactly like the `PathBuf` it replaces
/// (`root.join(…)`, `&root` where a `&Path` is expected). Keep it bound for as
/// long as the directory is needed: `let _dir = TempDir::new(…)` drops it
/// immediately, `let dir = …` keeps it to the end of the scope.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Creates `$TMPDIR/metafolder-tests/<prefix>_<uuid>/`.
    pub fn new(prefix: &str) -> Self {
        let path = tests_root().join(format!("{prefix}_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create the test directory");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Best effort: the test may have removed it already, and a failure here
        // must never replace the test's own (more interesting) failure.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

impl std::ops::Deref for TempDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl std::fmt::Debug for TempDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.path.fmt(f)
    }
}

/// A file in its own self-removing directory. Derefs to the *file's* path, so
/// it is used exactly like the `PathBuf` it replaces.
pub struct TempFile {
    _dir: TempDir,
    path: PathBuf,
}

impl TempFile {
    /// Writes `content` to `$TMPDIR/metafolder-tests/<prefix>_<uuid>/file`.
    pub fn new(prefix: &str, content: &[u8]) -> Self {
        let dir = TempDir::new(prefix);
        let path = dir.join("file");
        std::fs::write(&path, content).expect("write the test file");
        Self { _dir: dir, path }
    }
}

impl std::ops::Deref for TempFile {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TempFile {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl std::fmt::Debug for TempFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.path.fmt(f)
    }
}
