//! Disposable directories for the integration tests.
//!
//! Two things, for one reason: the suites used to scatter their throwaway
//! repositories directly in `$TMPDIR` and remove them with an explicit
//! `remove_dir_all` at the end of each test — which never runs when the test
//! panics or is interrupted. Thousands of runs later, 31 000 stale
//! repositories filled the disk, and a full disk is not a quiet failure:
//! SQLite (the store then) answered "database or disk is full", the watcher's
//! flush failed, and (before its failure budget) retried that batch for ever.
//!
//! So: every test directory lives under one parent, and each one removes itself
//! when its guard goes out of scope — panic included.

#![allow(dead_code)] // each test binary uses its own subset

pub mod engines;
pub mod journey;
pub mod kv;

/// The two watch sources a repository can run on (doc "Watch sources and regimes"): the inotify
/// source, and the fanotify broker
/// (`metafolder-watchd`) when one answers — or the same broker over a
/// simulated kernel ([`sim_broker`]), which needs no privilege and so runs
/// everywhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Regime {
    Inotify,
    Fanotify,
    Simulated,
}

impl Regime {
    /// What `GET /repos/:repo/watch` reports as `backend` under this regime.
    pub fn backend(self) -> &'static str {
        match self {
            Regime::Inotify => "inotify",
            Regime::Fanotify | Regime::Simulated => "fanotify",
        }
    }

    /// How the test changes files under a watched root in this regime.
    pub fn fs(self) -> Fs {
        match self {
            Regime::Inotify | Regime::Fanotify => Fs::Std,
            Regime::Simulated => Fs::Sim(sim_broker().fs().clone()),
        }
    }
}

/// The process's one simulated broker: a fanotify broker whose kernel is
/// `metafolder_watchd::sim` — its filesystem is [`tests_root`], so every test
/// directory is on it, and every repository of the process subscribes to it,
/// as they would to the machine's one broker.
pub fn sim_broker() -> &'static metafolder_watchd::sim::SimBroker {
    static SIM: std::sync::OnceLock<metafolder_watchd::sim::SimBroker> = std::sync::OnceLock::new();
    SIM.get_or_init(|| {
        std::fs::create_dir_all(tests_root()).expect("the tests' root");
        let socket = tests_root().join(format!("sim-watchd-{}.sock", std::process::id()));
        metafolder_watchd::sim::SimBroker::start(&socket, &tests_root())
            .expect("the simulated broker starts")
    })
}

/// How a test changes files under a watched root: straight through `std::fs`
/// where a real source watches, through the simulated kernel otherwise — it
/// sees only what goes through it, like a filesystem nobody else touches.
#[derive(Clone)]
pub enum Fs {
    Std,
    Sim(metafolder_watchd::sim::SimFs),
}

impl Fs {
    pub fn write(&self, path: impl AsRef<Path>, data: impl AsRef<[u8]>) -> std::io::Result<()> {
        match self {
            Fs::Std => std::fs::write(path, data),
            Fs::Sim(sim) => sim.write(path, data),
        }
    }

    pub fn create_dir(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        match self {
            Fs::Std => std::fs::create_dir(path),
            Fs::Sim(sim) => sim.create_dir(path),
        }
    }

    pub fn create_dir_all(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        match self {
            Fs::Std => std::fs::create_dir_all(path),
            Fs::Sim(sim) => sim.create_dir_all(path),
        }
    }

    pub fn rename(&self, from: impl AsRef<Path>, to: impl AsRef<Path>) -> std::io::Result<()> {
        match self {
            Fs::Std => std::fs::rename(from, to),
            Fs::Sim(sim) => sim.rename(from, to),
        }
    }

    pub fn remove_file(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        match self {
            Fs::Std => std::fs::remove_file(path),
            Fs::Sim(sim) => sim.remove_file(path),
        }
    }

    pub fn remove_dir(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        match self {
            Fs::Std => std::fs::remove_dir(path),
            Fs::Sim(sim) => sim.remove_dir(path),
        }
    }

    pub fn remove_dir_all(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        match self {
            Fs::Std => std::fs::remove_dir_all(path),
            Fs::Sim(sim) => sim.remove_dir_all(path),
        }
    }

    pub fn set_permissions(
        &self,
        path: impl AsRef<Path>,
        perm: std::fs::Permissions,
    ) -> std::io::Result<()> {
        match self {
            Fs::Std => std::fs::set_permissions(path, perm),
            Fs::Sim(sim) => sim.set_permissions(path, perm),
        }
    }

    pub fn symlink(&self, target: impl AsRef<Path>, link: impl AsRef<Path>) -> std::io::Result<()> {
        match self {
            Fs::Std => std::os::unix::fs::symlink(target, link),
            Fs::Sim(sim) => sim.symlink(target, link),
        }
    }

    /// `mount -t tmpfs` over the directory `at`: a new, empty drive. Real
    /// mounts need a user namespace ([`in_userns`]).
    pub fn mount_tmpfs(&self, at: impl AsRef<Path>) -> std::io::Result<()> {
        match self {
            Fs::Std => run_mount(&["-t", "tmpfs", "t"], &[at.as_ref()]),
            Fs::Sim(sim) => sim.mount_tmpfs(at),
        }
    }

    /// `mount --bind`: the drive mounted at `from` mounted at `at` too.
    pub fn bind_mount(&self, from: impl AsRef<Path>, at: impl AsRef<Path>) -> std::io::Result<()> {
        match self {
            Fs::Std => run_mount(&["--bind"], &[from.as_ref(), at.as_ref()]),
            Fs::Sim(sim) => sim.bind_mount(from, at),
        }
    }

    /// `umount`.
    pub fn unmount(&self, at: impl AsRef<Path>) -> std::io::Result<()> {
        match self {
            Fs::Std => {
                let ok = std::process::Command::new("umount").arg(at.as_ref()).status()?;
                ok.success().then_some(()).ok_or_else(|| std::io::Error::other("umount failed"))
            }
            Fs::Sim(sim) => sim.unmount(at),
        }
    }

    /// Lets what is done meanwhile pile up before the broker reads it, as a
    /// broker that fell behind (the simulated kernel only; the real ones read
    /// when they read).
    pub fn hold(&self) -> Option<metafolder_watchd::sim::Hold<'_>> {
        match self {
            Fs::Std => None,
            Fs::Sim(sim) => Some(sim.hold()),
        }
    }
}

fn run_mount(args: &[&str], paths: &[&Path]) -> std::io::Result<()> {
    let ok = std::process::Command::new("mount").args(args).args(paths).status()?;
    ok.success().then_some(()).ok_or_else(|| std::io::Error::other("mount failed"))
}

/// `true` inside a new user and mount namespace (`unshare -rm`), where the
/// test may mount what it likes; outside, runs the test `test` (its name in
/// the binary) there, alone, asserts it passed, and returns `false`. Skipped,
/// and announced, where user namespaces are refused.
pub fn in_userns(test: &str) -> bool {
    const CHILD: &str = "METAFOLDER_DAEMON_TEST_USERNS";
    if std::env::var_os(CHILD).is_some() {
        return true;
    }
    let usable = std::process::Command::new("unshare")
        .args(["-rm", "true"])
        .status()
        .is_ok_and(|s| s.success());
    if !usable {
        eprintln!("SKIP {test}: user namespaces are not available here");
        return false;
    }
    let out = std::process::Command::new("unshare")
        .args(["-rm", "--"])
        .arg(std::env::current_exe().unwrap())
        .args([test, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .output()
        .unwrap();
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success() && text.contains("1 passed"), "{text}");
    false
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
        Regime::Simulated => sim_broker().socket().to_path_buf(),
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

/// Runs each listed `async fn name(regime: Regime)` under every watch source:
/// `inotify::name` and `simulated::name` always, `fanotify::name` when a
/// broker answers — and a skip, announced on stderr (`--nocapture` shows it),
/// when none does. The test changes files through `regime.fs()`.
#[macro_export]
macro_rules! on_every_regime {
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
        mod simulated {
            $(
                #[tokio::test(flavor = "multi_thread")]
                async fn $name() {
                    super::$name(super::common::Regime::Simulated).await
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

/// The shipped metadata map: a repository does not load without one (doc
/// "Repository configuration").
pub const METADATA_MAP: &str = include_str!("../../default-config/repo-default/metadata-map.toml");

/// What `mf repo init` installs before it asks the daemon: the repository's
/// configuration under `<root>/.metafolder/config/` (its metadata map; no
/// schema, so nothing is validated).
pub fn seed_config(root: &Path) {
    seed_config_at(&root.join(".metafolder"));
}

/// [`seed_config`] for a `.metafolder/` given directly (an external one).
pub fn seed_config_at(metafolder: &Path) {
    let config = metafolder.join("config");
    std::fs::create_dir_all(&config).expect("create the configuration directory");
    std::fs::write(config.join("metadata-map.toml"), METADATA_MAP).expect("seed the metadata map");
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

    /// [`TempDir::new`], holding the configuration a repository's root needs
    /// ([`seed_config`]).
    pub fn repo_root(prefix: &str) -> Self {
        let dir = Self::new(prefix);
        seed_config(&dir);
        dir
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
