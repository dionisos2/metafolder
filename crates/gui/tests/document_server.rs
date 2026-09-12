//! `GET /document` and `GET /document/info`: sandboxed page rendering for the
//! `file` panel's document preview (spec-gui "Documents").
//!
//! A PDF is never handed to the WebView — `/fsraw` bytes loaded as a document
//! would run in the GUI server's origin (see `tests/panel_invariants.rs`). The
//! panel shows a PNG rendered out of process instead, and these tests drive the
//! whole route: repository resolution through a stub daemon, poppler under the
//! sandbox, and the status each failure maps to.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::Json;
use http_body_util::BodyExt;
use metafolder_gui::config::ConfigDir;
use metafolder_gui::daemon_proxy::DaemonProxy;
use metafolder_gui::notifier::RecordingNotifier;
use metafolder_gui::server::{self, ServerState};
use metafolder_gui::state::GuiState;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use tower::util::ServiceExt;

mod common;

/// A syntactically complete one-page PDF (the xref offsets are exact, so
/// poppler parses it without reconstructing the table). Multi-page rendering is
/// covered by the unit tests in `src/documents.rs`; here one page is enough to
/// drive the route.
const ONE_PAGE_PDF: &[u8] = b"%PDF-1.4\n\
1 0 obj\n<</Type/Catalog/Pages 2 0 R>>\nendobj\n\
2 0 obj\n<</Type/Pages/Kids[4 0 R]/Count 1>>\nendobj\n\
3 0 obj\n<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>\nendobj\n\
4 0 obj\n<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 200]/Resources<</Font<</F1 3 0 R>>>>/Contents 5 0 R>>\nendobj\n\
5 0 obj\n<</Length 37>>stream\nBT /F1 24 Tf 20 100 Td (page 1) Tj ET\nendstream\nendobj\n\
xref\n0 6\n\
0000000000 65535 f \n\
0000000009 00000 n \n\
0000000054 00000 n \n\
0000000105 00000 n \n\
0000000168 00000 n \n\
0000000280 00000 n \n\
trailer\n<</Size 6/Root 1 0 R>>\nstartxref\n364\n%%EOF\n";

fn poppler_present() -> bool {
    std::process::Command::new("pdfinfo")
        .arg("-v")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

/// A daemon stub whose `GET /repos` reports one repository rooted at `root`,
/// so a file under it resolves to a cache directory.
async fn spawn_repo_stub(root: &Path) -> String {
    let internal = root.join(".metafolder").join("internal");
    let body = json!([{
        "uuid": "0".repeat(32),
        "name": "test",
        "root": root.to_string_lossy(),
        "internal_dir": internal.to_string_lossy(),
    }]);
    let router = axum::Router::new().route(
        "/repos",
        get(move || {
            let body = body.clone();
            async move { Json(body) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://127.0.0.1:{port}")
}

fn router_for(daemon_url: String, config_dir: &Path) -> axum::Router {
    let config = Arc::new(ConfigDir::at(config_dir.join("gui")));
    common::install_defaults(&config);
    let state = ServerState {
        config: config.clone(),
        gui: Arc::new(GuiState::new(Arc::new(RecordingNotifier::new()))),
        daemon: Arc::new(DaemonProxy::new(daemon_url)),
        keybindings: Arc::new(std::sync::Mutex::new(config.load_keybindings().unwrap())),
        input: Arc::new(server::input_wait::InputWait::new()),
        commands: Arc::new(server::command_wait::CommandWait::new()),
        bench: Arc::new(server::bench::BenchBuffer::new()),
        // No repository-list caching: the cache is process-wide, so with a
        // TTL these tests would read each other's stub daemons.
        repo_list_cache_ttl: std::time::Duration::ZERO,
    };
    server::build_router(state)
}

async fn get_uri(router: &axum::Router, uri: &str) -> (StatusCode, String, Vec<u8>) {
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

/// The whole route, end to end: a real PDF in a real repository is counted and
/// rendered, and what comes back is a PNG — never the PDF's own bytes.
#[tokio::test]
async fn test_document_info_and_page_render_a_real_pdf() {
    if !poppler_present() {
        eprintln!("skipping: poppler not available");
        return;
    }
    let guard = common::TempDir::new("document-server");
    let pdf = guard.path().join("report.pdf");
    std::fs::write(&pdf, ONE_PAGE_PDF).unwrap();
    let router = router_for(spawn_repo_stub(guard.path()).await, guard.path());

    let uri = format!("/document/info?path={}", pdf.display());
    let (status, content_type, body) = get_uri(&router, &uri).await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.starts_with("application/json"), "got {content_type}");
    let info: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(info["pages"], 1);

    let uri = format!("/document?path={}&page=1&dpi=100", pdf.display());
    let (status, content_type, body) = get_uri(&router, &uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type, "image/png");
    assert_eq!(&body[..8], b"\x89PNG\r\n\x1a\n", "the panel is served an image, never the PDF");

    // The render is cached inside the repository, like a video poster.
    let cache = guard.path().join(".metafolder").join("internal").join("documents");
    let cached: Vec<_> = std::fs::read_dir(&cache).unwrap().filter_map(|e| e.ok()).collect();
    assert_eq!(cached.len(), 1, "exactly one page PNG cached");
}

/// A page past the end renders nothing; the panel bounds navigation by the
/// page count, so this is a failure, not an empty image.
#[tokio::test]
async fn test_page_past_the_end_fails() {
    if !poppler_present() {
        return;
    }
    let guard = common::TempDir::new("document-past-end");
    let pdf = guard.path().join("report.pdf");
    std::fs::write(&pdf, ONE_PAGE_PDF).unwrap();
    let router = router_for(spawn_repo_stub(guard.path()).await, guard.path());
    let uri = format!("/document?path={}&page=9", pdf.display());
    assert_eq!(get_uri(&router, &uri).await.0, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn test_non_document_types_are_refused() {
    let guard = common::TempDir::new("document-unsupported");
    let text = guard.path().join("note.txt");
    std::fs::write(&text, b"hello").unwrap();
    let router = router_for(spawn_repo_stub(guard.path()).await, guard.path());

    let uri = format!("/document?path={}", text.display());
    assert_eq!(get_uri(&router, &uri).await.0, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let uri = format!("/document/info?path={}", text.display());
    assert_eq!(get_uri(&router, &uri).await.0, StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn test_missing_file_is_not_found() {
    let guard = common::TempDir::new("document-missing");
    let router = router_for(spawn_repo_stub(guard.path()).await, guard.path());
    let uri = format!("/document/info?path={}/nope.pdf", guard.path().display());
    assert_eq!(get_uri(&router, &uri).await.0, StatusCode::NOT_FOUND);
}

/// Rendering writes a cache, so it needs a repository to write it into — like
/// `/thumbnail`. Counting pages writes nothing, so it answers for any file.
#[tokio::test]
async fn test_page_outside_any_repository_is_not_found_but_info_still_answers() {
    if !poppler_present() {
        return;
    }
    let guard = common::TempDir::new("document-no-repo");
    let pdf = guard.path().join("report.pdf");
    std::fs::write(&pdf, ONE_PAGE_PDF).unwrap();
    // A daemon that reports no repository at all.
    let router = router_for("http://127.0.0.1:1".to_string(), guard.path());

    let uri = format!("/document?path={}", pdf.display());
    assert_eq!(get_uri(&router, &uri).await.0, StatusCode::NOT_FOUND);
    let uri = format!("/document/info?path={}", pdf.display());
    let (status, _, body) = get_uri(&router, &uri).await;
    assert_eq!(status, StatusCode::OK);
    let info: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(info["pages"], 1);
}

#[tokio::test]
async fn test_missing_path_parameter_is_a_bad_request() {
    let guard = common::TempDir::new("document-no-param");
    let router = router_for(spawn_repo_stub(guard.path()).await, guard.path());
    assert_eq!(get_uri(&router, "/document").await.0, StatusCode::BAD_REQUEST);
    assert_eq!(get_uri(&router, "/document/info").await.0, StatusCode::BAD_REQUEST);
}
