//! In-memory daemon state: the set of loaded repositories.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

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
}

impl RepoState {
    /// Absolute path of `.metafolder/internal/` — the only part of the
    /// repository excluded from tracking (watcher and reconcile).
    pub fn internal_dir(&self) -> PathBuf {
        self.metafolder_dir.join(repo::INTERNAL_DIR)
    }

    pub fn from_opened(opened: OpenedRepo) -> Self {
        Self {
            conn: Mutex::new(opened.conn),
            cache: Mutex::new(TreeCache::new(opened.case_insensitive)),
            config: opened.config,
            metafolder_dir: opened.metafolder_dir,
            case_insensitive: opened.case_insensitive,
            handles: Mutex::new(None),
            schema: Mutex::new(None),
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
    #[serde(with = "metafolder_core::entry::hex_uuid")]
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
    pub fn init_repo(&self, root: &Path, metafolder: Option<&Path>) -> Result<Uuid, ApiError> {
        let opened = repo::init_repository(root, metafolder)?;
        let uuid = opened.config.repo_uuid;
        let repo_state = Self::activate(Arc::new(RepoState::from_opened(opened)))?;
        self.repos.lock().unwrap().insert(uuid, repo_state);
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
        *repo_state.schema.lock().unwrap() = schema;
        crate::executor::flush_pending(&repo_state)?;
        let quiet = std::time::Duration::from_millis(500);
        let executor = crate::executor::spawn(&repo_state, quiet);
        let watcher = crate::watcher::start(&repo_state, executor.pinger())?;
        *repo_state.handles.lock().unwrap() = Some(RepoHandles { watcher, executor });
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
            if self.repos.lock().unwrap().contains_key(&config.repo_uuid) {
                return Ok(config.repo_uuid);
            }
        }
        let opened = repo::load_repository(RepoLocator::Metafolder(metafolder_dir))?;
        let uuid = opened.config.repo_uuid;
        let repo_state = Self::activate(Arc::new(RepoState::from_opened(opened)))?;
        self.repos.lock().unwrap().insert(uuid, repo_state);
        Ok(uuid)
    }

    /// Fetches a loaded repository or fails with 404.
    pub fn repo(&self, repo_uuid: Uuid) -> Result<Arc<RepoState>, ApiError> {
        self.repos
            .lock()
            .unwrap()
            .get(&repo_uuid)
            .cloned()
            .ok_or_else(|| ApiError::not_found(format!("Repository not found: {repo_uuid}")))
    }

    pub fn list_repos(&self) -> Vec<RepoInfo> {
        let repos = self.repos.lock().unwrap();
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
}
