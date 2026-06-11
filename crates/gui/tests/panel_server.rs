//! GUI HTTP server, panel-asset side: serves panel-type directories from
//! the config dir (with the metafolder shim injected into HTML) and raw
//! local files for the `file` panel type (`/fsraw`, with Range support).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use metafolder_gui::config::ConfigDir;
use metafolder_gui::server;
use std::sync::Arc;
use tower::util::ServiceExt;

fn setup() -> (tempfile::TempDir, Arc<ConfigDir>, axum::Router) {
    let dir = tempfile::tempdir().unwrap();
    let config = Arc::new(ConfigDir::at(dir.path().join("metafolder-gui")));
    config.install_defaults().unwrap();
    let router = server::build_router(config.clone());
    (dir, config, router)
}

async fn get(router: &axum::Router, uri: &str) -> (StatusCode, String, Vec<u8>) {
    let response = router
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    let body = response.into_body().collect().await.unwrap().to_bytes().to_vec();
    (status, content_type, body)
}

#[tokio::test]
async fn test_panel_html_served_with_shim_injected() {
    let (_guard, _config, router) = setup();
    let (status, content_type, body) = get(&router, "/panel/hello/index.html").await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.starts_with("text/html"));

    let html = String::from_utf8(body).unwrap();
    // A module script so the shim can import /__keymatch.js.
    assert!(html.contains(r#"<script type="module" src="/__shim.js"></script>"#));
    assert!(html.contains("Hello from a panel type"));
}

#[tokio::test]
async fn test_panel_non_html_asset_served_verbatim() {
    let (_guard, config, router) = setup();
    let js_path = config.root().join("panel-types/hello/main.js");
    std::fs::write(&js_path, "console.log('x');").unwrap();

    let (status, content_type, body) = get(&router, "/panel/hello/main.js").await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.starts_with("text/javascript"));
    assert_eq!(body, b"console.log('x');");
}

#[tokio::test]
async fn test_unknown_panel_or_file_is_404() {
    let (_guard, _config, router) = setup();
    assert_eq!(get(&router, "/panel/nope/index.html").await.0, StatusCode::NOT_FOUND);
    assert_eq!(get(&router, "/panel/hello/missing.js").await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_path_traversal_is_rejected() {
    let (_guard, config, router) = setup();
    // A real file outside the panel dir that traversal would reach.
    assert!(config.root().join("keybindings.toml").exists());
    let (status, _, _) = get(&router, "/panel/hello/..%2F..%2Fkeybindings.toml").await;
    assert_ne!(status, StatusCode::OK);
    let (status, _, _) = get(&router, "/panel/..%2Fescape/index.html").await;
    assert_ne!(status, StatusCode::OK);
}

#[tokio::test]
async fn test_shim_and_keymatch_are_served() {
    let (_guard, _config, router) = setup();
    let (status, content_type, _) = get(&router, "/__shim.js").await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.starts_with("text/javascript"));
    let (status, _, _) = get(&router, "/__keymatch.js").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn test_fsraw_serves_local_files() {
    let (_guard, _config, router) = setup();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("notes.txt");
    std::fs::write(&file, "0123456789").unwrap();

    let uri = format!("/fsraw?path={}", file.display());
    let (status, _, body) = get(&router, &uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"0123456789");
}

#[tokio::test]
async fn test_fsraw_supports_range_requests() {
    let (_guard, _config, router) = setup();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("media.bin");
    std::fs::write(&file, "0123456789").unwrap();

    let request = Request::builder()
        .uri(format!("/fsraw?path={}", file.display()))
        .header("range", "bytes=2-5")
        .body(Body::empty())
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"2345");
}

#[tokio::test]
async fn test_fsraw_missing_file_and_missing_param() {
    let (_guard, _config, router) = setup();
    let (status, _, _) = get(&router, "/fsraw?path=/definitely/not/here").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = get(&router, "/fsraw").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
