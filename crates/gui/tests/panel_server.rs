//! GUI HTTP server, panel-asset side: serves panel-type directories from
//! the config dir (verbatim — the shell mounts them into Shadow DOM) and raw
//! local files for the `file` panel type (`/fsraw`, with Range support).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use metafolder_gui::config::ConfigDir;
use metafolder_gui::daemon_proxy::DaemonProxy;
use metafolder_gui::notifier::RecordingNotifier;
use metafolder_gui::server::{self, ServerState};
use metafolder_gui::state::GuiState;
use std::sync::Arc;
use tower::util::ServiceExt;

mod common;

fn setup() -> (tempfile::TempDir, Arc<ConfigDir>, axum::Router) {
    let dir = tempfile::tempdir().unwrap();
    let config = Arc::new(ConfigDir::at(dir.path().join("gui")));
    common::install_defaults(&config);
    let state = ServerState {
        config: config.clone(),
        gui: Arc::new(GuiState::new(Arc::new(RecordingNotifier::new()))),
        daemon: Arc::new(DaemonProxy::new("http://127.0.0.1:1".into())),
        keybindings: Arc::new(std::sync::Mutex::new(config.load_keybindings().unwrap())),
        input: Arc::new(server::input_wait::InputWait::new()),
        commands: Arc::new(server::command_wait::CommandWait::new()),
        bench: Arc::new(server::bench::BenchBuffer::new()),
        repo_list_cache_ttl: std::time::Duration::from_secs(3),
    };
    let router = server::build_router(state);
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
async fn test_panel_html_served_verbatim() {
    let (_guard, _config, router) = setup();
    let (status, content_type, body) = get(&router, "/panel/hello/index.html").await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.starts_with("text/html"));

    let html = String::from_utf8(body).unwrap();
    // The shell mounts panels into Shadow DOM and injects the API/stylesheet
    // itself: the served HTML is verbatim, no shim/style tags injected.
    assert!(!html.contains("/__shim.js"));
    assert!(!html.contains(r#"<link rel="stylesheet" href="/__style.css">"#));
    assert!(html.contains("Hello from a panel type"));
}

#[tokio::test]
async fn test_user_stylesheet_is_served_to_panels() {
    let (_guard, config, router) = setup();
    std::fs::write(config.style_css_path(), "body { color: teal }").unwrap();

    let (status, content_type, body) = get(&router, "/__style.css").await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.starts_with("text/css"));
    assert_eq!(String::from_utf8(body).unwrap(), "body { color: teal }");
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
async fn test_thumbnail_for_file_in_no_repo_is_404() {
    // The stub daemon is unreachable, so `GET /repos` yields no repository for
    // the path: the file is "in no repo", the thumbnail is refused (404) and
    // the panel falls back to a glyph. No ffmpeg runs, nothing is written.
    let (guard, _config, router) = setup();
    let video = guard.path().join("clip.mp4");
    std::fs::write(&video, b"x").unwrap();
    let uri = format!("/thumbnail?path={}", video.display());
    assert_eq!(get(&router, &uri).await.0, StatusCode::NOT_FOUND);
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
async fn test_panel_helper_modules_are_served() {
    let (_guard, _config, router) = setup();
    // Helpers panels import; the shim's own modules are no longer served.
    let (status, content_type, _) = get(&router, "/__ui.js").await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.starts_with("text/javascript"));
    let (status, _, _) = get(&router, "/__menu.js").await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = get(&router, "/__orphan.js").await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = get(&router, "/__paged-list.js").await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = get(&router, "/__value-widget.js").await;
    assert_eq!(status, StatusCode::OK);
    // Removed with the iframe shim.
    assert_eq!(get(&router, "/__shim.js").await.0, StatusCode::NOT_FOUND);
    assert_eq!(get(&router, "/__keymatch.js").await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_panel_helper_modules_are_not_cached() {
    // Panel `main.js` is cache-busted (?v=…) but its static `import '/__ui.js'`
    // is not, so a stale shim in the WebView's HTTP cache would silently mask a
    // rebuilt helper (e.g. a panel importing a newly added export). The shim
    // routes must therefore forbid caching.
    let (_guard, _config, router) = setup();
    for uri in ["/__ui.js", "/__menu.js", "/__orphan.js", "/__paged-list.js", "/__value-widget.js"] {
        let response = router
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let cache_control = response
            .headers()
            .get("cache-control")
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default();
        assert!(
            cache_control.contains("no-cache"),
            "{uri} must send a no-cache header, got {cache_control:?}"
        );
    }
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
