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

    /// `401 Unauthorized`: missing or invalid session token (spec-auth).
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }

    /// `423 Locked`: a metadata write was attempted while the repository is in
    /// coordinated-rollback lock mode (spec-event-log "Rollback lock").
    pub fn locked(message: impl Into<String>) -> Self {
        Self::new(StatusCode::LOCKED, message)
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

/// A domain error carrying its intended HTTP classification by *type* rather
/// than by message text. Producers return these (they convert into
/// `anyhow::Error` through `?`), so the status survives a message rename and is
/// not derived from substring-matching a message that may contain user data.
#[derive(Debug)]
pub enum DomainError {
    /// 404 — the addressed entity/operation/label does not exist.
    NotFound(String),
}

impl std::fmt::Display for DomainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DomainError::NotFound(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for DomainError {}

/// Classifies internal errors into HTTP statuses. Typed [`DomainError`]s are
/// matched by type; the remaining producers (still using bare `anyhow`
/// messages) are matched by message fragment — those fragments are distinctive
/// phrases owned by this crate (db/log/repo), so the mapping stays in sync.
impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        if let Some(domain) = err.downcast_ref::<DomainError>() {
            return match domain {
                DomainError::NotFound(_) => ApiError::not_found(domain.to_string()),
            };
        }
        let message = format!("{err:#}");
        let lower = message.to_lowercase();
        if lower.contains("already initialised") {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_not_found_maps_to_404_regardless_of_text() {
        // By type, not by the words in the message (no "not found" needed).
        let err: anyhow::Error = DomainError::NotFound("entity 7 is absent".into()).into();
        let api = ApiError::from(err);
        assert_eq!(api.status, StatusCode::NOT_FOUND);
        assert_eq!(api.message, "entity 7 is absent");
    }

    #[test]
    fn typed_not_found_survives_propagation_through_anyhow() {
        // A function returning anyhow::Result that propagates a DomainError via `?`.
        fn inner() -> anyhow::Result<()> {
            Err(DomainError::NotFound("gone".into()))?;
            Ok(())
        }
        assert_eq!(ApiError::from(inner().unwrap_err()).status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn distinctive_fragments_still_classify_by_message() {
        assert_eq!(
            ApiError::from(anyhow::anyhow!("already initialised at /x")).status,
            StatusCode::CONFLICT
        );
        assert_eq!(
            ApiError::from(anyhow::anyhow!("TreeRef write would create a cycle")).status,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            ApiError::from(anyhow::anyhow!("something unexpected")).status,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}

impl From<axum::extract::rejection::JsonRejection> for ApiError {
    fn from(rejection: axum::extract::rejection::JsonRejection) -> Self {
        ApiError::bad_request(rejection.body_text())
    }
}
