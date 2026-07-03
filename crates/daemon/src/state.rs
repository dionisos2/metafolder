//! In-memory daemon state: the set of loaded repositories.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use metafolder_core::sync::MutexExt;
use rusqlite::Connection;
use serde::Serialize;
use uuid::Uuid;

use crate::config::RepoConfig;
use crate::daemon_config::DaemonSettings;
use crate::error::ApiError;
use crate::repo::{self, OpenedRepo, RepoLocator};
use crate::tree_cache::TreeCache;

/// One loaded repository. The SQLite connection and the tree cache each sit
/// behind their own mutex; blocking work runs in `spawn_blocking`.
pub struct RepoState {
    pub conn: Mutex<Connection>,
    pub cache: Mutex<TreeCache>,
    pub config: RepoConfig,
    /// The repository's display name. Starts at `config.name` but is mutable
    /// (rename, spec-main "PATCH /repos/:repo") — persisted to `config.json` and
    /// the single source of truth for uniqueness and the repo listing.
    pub name: Mutex<String>,
    pub metafolder_dir: PathBuf,
    pub case_insensitive: bool,
    /// Watcher + executor; None until started (or in unit tests).
    pub handles: Mutex<Option<RepoHandles>>,
    /// Loaded user schema; replaced atomically on reload (spec-schema).
    pub schema: Mutex<Option<crate::schema::CompiledSchema>>,
    /// Coordinated-rollback lock (spec-event-log): `Some` while a rollback
    /// navigation is in progress, carrying its resolved target. Never
    /// persisted — a crash restarts unlocked.
    pub rollback_lock: Mutex<Option<RollbackLock>>,
    /// Observable background tasks for this repository (spec-tasks). In memory,
    /// separate from `conn` so progress reads never block behind a running
    /// reconcile.
    pub tasks: crate::tasks::TaskRegistry,
    /// Derived in-memory query accelerator (spec-indexing). Rebuilt from the
    /// `field` table whenever the log HEAD it was built at no longer matches
    /// the current HEAD (it carries no incremental maintenance yet), and only
    /// consulted while fresh — so it never serves stale results. `None` until
    /// the first query builds it.
    pub index: Mutex<Option<crate::index::RepoIndex>>,
    /// Serializes read-modify-write cycles on the input-history files under
    /// `internal/history/` (crate::history). In-process is sufficient: the
    /// exclusive SQLite lock guarantees one daemon per repository.
    pub history_lock: Mutex<()>,
}

/// State of an in-progress coordinated rollback navigation.
pub struct RollbackLock {
    /// Resolved target operation id; `None` is the empty state.
    pub target: Option<i64>,
}

impl RepoState {
    /// Absolute path of `.metafolder/internal/` — the only part of the
    /// repository excluded from tracking (watcher and reconcile).
    pub fn internal_dir(&self) -> PathBuf {
        self.metafolder_dir.join(repo::INTERNAL_DIR)
    }

    /// Builds a `RepoState` with the default daemon settings (used by tests and
    /// by [`Self::from_opened_with`]).
    pub fn from_opened(opened: OpenedRepo) -> Self {
        Self::from_opened_with(opened, &DaemonSettings::default())
    }

    /// Builds a `RepoState`, applying the tunable daemon settings (here, the
    /// tree-cache node budget).
    pub fn from_opened_with(opened: OpenedRepo, settings: &DaemonSettings) -> Self {
        let repo_uuid = opened.config.repo_uuid;
        let name = Mutex::new(opened.config.name.clone());
        Self {
            conn: Mutex::new(opened.conn),
            cache: Mutex::new(TreeCache::with_limit(
                opened.case_insensitive,
                settings.tree_cache_max_nodes,
            )),
            config: opened.config,
            name,
            metafolder_dir: opened.metafolder_dir,
            case_insensitive: opened.case_insensitive,
            handles: Mutex::new(None),
            schema: Mutex::new(None),
            rollback_lock: Mutex::new(None),
            tasks: crate::tasks::TaskRegistry::new(repo_uuid),
            index: Mutex::new(None),
            history_lock: Mutex::new(()),
        }
    }

    /// The repository's current (mutable) display name.
    pub fn name(&self) -> String {
        self.name.lock_recover().clone()
    }

    /// This repository's listing info (the `GET /repos` / `GET /repos/:repo`
    /// shape), reading the live name.
    pub fn info(&self) -> RepoInfo {
        RepoInfo {
            repo_uuid: self.config.repo_uuid,
            name: self.name(),
            root: self.config.root.clone(),
            internal_dir: self.internal_dir(),
            created_at: self.config.created_at,
        }
    }

    /// Renames the repository: rewrites `config.json` with the new name, then
    /// swaps the in-memory name. Uniqueness is enforced by the caller
    /// ([`AppState::rename_repo`]).
    pub fn rename(&self, new_name: String) -> anyhow::Result<()> {
        let cfg = RepoConfig { name: new_name.clone(), ..self.config.clone() };
        cfg.write(&self.metafolder_dir)?;
        *self.name.lock_recover() = new_name;
        Ok(())
    }

    /// Locks the tree cache, recovering from a poisoned mutex. Unlike the
    /// connection (whose writes are transactional, so a panic mid-write is
    /// already rolled back), the in-memory cache can be left half-updated by a
    /// panic — and out of step with the rolled-back write — so its contents
    /// are discarded on recovery; it repopulates lazily from the DB. The
    /// poison flag is cleared so later locks take the normal fast path.
    /// See `docs/review-followups.md` (#5).
    pub fn lock_cache(&self) -> MutexGuard<'_, TreeCache> {
        match self.cache.lock() {
            Ok(guard) => guard,
            Err(poison) => {
                self.cache.clear_poison();
                let mut guard = poison.into_inner();
                guard.clear();
                guard
            }
        }
    }

    /// True while a coordinated rollback navigation holds the lock.
    pub fn is_rollback_locked(&self) -> bool {
        self.rollback_lock.lock_recover().is_some()
    }

    /// Warms the in-memory accelerators of a freshly loaded repository:
    /// eagerly populates the tree cache (so tree navigation is served from
    /// memory — spec-file-tracking "Tree Cache") and builds the query index (so
    /// the first query pays no build cost — spec-indexing). Both are
    /// best-effort: a failure just leaves the repository in DB-fallback mode,
    /// which is correct, only slower. `progress` reports `(phase, done, total)`
    /// for the load progress bar; it is a no-op for the synchronous callers
    /// (startup auto-load, `init`).
    ///
    /// Holds the connection for its duration (a single bulk read), so queries
    /// on this repository wait until it finishes — the load progress bar tells
    /// the user why. Idempotent enough: re-running on an already-warm repo just
    /// rebuilds, so callers skip it when [`TreeCache::is_complete`] already holds.
    pub fn warmup(&self, progress: &dyn Fn(&str, Option<u64>, Option<u64>)) {
        let conn = self.conn.lock_recover();
        progress("tree cache", None, None);
        if let Err(e) = self.lock_cache().populate(&conn) {
            eprintln!("warning: failed to populate tree cache for {}: {e}", self.config.repo_uuid);
            return;
        }
        match crate::index::RepoIndex::build_reported(&conn, self.config.repo_uuid, &|done, total| {
            progress("index", Some(done), Some(total));
        }) {
            Ok(index) => *self.index.lock_recover() = Some(index),
            Err(e) => {
                eprintln!("warning: failed to build query index for {}: {e}", self.config.repo_uuid)
            }
        }
    }

    /// Rejects a metadata write with `423 Locked` while a rollback navigation
    /// is in progress (spec-event-log "Rollback lock").
    pub fn ensure_writable(&self) -> Result<(), ApiError> {
        if self.is_rollback_locked() {
            Err(ApiError::locked(
                "repository is in rollback lock; complete or abort the navigation first",
            ))
        } else {
            Ok(())
        }
    }
}

/// Background machinery of a loaded repository. Held by the RepoState so it
/// is dropped (watcher stopped, executor joined) when the repo is unloaded.
pub struct RepoHandles {
    pub watcher: crate::watcher::WatcherHandle,
    pub executor: crate::executor::ExecutorHandle,
}

#[derive(Default)]
pub struct AppState {
    repos: Mutex<HashMap<Uuid, Arc<RepoState>>>,
    /// Shipped default schema copied into each new repo at init (spec-schema).
    /// `None` (the default, used by tests) disables seeding.
    seed_schema_path: Option<PathBuf>,
    /// Tunable UX/performance settings from `config.toml`'s `[settings]`, applied
    /// to every repository this state opens (tree-cache budget, watcher quiet
    /// period). Defaults when unset (tests, no config file).
    settings: DaemonSettings,
}

/// Public description of a loaded repository (`GET /repos`).
#[derive(Debug, Serialize)]
pub struct RepoInfo {
    #[serde(with = "metafolder_core::metarecord::hex_uuid")]
    pub repo_uuid: Uuid,
    pub name: String,
    pub root: PathBuf,
    /// `.metafolder/internal/`, always excluded from tracking; exposed so
    /// clients can flag it without guessing the metafolder location.
    pub internal_dir: PathBuf,
    pub created_at: u64,
}

impl AppState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Configures the shipped default schema seeded into each new repo at init
    /// (`<config>/daemon/schema.default.json`). `None` disables seeding.
    pub fn with_seed_schema(mut self, path: Option<PathBuf>) -> Self {
        self.seed_schema_path = path;
        self
    }

    /// Sets the tunable settings (`config.toml` `[settings]`) applied to every
    /// repository this state opens.
    pub fn with_settings(mut self, settings: DaemonSettings) -> Self {
        self.settings = settings;
        self
    }

    /// Initialises a new repository and registers it as loaded.
    pub fn init_repo(
        &self,
        root: &Path,
        metafolder: Option<&Path>,
        name: Option<&str>,
    ) -> Result<Uuid, ApiError> {
        let opened = repo::init_repository(root, metafolder, name)?;
        let uuid = opened.config.repo_uuid;
        self.ensure_name_available(&opened.config.name)?;
        // Seed the per-repo schema from the shipped default (best-effort),
        // before activate() reads it.
        if let Some(src) = self.seed_schema_path.as_deref() {
            repo::seed_schema_file(&opened.metafolder_dir, src);
        }
        let repo_state = self.activate(Arc::new(RepoState::from_opened_with(
            opened,
            &self.settings,
        )))?;
        // A fresh repository is tiny, so warm it synchronously (no progress bar).
        repo_state.warmup(&|_, _, _| {});
        self.repos.lock_recover().insert(uuid, repo_state);
        Ok(uuid)
    }

    /// Loads the user schema (an invalid schema file fails the load with
    /// 400), replays any pending buffer left by a previous run, then starts
    /// the watcher and its executor (spec: the buffer is replayed before the
    /// repository serves requests).
    fn activate(&self, repo_state: Arc<RepoState>) -> Result<Arc<RepoState>, ApiError> {
        let schema =
            crate::schema::load_for_repo(&repo_state.metafolder_dir, &repo_state.config)
                .map_err(ApiError::bad_request)?;
        *repo_state.schema.lock_recover() = schema;
        crate::executor::flush_pending(&repo_state)?;
        let quiet = self.settings.watch_quiet_period();
        let executor = crate::executor::spawn(&repo_state, quiet);
        let watcher = crate::watcher::start(&repo_state, executor.pinger())?;
        *repo_state.handles.lock_recover() = Some(RepoHandles { watcher, executor });
        Ok(repo_state)
    }

    /// Loads an existing repository. Loading an already-loaded repository is
    /// idempotent and returns its UUID (the exclusive SQLite lock would make
    /// a second real open fail anyway).
    pub fn load_repo(&self, locator: RepoLocator) -> Result<Uuid, ApiError> {
        let metafolder_dir = match &locator {
            RepoLocator::Root(root) => root
                .canonicalize()
                .map_err(|_| {
                    ApiError::bad_request(format!(
                        "Cannot resolve path {root:?}: the root directory must exist"
                    ))
                })?
                .join(".metafolder"),
            RepoLocator::Metafolder(dir) => dir.clone(),
        };
        if RepoConfig::exists(&metafolder_dir) {
            let config = RepoConfig::read(&metafolder_dir)?;
            if self.repos.lock_recover().contains_key(&config.repo_uuid) {
                return Ok(config.repo_uuid);
            }
        }
        let opened = repo::load_repository(RepoLocator::Metafolder(metafolder_dir))?;
        let uuid = opened.config.repo_uuid;
        self.ensure_name_available(&opened.config.name)?;
        let repo_state = self.activate(Arc::new(RepoState::from_opened_with(
            opened,
            &self.settings,
        )))?;
        self.repos.lock_recover().insert(uuid, repo_state);
        Ok(uuid)
    }

    /// Rejects a name already held by a loaded repository — names are unique
    /// among loaded repos, so the CLI's `-n <name>` selector resolves to exactly
    /// one UUID (spec-main "Global selection flags").
    fn ensure_name_available(&self, name: &str) -> Result<(), ApiError> {
        if self.repos.lock_recover().values().any(|r| r.name() == name) {
            return Err(ApiError::conflict(format!(
                "a repository named '{name}' is already loaded; names must be unique"
            )));
        }
        Ok(())
    }

    /// Unloads a repository: removes it from the loaded set, stops its watcher
    /// and executor, and releases the exclusive SQLite lock — so it can be
    /// re-loaded or opened by another daemon (spec-main "Repository management").
    ///
    /// An unknown repository is a 404 (no idempotency claimed). The unload is
    /// refused with 409 if:
    /// - a coordinated-rollback navigation is in progress (its lock must not be
    ///   silently dropped — complete or abort it first), or
    /// - a cancellable task (reconcile/query) is in flight: the caller is asked
    ///   to stop it first (`POST …/tasks/:id/cancel`), so the repository is
    ///   never pulled out from under running work. Transient `flush` tasks do
    ///   not block the unload.
    /// - a `load` warmup is in flight: it holds the connection, so the unload
    ///   waits for it to finish (warmup is not cancellable).
    pub fn unload_repo(&self, repo_uuid: Uuid) -> Result<(), ApiError> {
        let removed = {
            let mut repos = self.repos.lock_recover();
            let Some(repo_state) = repos.get(&repo_uuid) else {
                return Err(ApiError::not_found(format!("Repository not found: {repo_uuid}")));
            };
            if repo_state.is_rollback_locked() {
                return Err(ApiError::conflict(
                    "repository is in rollback lock; complete or abort the navigation first",
                ));
            }
            if repo_state.tasks.has_active_cancellable() {
                return Err(ApiError::conflict(
                    "a task is in progress; stop it first, then unload",
                ));
            }
            if repo_state.tasks.has_active_load() {
                // The warmup holds the connection; removing the repo now would
                // leave its database locked with no reachable task to wait on.
                return Err(ApiError::conflict(
                    "repository is warming up; wait for the load to finish, then unload",
                ));
            }
            repos.remove(&repo_uuid)
            // The `repos` guard is released at the end of this block, before the
            // `Arc` is dropped below.
        };
        // Dropping the last `Arc` runs `RepoHandles::drop` (watcher stopped,
        // executor joined) and closes the connection (releasing the lock). Done
        // outside the map lock so the executor-thread join cannot block another
        // repository operation that needs the map.
        drop(removed);
        Ok(())
    }

    /// Fetches a loaded repository or fails with 404.
    pub fn repo(&self, repo_uuid: Uuid) -> Result<Arc<RepoState>, ApiError> {
        self.repos
            .lock_recover()
            .get(&repo_uuid)
            .cloned()
            .ok_or_else(|| ApiError::not_found(format!("Repository not found: {repo_uuid}")))
    }

    pub fn list_repos(&self) -> Vec<RepoInfo> {
        let repos = self.repos.lock_recover();
        let mut infos: Vec<RepoInfo> = repos.values().map(|r| r.info()).collect();
        infos.sort_by_key(|i| i.repo_uuid);
        infos
    }

    /// One loaded repository's info, or 404.
    pub fn repo_info(&self, repo_uuid: Uuid) -> Result<RepoInfo, ApiError> {
        Ok(self.repo(repo_uuid)?.info())
    }

    /// Renames a loaded repository, keeping names unique among loaded repos
    /// (409 on clash) and persisting to `config.json`.
    pub fn rename_repo(&self, repo_uuid: Uuid, new_name: &str) -> Result<RepoInfo, ApiError> {
        let target = {
            let repos = self.repos.lock_recover();
            if repos.iter().any(|(u, r)| *u != repo_uuid && r.name() == new_name) {
                return Err(ApiError::conflict(format!(
                    "a repository named '{new_name}' is already loaded; names must be unique"
                )));
            }
            repos
                .get(&repo_uuid)
                .cloned()
                .ok_or_else(|| ApiError::not_found(format!("Repository not found: {repo_uuid}")))?
        };
        target
            .rename(new_name.to_string())
            .map_err(|e| ApiError::internal(format!("failed to persist the rename: {e}")))?;
        Ok(target.info())
    }

    /// All tasks across every loaded repository (global `GET /tasks`).
    pub fn all_tasks(&self) -> Vec<crate::tasks::TaskView> {
        let repos = self.repos.lock_recover();
        let mut tasks: Vec<crate::tasks::TaskView> =
            repos.values().flat_map(|r| r.tasks.list()).collect();
        tasks.sort_by(|a, b| a.started_at.cmp(&b.started_at).then(a.id.cmp(&b.id)));
        tasks
    }
}
