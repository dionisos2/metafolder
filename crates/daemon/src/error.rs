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
/// never derived from substring-matching a message that may contain user data.
/// Anything not typed here is an internal error (500).
#[derive(Debug)]
pub enum DomainError {
    /// 404 — the addressed entity/operation/label does not exist.
    NotFound(String),
    /// 409 — the request conflicts with current state (e.g. already exists).
    Conflict(String),
    /// 400 — the request itself is invalid (bad path, TreeRef cycle/depth…).
    BadRequest(String),
}

impl std::fmt::Display for DomainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DomainError::NotFound(m) | DomainError::Conflict(m) | DomainError::BadRequest(m) => {
                f.write_str(m)
            }
        }
    }
}

impl std::error::Error for DomainError {}

/// Classifies internal errors into HTTP statuses purely by type: a
/// [`DomainError`] (matched through the anyhow chain) maps to its status,
/// anything else is a 500. No message-text matching.
impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        match err.downcast_ref::<DomainError>() {
            Some(domain @ DomainError::NotFound(_)) => ApiError::not_found(domain.to_string()),
            Some(domain @ DomainError::Conflict(_)) => ApiError::conflict(domain.to_string()),
            Some(domain @ DomainError::BadRequest(_)) => ApiError::bad_request(domain.to_string()),
            None => ApiError::internal(format!("{err:#}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_errors_map_by_type_regardless_of_text() {
        // The status comes from the variant, not the wording of the message.
        let cases = [
            (DomainError::NotFound("absent".into()), StatusCode::NOT_FOUND),
            (DomainError::Conflict("clash".into()), StatusCode::CONFLICT),
            (DomainError::BadRequest("nope".into()), StatusCode::BAD_REQUEST),
        ];
        for (domain, status) in cases {
            let message = domain.to_string();
            let api = ApiError::from(anyhow::Error::from(domain));
            assert_eq!(api.status, status);
            assert_eq!(api.message, message);
        }
    }

    #[test]
    fn domain_error_survives_propagation_through_anyhow() {
        fn inner() -> anyhow::Result<()> {
            Err(DomainError::BadRequest("bad treeref".into()))?;
            Ok(())
        }
        assert_eq!(ApiError::from(inner().unwrap_err()).status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn an_untyped_error_is_internal() {
        // No more substring magic: a plain message (even one that says
        // "not found") is a 500 unless it is a typed DomainError.
        assert_eq!(
            ApiError::from(anyhow::anyhow!("disk not found")).status,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}

impl From<axum::extract::rejection::JsonRejection> for ApiError {
    fn from(rejection: axum::extract::rejection::JsonRejection) -> Self {
        ApiError::bad_request(rejection.body_text())
    }
}
