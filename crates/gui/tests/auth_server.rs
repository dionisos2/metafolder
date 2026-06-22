//! GUI HTTP server authentication layer (spec-auth): `build_router_authenticated`
//! gates the sensitive routes (`/fsraw`, `/thumbnail`, `/__media-probe`,
//! `/gui/*`) while leaving panel assets open. Header on every protected route;
//! `?token=` query parameter additionally accepted on the media/raw routes.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use metafolder_gui::config::ConfigDir;
use metafolder_gui::daemon_proxy::DaemonProxy;
use metafolder_gui::notifier::RecordingNotifier;
use metafolder_gui::server::{self, ServerState};
use metafolder_gui::state::GuiState;
use std::sync::Arc;
use tower::util::ServiceExt;

mod common;

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn setup() -> (tempfile::TempDir, axum::Router) {
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
    };
    let router = server::build_router_authenticated(state, TOKEN.into());
    (dir, router)
}

async fn status(router: &axum::Router, uri: &str, authorization: Option<&str>) -> StatusCode {
    let mut builder = Request::builder().method("GET").uri(uri);
    if let Some(value) = authorization {
        builder = builder.header("authorization", value);
    }
    router.clone().oneshot(builder.body(Body::empty()).unwrap()).await.unwrap().status()
}

#[tokio::test]
async fn open_panel_asset_needs_no_token() {
    let (_guard, router) = setup();
    assert_eq!(status(&router, "/panel/hello/index.html", None).await, StatusCode::OK);
}

#[tokio::test]
async fn gui_api_requires_token() {
    let (_guard, router) = setup();
    assert_eq!(status(&router, "/gui/status", None).await, StatusCode::UNAUTHORIZED);
    let header = format!("Bearer {TOKEN}");
    assert_eq!(status(&router, "/gui/status", Some(&header)).await, StatusCode::OK);
}

#[tokio::test]
async fn fsraw_rejects_missing_and_wrong_token() {
    let (_guard, router) = setup();
    assert_eq!(status(&router, "/fsraw?path=/etc/hostname", None).await, StatusCode::UNAUTHORIZED);
    let wrong = format!("/fsraw?path=/etc/hostname&token={}", "f".repeat(64));
    assert_eq!(status(&router, &wrong, None).await, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn fsraw_accepts_query_token() {
    let (guard, router) = setup();
    let file = guard.path().join("hello.txt");
    std::fs::write(&file, "hi").unwrap();
    let uri = format!("/fsraw?path={}&token={TOKEN}", file.display());
    // Passes auth (the file exists, so the raw-file handler returns 200).
    assert_eq!(status(&router, &uri, None).await, StatusCode::OK);
}

#[tokio::test]
async fn fsraw_accepts_header_token() {
    let (guard, router) = setup();
    let file = guard.path().join("hello.txt");
    std::fs::write(&file, "hi").unwrap();
    let uri = format!("/fsraw?path={}", file.display());
    let header = format!("Bearer {TOKEN}");
    assert_eq!(status(&router, &uri, Some(&header)).await, StatusCode::OK);
}
