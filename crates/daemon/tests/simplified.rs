//! Integration tests for the simplified-query expansion endpoint
//! (`POST /query/expand`).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use metafolder_core::simplified::grammar::parse_grammar;
use metafolder_daemon::routes;
use metafolder_daemon::simplified::DEFAULT_GRAMMAR;
use metafolder_daemon::state::AppState;
use serde_json::{json, Value};
use tower::util::ServiceExt;

fn app_with_grammar(grammar_src: Option<&str>) -> Router {
    let mut state = AppState::new();
    state.set_simplified_grammar(grammar_src.map(|s| parse_grammar(s).unwrap()));
    routes::build(std::sync::Arc::new(state))
}

async fn expand(app: &Router, simplified: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri("/query/expand")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "simplified": simplified }).to_string()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn expands_with_the_default_grammar() {
    let app = app_with_grammar(Some(DEFAULT_GRAMMAR));
    let (status, body) = expand(&app, "genre:jazz rating:4").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["dsl"], "genre = \"jazz\" AND rating = 4");
}

#[tokio::test]
async fn grammar_error_is_bad_request() {
    let app = app_with_grammar(Some(DEFAULT_GRAMMAR));
    let (status, body) = expand(&app, "::: not valid").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.get("error").is_some());
}

#[tokio::test]
async fn disabled_when_no_grammar() {
    let app = app_with_grammar(None);
    let (status, body) = expand(&app, "genre:jazz").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("not configured"));
}
