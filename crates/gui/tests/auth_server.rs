//! GUI HTTP server authentication layer (doc "Session tokens"): `build_router_authenticated`
//! gates the sensitive routes (`/fsraw`, `/thumbnail`, `/document*`, `/__media-probe`,
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
const PORT: u16 = 7524;

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
        repo_list_cache_ttl: std::time::Duration::from_secs(3),
        video_thumbnail_percent: 10.0,
    };
    let router = server::build_router_authenticated(state, TOKEN.into(), PORT);
    (dir, router)
}

async fn status(router: &axum::Router, uri: &str, authorization: Option<&str>) -> StatusCode {
    let mut builder = Request::builder().method("GET").uri(uri).header("host", "127.0.0.1:7524");
    if let Some(value) = authorization {
        builder = builder.header("authorization", value);
    }
    router.clone().oneshot(builder.body(Body::empty()).unwrap()).await.unwrap().status()
}

#[tokio::test]
async fn open_panel_asset_needs_no_token() {
    let (_guard, router) = setup();
    assert_eq!(status(&router, "/panel/message/index.html", None).await, StatusCode::OK);
}

#[tokio::test]
async fn docs_need_no_token() {
    // Shipped, public content fetched like the panel assets.
    let (guard, router) = setup();
    let docs = guard.path().join("docs");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(docs.join("index.json"), "[]").unwrap();
    assert_eq!(status(&router, "/docs/index.json", None).await, StatusCode::OK);
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

#[tokio::test]
async fn document_routes_require_a_token() {
    // Rendering a page reads a local file, so both document routes are gated
    // like `/fsraw` — and both accept the token in the query string, since the
    // page is fetched by an `<img>` that cannot set a header.
    let (_guard, router) = setup();
    for uri in ["/document?path=/tmp/a.pdf", "/document/info?path=/tmp/a.pdf"] {
        assert_eq!(status(&router, uri, None).await, StatusCode::UNAUTHORIZED);
        let wrong = format!("{uri}&token={}", "f".repeat(64));
        assert_eq!(status(&router, &wrong, None).await, StatusCode::UNAUTHORIZED);
        // With the right token the auth layer lets it through; the handler then
        // answers on its own merits (no such file).
        let allowed = format!("{uri}&token={TOKEN}");
        assert_ne!(status(&router, &allowed, None).await, StatusCode::UNAUTHORIZED);
    }
}

/// A request as a web page would send it: `Host` and `Origin` set.
async fn from_page(
    router: &axum::Router,
    uri: &str,
    host: Option<&str>,
    origin: Option<&str>,
) -> axum::response::Response {
    let mut builder = Request::builder().method("GET").uri(uri);
    if let Some(host) = host {
        builder = builder.header("host", host);
    }
    if let Some(origin) = origin {
        builder = builder.header("origin", origin);
    }
    router.clone().oneshot(builder.body(Body::empty()).unwrap()).await.unwrap()
}

const USER_COMMANDS: &str = "/__commands/1/user-commands.js";

#[tokio::test]
async fn a_foreign_page_cannot_read_the_open_routes() {
    // The user's own commands and macros are open (an `import()` cannot send
    // a token) — but only the shell's origin may read them cross-origin.
    let (_guard, router) = setup();
    let response =
        from_page(&router, USER_COMMANDS, Some("127.0.0.1:7524"), Some("https://evil.example"))
            .await;
    assert!(response.headers().get("access-control-allow-origin").is_none());
}

#[tokio::test]
async fn the_shell_origin_reads_the_open_routes() {
    let (_guard, router) = setup();
    for origin in ["tauri://localhost", "http://tauri.localhost"] {
        let response =
            from_page(&router, USER_COMMANDS, Some("127.0.0.1:7524"), Some(origin)).await;
        assert_eq!(response.status(), StatusCode::OK, "{origin}");
        assert_eq!(
            response.headers().get("access-control-allow-origin").unwrap(),
            origin,
            "{origin}"
        );
    }
}

#[tokio::test]
async fn a_request_for_another_host_is_refused() {
    // DNS rebinding: a page of evil.example whose name now resolves to
    // 127.0.0.1 is same-origin with itself, so CORS does not stop it — the
    // `Host` it sends does.
    let (_guard, router) = setup();
    for host in [Some("evil.example:7524"), Some("127.0.0.1:9999"), None] {
        let response = from_page(&router, USER_COMMANDS, host, None).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{host:?}");
    }
    for host in ["127.0.0.1:7524", "localhost:7524"] {
        let response = from_page(&router, USER_COMMANDS, Some(host), None).await;
        assert_eq!(response.status(), StatusCode::OK, "{host}");
    }
}
