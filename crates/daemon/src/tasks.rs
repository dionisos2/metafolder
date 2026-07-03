//! Background tasks and progress reporting (spec-tasks.org).
//!
//! A [`TaskRegistry`] is an in-memory, per-repository set of observable units
//! of work. It is deliberately *separate from the SQLite connection*: a
//! running reconcile holds the connection lock for its whole duration, so a
//! progress reader that touched the database would block behind it. Reading
//! the registry never touches the database.
//!
//! Tasks are never persisted; a daemon restart starts with an empty registry.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use metafolder_core::sync::MutexExt;
use serde::Serialize;
use uuid::Uuid;

/// How long a terminal (`done`/`failed`) task is retained before eviction.
pub const RETENTION: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Reconcile,
    Query,
    Flush,
    /// Warming a freshly loaded repository: populating the tree cache and
    /// building the in-memory query index. Observable so the GUI can show a
    /// load progress bar; not cancellable (a partial warmup just falls back to
    /// the DB, so there is nothing to roll back or refuse an unload for).
    Load,
}

impl TaskKind {
    /// Whether a task of this kind can be cancelled (spec-tasks "Cancellation").
    /// `flush` is internal and transient; `load` is a harmless warmup.
    pub fn is_cancellable(self) -> bool {
        !matches!(self, TaskKind::Flush | TaskKind::Load)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Running,
    Done,
    Failed,
    /// Stopped at the user's request (spec-tasks "Cancellation"). Terminal,
    /// distinct from `failed` so a deliberate stop is not read as an error.
    Cancelled,
}

impl TaskStatus {
    /// True while the task is not yet terminal.
    pub fn is_active(self) -> bool {
        matches!(self, TaskStatus::Pending | TaskStatus::Running)
    }
}

/// Outcome of a cancellation request, mapped to an HTTP status by the route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    /// The cooperative flag was set (and any registered canceller fired). The
    /// task becomes `cancelled` once the worker observes the request.
    Requested,
    /// The task is already terminal (`done` / `failed` / `cancelled`): nothing
    /// to stop. → `409`.
    AlreadyTerminal,
    /// This kind of task cannot be cancelled (currently `flush`). → `400`.
    NotCancellable,
    /// No such task (unknown id or already evicted). → `404`.
    NotFound,
}

/// Internal task record. Carries a wall-clock `started_at_ms` for display and
/// a monotonic `finished_instant` for retention/eviction.
///
/// Not `Clone`/`Debug`: it may own an `on_cancel` side-effect closure (e.g. the
/// SQLite interrupt handle for a query), which is neither.
struct Task {
    id: Uuid,
    repo_uuid: Uuid,
    kind: TaskKind,
    status: TaskStatus,
    phase: String,
    done: Option<u64>,
    total: Option<u64>,
    started_at_ms: i64,
    finished_at_ms: Option<i64>,
    result: Option<serde_json::Value>,
    error: Option<String>,
    /// Monotonic instant of completion; drives TTL eviction. `None` while active.
    finished_instant: Option<Instant>,
    /// Set by [`TaskRegistry::request_cancel`]; polled cooperatively by the
    /// worker (reconcile) at progress checkpoints. Guarded by the registry
    /// mutex like every other field, so a plain `bool` suffices.
    cancel_requested: bool,
    /// Optional side effect run *immediately* when cancellation is requested,
    /// for work that cannot poll a flag (a running query: the closure calls the
    /// connection's SQLite interrupt handle). `None` for cooperative kinds.
    on_cancel: Option<Box<dyn Fn() + Send + Sync>>,
}

/// Public, serializable view of a task (the JSON shape in spec-tasks.org).
#[derive(Debug, Clone, Serialize)]
pub struct TaskView {
    #[serde(with = "metafolder_core::metarecord::hex_uuid")]
    pub id: Uuid,
    #[serde(with = "metafolder_core::metarecord::hex_uuid")]
    pub repo_uuid: Uuid,
    pub kind: TaskKind,
    pub status: TaskStatus,
    pub phase: String,
    pub done: Option<u64>,
    pub total: Option<u64>,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
}

impl Task {
    fn view(&self) -> TaskView {
        TaskView {
            id: self.id,
            repo_uuid: self.repo_uuid,
            kind: self.kind,
            status: self.status,
            phase: self.phase.clone(),
            done: self.done,
            total: self.total,
            started_at: metafolder_core::date::iso8601_from_ms(self.started_at_ms),
            finished_at: self.finished_at_ms.map(metafolder_core::date::iso8601_from_ms),
            result: self.result.clone(),
            error: self.error.clone(),
        }
    }
}

/// Per-repository registry of observable tasks.
pub struct TaskRegistry {
    repo_uuid: Uuid,
    tasks: Mutex<HashMap<Uuid, Task>>,
}

impl TaskRegistry {
    pub fn new(repo_uuid: Uuid) -> Self {
        Self { repo_uuid, tasks: Mutex::new(HashMap::new()) }
    }

    /// Creates a new `pending` task and returns its id.
    pub fn start(&self, kind: TaskKind) -> Uuid {
        let id = Uuid::new_v4();
        let task = self.new_task(id, kind);
        let mut tasks = self.tasks.lock_recover();
        Self::evict_locked(&mut tasks, Instant::now());
        tasks.insert(id, task);
        id
    }

    /// Creates a new `pending` task only if no active (pending/running) task of
    /// the same kind exists for this repository. Returns `None` otherwise
    /// (used by reconcile to reject a concurrent run). Atomic check-and-insert.
    pub fn start_unique(&self, kind: TaskKind) -> Option<Uuid> {
        let id = Uuid::new_v4();
        let task = self.new_task(id, kind);
        let mut tasks = self.tasks.lock_recover();
        Self::evict_locked(&mut tasks, Instant::now());
        if tasks.values().any(|t| t.kind == kind && t.status.is_active()) {
            return None;
        }
        tasks.insert(id, task);
        Some(id)
    }

    /// Transitions a task to `running`. No-op if the id is unknown.
    pub fn mark_running(&self, id: Uuid) {
        if let Some(t) = self.tasks.lock_recover().get_mut(&id) {
            t.status = TaskStatus::Running;
        }
    }

    /// Updates a task's progress (phase + optional counts). No-op if unknown.
    pub fn set_progress(&self, id: Uuid, phase: &str, done: Option<u64>, total: Option<u64>) {
        if let Some(t) = self.tasks.lock_recover().get_mut(&id) {
            t.phase = phase.to_string();
            t.done = done;
            t.total = total;
        }
    }

    /// Marks a task `done`, attaching an optional result payload. No-op if unknown.
    pub fn finish(&self, id: Uuid, result: Option<serde_json::Value>) {
        if let Some(t) = self.tasks.lock_recover().get_mut(&id) {
            t.status = TaskStatus::Done;
            t.result = result;
            t.finished_at_ms = Some(metafolder_core::date::now_ms());
            t.finished_instant = Some(Instant::now());
        }
    }

    /// Marks a task `failed` with an error message. No-op if unknown.
    pub fn fail(&self, id: Uuid, error: &str) {
        if let Some(t) = self.tasks.lock_recover().get_mut(&id) {
            t.status = TaskStatus::Failed;
            t.error = Some(error.to_string());
            t.finished_at_ms = Some(metafolder_core::date::now_ms());
            t.finished_instant = Some(Instant::now());
        }
    }

    /// Marks a task `cancelled` (terminal). Called by a worker once it observes
    /// that cancellation was requested and has unwound (rolling back its
    /// transaction). No-op if unknown.
    pub fn mark_cancelled(&self, id: Uuid) {
        if let Some(t) = self.tasks.lock_recover().get_mut(&id) {
            t.status = TaskStatus::Cancelled;
            t.finished_at_ms = Some(metafolder_core::date::now_ms());
            t.finished_instant = Some(Instant::now());
        }
    }

    /// Requests cancellation of a task: sets the cooperative flag and fires any
    /// registered [`on_cancel`](Task::on_cancel) side effect (e.g. interrupting
    /// a running query). The task only becomes `cancelled` once its worker
    /// observes the request and unwinds. See [`CancelOutcome`].
    pub fn request_cancel(&self, id: Uuid) -> CancelOutcome {
        let mut tasks = self.tasks.lock_recover();
        Self::evict_locked(&mut tasks, Instant::now());
        let Some(t) = tasks.get_mut(&id) else {
            return CancelOutcome::NotFound;
        };
        if !t.status.is_active() {
            return CancelOutcome::AlreadyTerminal;
        }
        if !t.kind.is_cancellable() {
            return CancelOutcome::NotCancellable;
        }
        t.cancel_requested = true;
        if let Some(on_cancel) = &t.on_cancel {
            on_cancel();
        }
        CancelOutcome::Requested
    }

    /// Whether cancellation has been requested for a task. Polled by cooperative
    /// workers; `false` for an unknown task.
    pub fn is_cancel_requested(&self, id: Uuid) -> bool {
        self.tasks.lock_recover().get(&id).is_some_and(|t| t.cancel_requested)
    }

    /// Whether any *cancellable* task (reconcile/query) is currently active.
    /// Used to refuse unloading a repository out from under in-flight work — the
    /// user is asked to stop the task first. Transient `flush` tasks are ignored.
    pub fn has_active_cancellable(&self) -> bool {
        let mut tasks = self.tasks.lock_recover();
        Self::evict_locked(&mut tasks, Instant::now());
        tasks.values().any(|t| t.status.is_active() && t.kind.is_cancellable())
    }

    /// Whether a `load` warmup task is currently active. Used to refuse an
    /// unload while the repository is still warming (the warmup holds the
    /// connection, so removing the repo would leave its database locked with
    /// nothing reachable to wait on). `load` is not cancellable, so it is not
    /// covered by [`Self::has_active_cancellable`].
    pub fn has_active_load(&self) -> bool {
        self.active_id(TaskKind::Load).is_some()
    }

    /// The id of the active (pending/running) task of `kind`, if any. Together
    /// with [`Self::start_unique`] there is at most one; used by the load route
    /// to hand a redundant load the already-running warmup's task id.
    pub fn active_id(&self, kind: TaskKind) -> Option<Uuid> {
        let mut tasks = self.tasks.lock_recover();
        Self::evict_locked(&mut tasks, Instant::now());
        tasks.values().find(|t| t.status.is_active() && t.kind == kind).map(|t| t.id)
    }

    /// Registers an `on_cancel` side effect for a task (e.g. a closure capturing
    /// a query's SQLite interrupt handle). No-op if unknown.
    pub fn set_canceller(&self, id: Uuid, on_cancel: Box<dyn Fn() + Send + Sync>) {
        if let Some(t) = self.tasks.lock_recover().get_mut(&id) {
            t.on_cancel = Some(on_cancel);
        }
    }

    /// Returns one task's view, or `None` if unknown or already evicted.
    pub fn get(&self, id: Uuid) -> Option<TaskView> {
        let mut tasks = self.tasks.lock_recover();
        Self::evict_locked(&mut tasks, Instant::now());
        tasks.get(&id).map(Task::view)
    }

    /// Lists all tasks currently retained, oldest first. Reads are idempotent
    /// (never delete).
    pub fn list(&self) -> Vec<TaskView> {
        let mut tasks = self.tasks.lock_recover();
        Self::evict_locked(&mut tasks, Instant::now());
        let mut views: Vec<TaskView> = tasks.values().map(Task::view).collect();
        views.sort_by(|a, b| a.started_at.cmp(&b.started_at).then(a.id.cmp(&b.id)));
        views
    }

    /// Removes terminal tasks older than [`RETENTION`] relative to `now`.
    /// Exposed for deterministic testing; the public methods call it with
    /// `Instant::now()`.
    #[cfg(test)]
    pub(crate) fn evict_expired(&self, now: Instant) {
        Self::evict_locked(&mut self.tasks.lock_recover(), now);
    }

    fn new_task(&self, id: Uuid, kind: TaskKind) -> Task {
        Task {
            id,
            repo_uuid: self.repo_uuid,
            kind,
            status: TaskStatus::Pending,
            phase: String::new(),
            done: None,
            total: None,
            started_at_ms: metafolder_core::date::now_ms(),
            finished_at_ms: None,
            result: None,
            error: None,
            finished_instant: None,
            cancel_requested: false,
            on_cancel: None,
        }
    }

    fn evict_locked(tasks: &mut HashMap<Uuid, Task>, now: Instant) {
        tasks.retain(|_, t| match t.finished_instant {
            Some(done_at) => now.saturating_duration_since(done_at) < RETENTION,
            None => true,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg() -> TaskRegistry {
        TaskRegistry::new(Uuid::new_v4())
    }

    #[test]
    fn start_creates_a_pending_task() {
        let r = reg();
        let id = r.start(TaskKind::Query);
        let t = r.get(id).expect("task present");
        assert_eq!(t.id, id);
        assert_eq!(t.status, TaskStatus::Pending);
        assert_eq!(t.kind, TaskKind::Query);
        assert!(t.finished_at.is_none());
        assert!(t.result.is_none());
        assert!(t.error.is_none());
    }

    #[test]
    fn view_carries_repo_uuid_and_iso_started_at() {
        let repo = Uuid::new_v4();
        let r = TaskRegistry::new(repo);
        let id = r.start(TaskKind::Reconcile);
        let t = r.get(id).unwrap();
        assert_eq!(t.repo_uuid, repo);
        // ISO-8601 UTC, e.g. 2026-06-21T11:30:00.000Z
        assert!(t.started_at.ends_with('Z'), "started_at = {}", t.started_at);
        assert!(t.started_at.contains('T'));
    }

    #[test]
    fn lifecycle_running_progress_done() {
        let r = reg();
        let id = r.start(TaskKind::Reconcile);
        r.mark_running(id);
        assert_eq!(r.get(id).unwrap().status, TaskStatus::Running);
        r.set_progress(id, "fingerprint", Some(3), Some(10));
        let t = r.get(id).unwrap();
        assert_eq!(t.phase, "fingerprint");
        assert_eq!(t.done, Some(3));
        assert_eq!(t.total, Some(10));
        r.finish(id, Some(serde_json::json!({"created": 2})));
        let t = r.get(id).unwrap();
        assert_eq!(t.status, TaskStatus::Done);
        assert_eq!(t.result, Some(serde_json::json!({"created": 2})));
        assert!(t.finished_at.is_some());
    }

    #[test]
    fn fail_records_error_and_is_terminal() {
        let r = reg();
        let id = r.start(TaskKind::Query);
        r.fail(id, "boom");
        let t = r.get(id).unwrap();
        assert_eq!(t.status, TaskStatus::Failed);
        assert_eq!(t.error.as_deref(), Some("boom"));
        assert!(t.finished_at.is_some());
    }

    #[test]
    fn start_unique_rejects_a_second_active_task_of_same_kind() {
        let r = reg();
        let first = r.start_unique(TaskKind::Reconcile).expect("first allowed");
        assert!(r.start_unique(TaskKind::Reconcile).is_none(), "second rejected");
        // A different kind is unaffected.
        assert!(r.start_unique(TaskKind::Query).is_some());
        // Once the first is terminal, a new reconcile is allowed again.
        r.finish(first, None);
        assert!(r.start_unique(TaskKind::Reconcile).is_some());
    }

    #[test]
    fn unknown_id_get_is_none_and_mutators_are_noops() {
        let r = reg();
        let ghost = Uuid::new_v4();
        assert!(r.get(ghost).is_none());
        // Must not panic.
        r.mark_running(ghost);
        r.set_progress(ghost, "x", None, None);
        r.finish(ghost, None);
        r.fail(ghost, "x");
    }

    #[test]
    fn list_returns_all_active_tasks() {
        let r = reg();
        let a = r.start(TaskKind::Reconcile);
        let b = r.start(TaskKind::Query);
        let ids: Vec<Uuid> = r.list().into_iter().map(|t| t.id).collect();
        assert!(ids.contains(&a));
        assert!(ids.contains(&b));
        assert_eq!(ids.len(), 2);
    }

    #[test]
    fn terminal_tasks_are_evicted_after_retention() {
        let r = reg();
        let id = r.start(TaskKind::Reconcile);
        r.finish(id, None);
        // Still present just after completion.
        assert!(r.get(id).is_some());
        // Present right before the TTL elapses.
        r.evict_expired(Instant::now() + RETENTION - Duration::from_millis(1));
        assert!(r.get(id).is_some());
        // Gone once the TTL elapses.
        r.evict_expired(Instant::now() + RETENTION + Duration::from_millis(1));
        assert!(r.get(id).is_none());
    }

    #[test]
    fn active_tasks_are_never_evicted() {
        let r = reg();
        let id = r.start(TaskKind::Reconcile);
        r.mark_running(id);
        r.evict_expired(Instant::now() + RETENTION * 100);
        assert!(r.get(id).is_some(), "running task must survive eviction");
    }

    #[test]
    fn reads_are_idempotent() {
        let r = reg();
        let id = r.start(TaskKind::Query);
        r.finish(id, None);
        assert!(r.get(id).is_some());
        assert!(r.get(id).is_some(), "a read must not delete the task");
        assert_eq!(r.list().len(), 1);
    }

    #[test]
    fn request_cancel_on_active_reconcile_sets_the_flag() {
        let r = reg();
        let id = r.start(TaskKind::Reconcile);
        assert!(!r.is_cancel_requested(id));
        assert_eq!(r.request_cancel(id), CancelOutcome::Requested);
        assert!(r.is_cancel_requested(id), "the cooperative flag is now set");
        // The task is not terminal yet: the worker flips it to `cancelled` once
        // it observes the flag.
        assert!(r.get(id).unwrap().status.is_active());
    }

    #[test]
    fn request_cancel_fires_the_registered_canceller() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let r = reg();
        let id = r.start(TaskKind::Query);
        let fired = Arc::new(AtomicBool::new(false));
        let flag = fired.clone();
        r.set_canceller(id, Box::new(move || flag.store(true, Ordering::SeqCst)));
        assert_eq!(r.request_cancel(id), CancelOutcome::Requested);
        assert!(fired.load(Ordering::SeqCst), "the canceller (e.g. sqlite interrupt) ran");
    }

    #[test]
    fn request_cancel_rejects_flush_and_terminal_and_unknown() {
        let r = reg();
        let flush = r.start(TaskKind::Flush);
        assert_eq!(r.request_cancel(flush), CancelOutcome::NotCancellable);

        let done = r.start(TaskKind::Reconcile);
        r.finish(done, None);
        assert_eq!(r.request_cancel(done), CancelOutcome::AlreadyTerminal);

        assert_eq!(r.request_cancel(Uuid::new_v4()), CancelOutcome::NotFound);
    }

    #[test]
    fn has_active_cancellable_ignores_flush_and_terminal() {
        let r = reg();
        assert!(!r.has_active_cancellable(), "empty registry");

        // An active flush does not count (transient, internal).
        let flush = r.start(TaskKind::Flush);
        r.mark_running(flush);
        assert!(!r.has_active_cancellable(), "flush is ignored");

        // An active reconcile counts.
        let rec = r.start(TaskKind::Reconcile);
        r.mark_running(rec);
        assert!(r.has_active_cancellable(), "running reconcile counts");

        // Once it is terminal, it no longer counts.
        r.finish(rec, None);
        assert!(!r.has_active_cancellable(), "finished reconcile does not count");

        // A pending query also counts (it is active).
        r.start(TaskKind::Query);
        assert!(r.has_active_cancellable(), "pending query counts");
    }

    #[test]
    fn active_id_returns_the_active_task_of_kind_only() {
        let r = reg();
        assert_eq!(r.active_id(TaskKind::Load), None, "empty registry");
        let id = r.start(TaskKind::Load);
        assert_eq!(r.active_id(TaskKind::Load), Some(id), "pending counts");
        assert_eq!(r.active_id(TaskKind::Reconcile), None, "other kinds unaffected");
        r.mark_running(id);
        assert_eq!(r.active_id(TaskKind::Load), Some(id), "running counts");
        r.finish(id, None);
        assert_eq!(r.active_id(TaskKind::Load), None, "terminal does not count");
    }

    #[test]
    fn mark_cancelled_is_terminal_and_evictable() {
        let r = reg();
        let id = r.start(TaskKind::Reconcile);
        r.mark_running(id);
        r.request_cancel(id);
        r.mark_cancelled(id);
        let t = r.get(id).unwrap();
        assert_eq!(t.status, TaskStatus::Cancelled);
        assert!(t.finished_at.is_some());
        assert!(!t.status.is_active());
        // Subject to the same retention as done/failed.
        r.evict_expired(Instant::now() + RETENTION + Duration::from_millis(1));
        assert!(r.get(id).is_none());
    }
}
