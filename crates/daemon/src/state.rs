//! In-memory daemon state: the set of loaded repositories.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use metafolder_core::sync::MutexExt;
use rusqlite::Connection;
use serde::Serialize;
use uuid::Uuid;

use crate::config::RepoConfig;
use crate::error::ApiError;
use crate::repo::{self, OpenedRepo, RepoLocator};
use crate::tree_cache::TreeCache;

/// One loaded repository. The SQLite connection and the tree cache each sit
/// behind their own mutex; blocking work runs in `spawn_blocking`.
pub struct RepoState {
    pub conn: Mutex<Connection>,
    pub cache: Mutex<TreeCache>,
    pub config: RepoConfig,
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

    pub fn from_opened(opened: OpenedRepo) -> Self {
        let repo_uuid = opened.config.repo_uuid;
        Self {
            conn: Mutex::new(opened.conn),
            cache: Mutex::new(TreeCache::new(opened.case_insensitive)),
            config: opened.config,
            metafolder_dir: opened.metafolder_dir,
            case_insensitive: opened.case_insensitive,
            handles: Mutex::new(None),
            schema: Mutex::new(None),
            rollback_lock: Mutex::new(None),
            tasks: crate::tasks::TaskRegistry::new(repo_uuid),
            index: Mutex::new(None),
        }
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

    /// Initialises a new repository and registers it as loaded.
    pub fn init_repo(
        &self,
        root: &Path,
        metafolder: Option<&Path>,
        name: Option<&str>,
    ) -> Result<Uuid, ApiError> {
        let opened = repo::init_repository(root, metafolder, name)?;
        let uuid = opened.config.repo_uuid;
        let repo_state = Self::activate(Arc::new(RepoState::from_opened(opened)))?;
        self.repos.lock_recover().insert(uuid, repo_state);
        Ok(uuid)
    }

    /// Loads the user schema (an invalid schema file fails the load with
    /// 400), replays any pending buffer left by a previous run, then starts
    /// the watcher and its executor (spec: the buffer is replayed before the
    /// repository serves requests).
    fn activate(repo_state: Arc<RepoState>) -> Result<Arc<RepoState>, ApiError> {
        let schema =
            crate::schema::load_for_repo(&repo_state.metafolder_dir, &repo_state.config)
                .map_err(ApiError::bad_request)?;
        *repo_state.schema.lock_recover() = schema;
        crate::executor::flush_pending(&repo_state)?;
        let quiet = std::time::Duration::from_millis(500);
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
        let repo_state = Self::activate(Arc::new(RepoState::from_opened(opened)))?;
        self.repos.lock_recover().insert(uuid, repo_state);
        Ok(uuid)
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
        let mut infos: Vec<RepoInfo> = repos
            .values()
            .map(|r| RepoInfo {
                repo_uuid: r.config.repo_uuid,
                name: r.config.name.clone(),
                root: r.config.root.clone(),
                internal_dir: r.internal_dir(),
                created_at: r.config.created_at,
            })
            .collect();
        infos.sort_by_key(|i| i.repo_uuid);
        infos
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
