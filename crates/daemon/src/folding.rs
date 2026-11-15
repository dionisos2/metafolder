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

/// What the repository has been doing: the operations in flight, and when it
/// last had activity — a user action the GUI reported, the end of an operation,
/// or the load. The quiet a fold waits for is counted from the last of these,
/// and never begins while an operation is under way: one that lasts a minute
/// does not end inside a quiet period.
#[derive(Debug)]
pub struct Activity(std::sync::Mutex<State>);

#[derive(Debug)]
struct State {
    in_flight: usize,
    last: Instant,
}

impl Default for Activity {
    fn default() -> Self {
        Self(std::sync::Mutex::new(State { in_flight: 0, last: Instant::now() }))
    }
}

impl Activity {
    pub fn note(&self) {
        self.note_at(Instant::now());
    }

    /// Activity that happened at `when` (tests set it back).
    pub fn note_at(&self, when: Instant) {
        self.0.lock_recover().last = when;
    }

    /// Zero while an operation is in flight, else the time since the last
    /// activity.
    pub fn idle_for(&self) -> Duration {
        let state = self.0.lock_recover();
        if state.in_flight > 0 {
            Duration::ZERO
        } else {
            state.last.elapsed()
        }
    }
}

/// An operation under way on a repository: held for its whole duration; the
/// quiet starts when it is dropped. It holds the repository's activity and not
/// the repository, which an unload waits to be the last reference to.
pub struct Operation(std::sync::Arc<Activity>);

/// Starts counting an operation of `repo` as activity.
pub fn begin_operation(repo: &RepoState) -> Operation {
    repo.activity.0.lock_recover().in_flight += 1;
    Operation(std::sync::Arc::clone(&repo.activity))
}

impl Drop for Operation {
    fn drop(&mut self) {
        let mut state = self.0 .0.lock_recover();
        state.in_flight -= 1;
        state.last = Instant::now();
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

/// Folds every list pending now, one at a time — the connection is free
/// between two — whatever the repository's activity: what `POST /repos/:repo/fold`
/// asks for. How many lists it folded.
pub fn fold_all(repo: &RepoState) -> anyhow::Result<usize> {
    let mut folded = 0;
    while step(repo, Duration::ZERO)? == Some(true) {
        folded += 1;
    }
    Ok(folded)
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
