//! API error type. All error responses are JSON `{"error": "<message>"}`
//! with the status codes of the spec-main error table.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
    /// Schema violations, rendered as a `violations` array (spec-schema).
    pub violations: Option<Vec<serde_json::Value>>,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self { status, message: message.into(), violations: None }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }

    pub fn with_violations(mut self, violations: Vec<serde_json::Value>) -> Self {
        self.violations = Some(violations);
        self
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body = json!({"error": self.message});
        if let Some(violations) = self.violations {
            body["violations"] = serde_json::Value::Array(violations);
        }
        (self.status, Json(body)).into_response()
    }
}

/// Classifies internal errors by message. The matched fragments are owned by
/// this crate (db/log/repo modules), so the mapping stays in sync with them.
impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        let message = format!("{err:#}");
        let lower = message.to_lowercase();
        if lower.contains("not found") {
            ApiError::not_found(message)
        } else if lower.contains("already initialised") {
            ApiError::conflict(message)
        } else if lower.contains("cannot resolve path")
            || lower.contains("cycle")
            || lower.contains("depth exceeds")
            || lower.contains("occupied")
            || lower.contains("reserved")
            || lower.contains("invalid treeref")
        {
            ApiError::bad_request(message)
        } else {
            ApiError::internal(message)
        }
    }
}

impl From<axum::extract::rejection::JsonRejection> for ApiError {
    fn from(rejection: axum::extract::rejection::JsonRejection) -> Self {
        ApiError::bad_request(rejection.body_text())
    }
}
