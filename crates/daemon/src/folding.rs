//! Folding in idle time (doc "Folding in idle time"): what writes to many
//! records left pending — a part's entries, a field's events, the trigrams
//! kept by metarecord — is written into the store once the repository has
//! been quiet for [`IDLE`], one list per step, the biggest first.
//!
//! Each step is a transaction of its own and holds the connection for that
//! one list: what waits for the store waits for one list, never for all.

use std::time::{Duration, Instant};

use crate::state::RepoState;
use crate::store::Begin;
use metafolder_core::sync::MutexExt;

/// How long without activity before the first list is folded.
pub const IDLE: Duration = Duration::from_secs(30);

/// How long after a fold before the next list is, if nothing happened since.
pub const NEXT: Duration = Duration::from_secs(1);

/// When a repository last had activity: a user action the GUI reported
/// (`POST /repos/:repo/activity`), or the load.
#[derive(Debug)]
pub struct Activity(std::sync::Mutex<Instant>);

impl Default for Activity {
    fn default() -> Self {
        Self(std::sync::Mutex::new(Instant::now()))
    }
}

impl Activity {
    pub fn note(&self) {
        *self.0.lock_recover() = Instant::now();
    }

    pub fn idle_for(&self) -> Duration {
        self.0.lock_recover().elapsed()
    }
}

/// Folds the biggest list pending if the repository has been idle for `idle`:
/// `None` when it has not, else whether there was a list to fold. What the
/// worker thread runs; tests call it directly.
pub fn step(repo: &RepoState, idle: Duration) -> anyhow::Result<Option<bool>> {
    if repo.activity.idle_for() < idle {
        return Ok(None);
    }
    if repo.is_revert_locked() || !repo.root_present() {
        return Ok(None);
    }
    let mut conn = metafolder_core::slowlog::timed("wait:conn", || repo.conn.lock_recover());
    match conn.as_kv_mut() {
        Some(kv) => Ok(Some(kv.fold_step()?)),
        None => Ok(Some(false)),
    }
}

enum Msg {
    Shutdown,
}

/// The folding thread; dropping it stops it.
pub struct FoldHandle {
    tx: std::sync::mpsc::Sender<Msg>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Drop for FoldHandle {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Shutdown);
        if let Some(join) = self.join.take() {
            if join.thread().id() != std::thread::current().id() {
                let _ = join.join();
            }
        }
    }
}

/// Starts the folding thread of `repo`, with a Weak reference like the hash
/// worker's: the repository owns the handle.
pub fn spawn_worker(repo: &std::sync::Arc<RepoState>) -> FoldHandle {
    let (tx, rx) = std::sync::mpsc::channel::<Msg>();
    let weak = std::sync::Arc::downgrade(repo);
    let join = std::thread::spawn(move || {
        let mut wait = IDLE;
        loop {
            match rx.recv_timeout(wait) {
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Ok(Msg::Shutdown) | Err(_) => return,
            }
            let Some(repo) = weak.upgrade() else { return };
            let _scope = crate::diagnostics::in_repo(repo.uuid());
            wait = match step(&repo, IDLE) {
                // A list was folded: the next one a moment later, if still quiet.
                Ok(Some(true)) => NEXT,
                // Nothing to fold, or not idle yet: look again once it could be.
                Ok(_) => IDLE.saturating_sub(repo.activity.idle_for()).max(NEXT),
                Err(err) => {
                    crate::diagnostics::error_for(
                        "folding",
                        format!("folding failed: {err:#}"),
                        repo.uuid(),
                    );
                    IDLE
                }
            };
        }
    });
    FoldHandle { tx, join: Some(join) }
}
