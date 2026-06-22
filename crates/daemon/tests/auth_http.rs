//! Integration tests for the session-token authentication layer (spec-auth):
//! `build_authenticated` rejects requests without a valid bearer token.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use metafolder_daemon::routes;
use metafolder_daemon::state::AppState;
use tower::util::ServiceExt;

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn app() -> Router {
    routes::build_authenticated(Arc::new(AppState::new()), TOKEN.into())
}

async fn status_with_auth(authorization: Option<&str>) -> StatusCode {
    let mut builder = Request::builder().method("GET").uri("/health");
    if let Some(value) = authorization {
        builder = builder.header("authorization", value);
    }
    let request = builder.body(Body::empty()).unwrap();
    app().oneshot(request).await.unwrap().status()
}

#[tokio::test]
async fn rejects_request_without_token() {
    assert_eq!(status_with_auth(None).await, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn rejects_wrong_token() {
    let wrong = format!("Bearer {}", "f".repeat(64));
    assert_eq!(status_with_auth(Some(&wrong)).await, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn rejects_non_bearer_scheme() {
    let basic = format!("Basic {TOKEN}");
    assert_eq!(status_with_auth(Some(&basic)).await, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn accepts_valid_token() {
    let header = format!("Bearer {TOKEN}");
    assert_eq!(status_with_auth(Some(&header)).await, StatusCode::OK);
}
