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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Running,
    Done,
    Failed,
}

impl TaskStatus {
    /// True while the task is not yet terminal.
    pub fn is_active(self) -> bool {
        matches!(self, TaskStatus::Pending | TaskStatus::Running)
    }
}

/// Internal task record. Carries a wall-clock `started_at_ms` for display and
/// a monotonic `finished_instant` for retention/eviction.
#[derive(Debug, Clone)]
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
}
