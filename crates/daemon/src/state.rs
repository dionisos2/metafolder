use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use rusqlite::Connection;
use uuid::Uuid;

use crate::config::RepoConfig;
use crate::watcher::WatcherHandle;

pub struct RepoState {
    pub conn: Arc<Mutex<Connection>>,
    pub config: RepoConfig,
    _watcher: WatcherHandle,
}

pub struct AppState {
    pub repos: Mutex<HashMap<Uuid, RepoState>>,
}

impl AppState {
    pub fn new() -> Self {
        Self { repos: Mutex::new(HashMap::new()) }
    }

    /// Creates a new repository at `root`: writes config.json, initializes db.sqlite.
    /// Fails if a repository already exists there.
    pub fn create_repo(&self, root: &Path) -> anyhow::Result<Uuid> {
        let root = root.canonicalize()
            .with_context(|| format!("Cannot resolve path {:?}: the directory must exist before initializing a repository", root))?;
        let metafolder_dir = root.join(".metafolder");

        if metafolder_dir.exists() {
            anyhow::bail!("Repository already exists at {:?}", root);
        }

        std::fs::create_dir_all(&metafolder_dir)
            .context("Failed to create .metafolder directory")?;

        let config = RepoConfig::new(root.clone());
        config.write(&metafolder_dir)?;

        let db_path = metafolder_dir.join("db.sqlite");
        let conn = Connection::open(&db_path)
            .context("Failed to open SQLite database")?;
        crate::db::init_db(&conn)?;

        let conn = Arc::new(Mutex::new(conn));
        let watcher = crate::watcher::start(root, conn.clone(), config.repo_uuid);
        let repo_uuid = config.repo_uuid;

        self.repos.lock().unwrap().insert(repo_uuid, RepoState {
            conn,
            config,
            _watcher: watcher,
        });

        println!("[daemon] Repository created (uuid: {repo_uuid})");
        Ok(repo_uuid)
    }

    /// Loads an existing repository from `root`.
    /// Fails if no repository is found there.
    pub fn load_repo(&self, root: &Path) -> anyhow::Result<Uuid> {
        let root = root.canonicalize()
            .with_context(|| format!("Cannot resolve path {:?}: the directory must exist before loading a repository", root))?;
        let metafolder_dir = root.join(".metafolder");

        if !metafolder_dir.exists() {
            anyhow::bail!("No repository found at {:?} (missing .metafolder/)", root);
        }

        let config = RepoConfig::read(&metafolder_dir)?;
        let db_path = metafolder_dir.join("db.sqlite");
        let conn = Connection::open(&db_path)
            .context("Failed to open SQLite database")?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")
            .context("Failed to configure SQLite")?;

        let conn = Arc::new(Mutex::new(conn));
        let watcher = crate::watcher::start(root, conn.clone(), config.repo_uuid);
        let repo_uuid = config.repo_uuid;

        self.repos.lock().unwrap().insert(repo_uuid, RepoState {
            conn,
            config,
            _watcher: watcher,
        });

        println!("[daemon] Repository loaded (uuid: {repo_uuid})");
        Ok(repo_uuid)
    }

    /// Returns the connection and uuid for a loaded repo, or an error if not loaded.
    pub fn get_repo_conn(&self, repo_uuid: Uuid) -> anyhow::Result<(Arc<Mutex<Connection>>, Uuid)> {
        let repos = self.repos.lock().unwrap();
        let repo = repos.get(&repo_uuid)
            .ok_or_else(|| anyhow::anyhow!("Repository {repo_uuid} is not loaded"))?;
        Ok((repo.conn.clone(), repo.config.repo_uuid))
    }

    /// Returns (conn, db_id, root) for a loaded repo.
    pub fn get_repo_info(
        &self,
        repo_uuid: Uuid,
    ) -> anyhow::Result<(Arc<Mutex<Connection>>, Uuid, std::path::PathBuf)> {
        let repos = self.repos.lock().unwrap();
        let repo = repos
            .get(&repo_uuid)
            .ok_or_else(|| anyhow::anyhow!("Repository {repo_uuid} is not loaded"))?;
        Ok((repo.conn.clone(), repo.config.repo_uuid, repo.config.root.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("metafolder_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_create_repo_creates_metafolder_structure() {
        let dir = make_temp_dir();
        let state = AppState::new();

        let uuid = state.create_repo(&dir).unwrap();

        assert!(dir.join(".metafolder/config.json").exists());
        assert!(dir.join(".metafolder/db.sqlite").exists());
        let repos = state.repos.lock().unwrap();
        assert!(repos.contains_key(&uuid));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_create_repo_fails_if_already_exists() {
        let dir = make_temp_dir();
        let state = AppState::new();
        state.create_repo(&dir).unwrap();

        let err = state.create_repo(&dir).unwrap_err();
        assert!(err.to_string().contains("already exists"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_create_repo_fails_if_dir_missing() {
        let dir = std::env::temp_dir()
            .join(format!("metafolder_nonexistent_{}", uuid::Uuid::new_v4()));
        let state = AppState::new();

        let err = state.create_repo(&dir).unwrap_err();
        assert!(err.to_string().contains("must exist"));
    }

    #[test]
    fn test_load_repo_restores_uuid() {
        let dir = make_temp_dir();
        let state1 = AppState::new();
        let created_uuid = state1.create_repo(&dir).unwrap();

        let state2 = AppState::new();
        let loaded_uuid = state2.load_repo(&dir).unwrap();

        assert_eq!(created_uuid, loaded_uuid);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_load_repo_fails_if_no_metafolder() {
        let dir = make_temp_dir();
        let state = AppState::new();

        let err = state.load_repo(&dir).unwrap_err();
        assert!(err.to_string().contains("No repository found"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_load_repo_fails_if_dir_missing() {
        let dir = std::env::temp_dir()
            .join(format!("metafolder_nonexistent_{}", uuid::Uuid::new_v4()));
        let state = AppState::new();

        let err = state.load_repo(&dir).unwrap_err();
        assert!(err.to_string().contains("must exist"));
    }
}
