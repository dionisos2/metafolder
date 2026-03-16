use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use uuid::Uuid;

use metafolder_core::entry::{Field, Value};

use crate::state::AppState;

// ── Error handling ────────────────────────────────────────────────────────────

/// Wrapper to convert any error into an HTTP 500 response.
pub struct AppError(anyhow::Error);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, self.0.to_string()).into_response()
    }
}

// Allows using `?` in handlers to propagate errors.
impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(e: E) -> Self {
        AppError(e.into())
    }
}

// ── Router construction ───────────────────────────────────────────────────────

pub fn build(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/entries", post(create_entry))
        .route("/entries/:uuid", get(get_entry))
        .with_state(state)
}

// ── Handlers ─────────────────────────────────────────────────────────────────

pub async fn health() -> &'static str {
    "ok"
}

/// Request body for creating an entry.
#[derive(Deserialize)]
pub struct CreateEntryRequest {
    /// The database identifier to attach this entry to.
    pub db_id: Uuid,
    /// The initial fields of the entry (may be empty).
    pub fields: Vec<FieldRequest>,
}

#[derive(Deserialize)]
pub struct FieldRequest {
    pub name: String,
    pub value: Value,
}

/// `POST /entries` — creates a new entry with its fields.
pub async fn create_entry(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateEntryRequest>,
) -> Result<Json<metafolder_core::entry::Metadata>, AppError> {
    let fields: Vec<Field> = req
        .fields
        .into_iter()
        .map(|f| Field { name: f.name, value: f.value })
        .collect();

    let conn = state.conn.clone();

    let entry = tokio::task::spawn_blocking(move || {
        let conn = conn.lock().unwrap();
        crate::db::create_entry(&conn, req.db_id, fields)
    })
    .await??;

    Ok(Json(entry))
}

/// `GET /entries/:uuid` — retrieves an entry and all its fields.
pub async fn get_entry(
    State(state): State<Arc<AppState>>,
    Path(uuid): Path<Uuid>,
) -> Result<Json<metafolder_core::entry::Metadata>, AppError> {
    let conn = state.conn.clone();

    let entry = tokio::task::spawn_blocking(move || {
        let conn = conn.lock().unwrap();
        crate::db::get_entry(&conn, uuid)
    })
    .await??;

    Ok(Json(entry))
}
