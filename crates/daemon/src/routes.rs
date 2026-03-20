use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    extract::{Path as AxumPath, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use metafolder_core::entry::{Field, Value};
use metafolder_core::query::Query;

use crate::state::AppState;

// ── Error handling ────────────────────────────────────────────────────────────

pub struct AppError(anyhow::Error);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, self.0.to_string()).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(e: E) -> Self {
        AppError(e.into())
    }
}

// ── Router construction ───────────────────────────────────────────────────────

pub fn build(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/repos", get(list_repos))
        .route("/repos/init", post(init_repo))
        .route("/repos/load", post(load_repo))
        .route("/repos/:repo_uuid/entries", get(list_entries).post(create_entry))
        .route(
            "/repos/:repo_uuid/entries/:entry_uuid",
            get(get_entry).delete(delete_entry).patch(patch_entry),
        )
        .route("/repos/:repo_uuid/query", post(query_handler))
        .route("/repos/:repo_uuid/set", post(batch_set_handler))
        .route("/repos/:repo_uuid/reconcile", post(reconcile_handler))
        .with_state(state)
}

// ── Handlers ─────────────────────────────────────────────────────────────────

pub async fn health() -> &'static str {
    "ok"
}

// ── Repo management ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct RepoPathRequest {
    pub root: std::path::PathBuf,
}

#[derive(Serialize)]
pub struct RepoInfo {
    pub repo_uuid: Uuid,
    pub root: std::path::PathBuf,
    pub version: u32,
    pub created_at: u64,
}

/// `GET /repos` — list all currently loaded repositories.
pub async fn list_repos(State(state): State<Arc<AppState>>) -> Json<Vec<RepoInfo>> {
    let repos = state.repos.lock().unwrap();
    let list = repos
        .values()
        .map(|r| RepoInfo {
            repo_uuid: r.config.repo_uuid,
            root: r.config.root.clone(),
            version: r.config.version,
            created_at: r.config.created_at,
        })
        .collect();
    Json(list)
}

/// `POST /repos/init` — create a new repository.
pub async fn init_repo(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RepoPathRequest>,
) -> Result<Json<Uuid>, AppError> {
    let uuid = tokio::task::spawn_blocking(move || state.create_repo(&req.root)).await??;
    Ok(Json(uuid))
}

/// `POST /repos/load` — load an existing repository.
pub async fn load_repo(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RepoPathRequest>,
) -> Result<Json<Uuid>, AppError> {
    let uuid = tokio::task::spawn_blocking(move || state.load_repo(&req.root)).await??;
    Ok(Json(uuid))
}

// ── Entry management ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CreateEntryRequest {
    pub fields: Vec<FieldRequest>,
}

#[derive(Deserialize)]
pub struct FieldRequest {
    pub name: String,
    pub value: Value,
}

/// `GET /repos/:repo_uuid/entries` — list all entry UUIDs.
pub async fn list_entries(
    State(state): State<Arc<AppState>>,
    AxumPath(repo_uuid): AxumPath<Uuid>,
) -> Result<Json<Vec<Uuid>>, AppError> {
    let (conn, db_id) = state.get_repo_conn(repo_uuid)?;
    let uuids = tokio::task::spawn_blocking(move || {
        let conn = conn.lock().unwrap();
        crate::db::list_entries(&conn, db_id)
    })
    .await??;
    Ok(Json(uuids))
}

/// `POST /repos/:repo_uuid/entries` — create a new entry.
pub async fn create_entry(
    State(state): State<Arc<AppState>>,
    AxumPath(repo_uuid): AxumPath<Uuid>,
    Json(req): Json<CreateEntryRequest>,
) -> Result<Json<metafolder_core::entry::Metadata>, AppError> {
    let (conn, db_id) = state.get_repo_conn(repo_uuid)?;
    let fields: Vec<Field> = req
        .fields
        .into_iter()
        .map(|f| Field { name: f.name, value: f.value })
        .collect();
    let entry = tokio::task::spawn_blocking(move || {
        let conn = conn.lock().unwrap();
        crate::db::create_entry(&conn, db_id, fields)
    })
    .await??;
    Ok(Json(entry))
}

/// `GET /repos/:repo_uuid/entries/:entry_uuid` — retrieve an entry.
pub async fn get_entry(
    State(state): State<Arc<AppState>>,
    AxumPath((repo_uuid, entry_uuid)): AxumPath<(Uuid, Uuid)>,
) -> Result<Json<metafolder_core::entry::Metadata>, AppError> {
    let (conn, _) = state.get_repo_conn(repo_uuid)?;
    let entry = tokio::task::spawn_blocking(move || {
        let conn = conn.lock().unwrap();
        crate::db::get_entry(&conn, entry_uuid)
    })
    .await??;
    Ok(Json(entry))
}

/// `DELETE /repos/:repo_uuid/entries/:entry_uuid` — delete an entry.
pub async fn delete_entry(
    State(state): State<Arc<AppState>>,
    AxumPath((repo_uuid, entry_uuid)): AxumPath<(Uuid, Uuid)>,
) -> Result<StatusCode, AppError> {
    let (conn, _) = state.get_repo_conn(repo_uuid)?;
    tokio::task::spawn_blocking(move || {
        let conn = conn.lock().unwrap();
        crate::db::delete_entry(&conn, entry_uuid)
    })
    .await??;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct PatchEntryRequest {
    pub name: String,
    pub value: Value,
}

/// `PATCH /repos/:repo_uuid/entries/:entry_uuid` — set a field value.
pub async fn patch_entry(
    State(state): State<Arc<AppState>>,
    AxumPath((repo_uuid, entry_uuid)): AxumPath<(Uuid, Uuid)>,
    Json(req): Json<PatchEntryRequest>,
) -> Result<Json<metafolder_core::entry::Metadata>, AppError> {
    let (conn, _) = state.get_repo_conn(repo_uuid)?;
    let entry = tokio::task::spawn_blocking(move || {
        let conn = conn.lock().unwrap();
        crate::db::set_field(&conn, entry_uuid, &req.name, req.value)?;
        crate::db::get_entry(&conn, entry_uuid)
    })
    .await??;
    Ok(Json(entry))
}

/// `POST /repos/:repo_uuid/query` — run a query, return matching UUIDs.
pub async fn query_handler(
    State(state): State<Arc<AppState>>,
    AxumPath(repo_uuid): AxumPath<Uuid>,
    Json(query): Json<Query>,
) -> Result<Json<Vec<Uuid>>, AppError> {
    let (conn, db_id) = state.get_repo_conn(repo_uuid)?;
    let uuids = tokio::task::spawn_blocking(move || {
        let compiled = crate::query_exec::compile(&query, db_id)?;
        let conn = conn.lock().unwrap();
        crate::db::query_entries(&conn, &compiled.sql, &compiled.params)
    })
    .await??;
    Ok(Json(uuids))
}

// ── Batch set ─────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct BatchSetRequest {
    pub query: Query,
    pub name: String,
    pub value: Value,
}

#[derive(Serialize)]
pub struct BatchSetResult {
    pub updated: usize,
}

/// `POST /repos/:repo_uuid/set` — run a query, set a field on every matching entry.
pub async fn batch_set_handler(
    State(state): State<Arc<AppState>>,
    AxumPath(repo_uuid): AxumPath<Uuid>,
    Json(req): Json<BatchSetRequest>,
) -> Result<Json<BatchSetResult>, AppError> {
    let (conn, db_id) = state.get_repo_conn(repo_uuid)?;
    let updated = tokio::task::spawn_blocking(move || {
        let compiled = crate::query_exec::compile(&req.query, db_id)?;
        let conn = conn.lock().unwrap();
        let uuids = crate::db::query_entries(&conn, &compiled.sql, &compiled.params)?;
        conn.execute_batch("BEGIN")?;
        for uuid in &uuids {
            crate::db::set_field(&conn, *uuid, &req.name, req.value.clone())?;
        }
        conn.execute_batch("COMMIT")?;
        Ok::<usize, anyhow::Error>(uuids.len())
    })
    .await??;
    Ok(Json(BatchSetResult { updated }))
}

// ── Reconcile ─────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct ReconcileResult {
    pub created: usize,
    pub cleared: usize,
}

/// `POST /repos/:repo_uuid/reconcile` — sync DB with filesystem.
pub async fn reconcile_handler(
    State(state): State<Arc<AppState>>,
    AxumPath(repo_uuid): AxumPath<Uuid>,
) -> Result<Json<ReconcileResult>, AppError> {
    let (conn, db_id, root) = state.get_repo_info(repo_uuid)?;
    let result = tokio::task::spawn_blocking(move || {
        let conn = conn.lock().unwrap();
        reconcile_repo(&conn, db_id, &root)
    })
    .await??;
    Ok(Json(result))
}

pub fn reconcile_repo(
    conn: &rusqlite::Connection,
    db_id: Uuid,
    root: &Path,
) -> anyhow::Result<ReconcileResult> {
    // Load all known paths from DB in one query.
    let known: HashMap<String, Uuid> = crate::db::list_path_entries(conn)?
        .into_iter()
        .map(|(uuid, path)| (path, uuid))
        .collect();

    // Walk filesystem and collect all paths.
    let fs_paths: HashSet<String> = walk_files(root)?
        .into_iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();

    // Create entries for new files in a single transaction.
    conn.execute_batch("BEGIN")?;
    let mut created = 0usize;
    for path_str in &fs_paths {
        if !known.contains_key(path_str) {
            crate::db::create_entry(
                conn,
                db_id,
                vec![Field {
                    name: "path".to_string(),
                    value: Value::String(path_str.clone()),
                }],
            )?;
            created += 1;
        }
    }
    conn.execute_batch("COMMIT")?;

    // Clear path for entries whose files no longer exist, in a single transaction.
    conn.execute_batch("BEGIN")?;
    let mut cleared = 0usize;
    for (path_str, uuid) in &known {
        if !fs_paths.contains(path_str) {
            crate::db::clear_path(conn, *uuid)?;
            cleared += 1;
        }
    }
    conn.execute_batch("COMMIT")?;

    Ok(ReconcileResult { created, cleared })
}

/// Recursively collect all file paths under `dir`, skipping `.metafolder/`.
fn walk_files(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut result = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        if name == ".metafolder" {
            continue;
        }
        if path.is_dir() {
            result.extend(walk_files(&path)?);
        } else {
            result.push(path);
        }
    }
    Ok(result)
}

// ── Tests (reconcile_repo) ────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init_db(&conn).unwrap();
        conn
    }

    fn tmp_dir() -> PathBuf {
        let p = std::env::temp_dir().join(format!("mf_reconcile_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn test_reconcile_creates_for_new_files() {
        let dir = tmp_dir();
        let conn = test_db();
        let db_id = Uuid::new_v4();
        std::fs::write(dir.join("a.mp3"), b"").unwrap();

        let r = reconcile_repo(&conn, db_id, &dir).unwrap();
        assert_eq!(r.created, 1);
        assert_eq!(r.cleared, 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_reconcile_ignores_existing() {
        let dir = tmp_dir();
        let conn = test_db();
        let db_id = Uuid::new_v4();
        let path = dir.join("a.mp3");
        std::fs::write(&path, b"").unwrap();
        let path_str = path.to_string_lossy().to_string();
        crate::db::create_entry(
            &conn,
            db_id,
            vec![Field { name: "path".into(), value: Value::String(path_str) }],
        )
        .unwrap();

        let r = reconcile_repo(&conn, db_id, &dir).unwrap();
        assert_eq!(r.created, 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_reconcile_clears_deleted() {
        let dir = tmp_dir();
        let conn = test_db();
        let db_id = Uuid::new_v4();
        let path = dir.join("gone.mp3");
        // Entry pointing to a non-existent file
        crate::db::create_entry(
            &conn,
            db_id,
            vec![Field {
                name: "path".into(),
                value: Value::String(path.to_string_lossy().to_string()),
            }],
        )
        .unwrap();

        let r = reconcile_repo(&conn, db_id, &dir).unwrap();
        assert_eq!(r.cleared, 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_reconcile_ignores_metafolder_dir() {
        let dir = tmp_dir();
        let conn = test_db();
        let db_id = Uuid::new_v4();
        let mf = dir.join(".metafolder");
        std::fs::create_dir_all(&mf).unwrap();
        std::fs::write(mf.join("db.sqlite"), b"").unwrap();
        std::fs::write(dir.join("real.mp3"), b"").unwrap();

        let r = reconcile_repo(&conn, db_id, &dir).unwrap();
        assert_eq!(r.created, 1, "only real.mp3, not the metafolder file");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_reconcile_recursive() {
        let dir = tmp_dir();
        let conn = test_db();
        let db_id = Uuid::new_v4();
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(dir.join("a.mp3"), b"").unwrap();
        std::fs::write(sub.join("b.mp3"), b"").unwrap();

        let r = reconcile_repo(&conn, db_id, &dir).unwrap();
        assert_eq!(r.created, 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
