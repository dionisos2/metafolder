//! The GUI HTTP server (default 127.0.0.1:7524). Serves panel-type
//! directories from the config dir (shim injected into HTML), raw local
//! files for the `file` panel type, and the `/gui/*` scripting API.
//! Built as a plain `axum::Router` so tests can drive it with
//! `tower::ServiceExt::oneshot`.

pub mod bench;
pub mod command_wait;
mod fsraw;
mod gui_api;
pub mod input_wait;
mod panel_assets;
mod thumbnail;

use crate::config::ConfigDir;
use crate::daemon_proxy::DaemonProxy;
use crate::keybindings::KeybindingSet;
use crate::state::GuiState;
use axum::routing::{delete, get, post, put};
use axum::Router;
use bench::BenchBuffer;
use command_wait::CommandWait;
use input_wait::InputWait;
use std::sync::{Arc, Mutex};

// Helper modules imported by panels (the shell bundles its own copies of
// keymatch/menu/resolve/visibility from panel-shim/).
const UI_JS: &str = include_str!("../../panel-shim/ui.js");
const MENU_JS: &str = include_str!("../../panel-shim/menu.js");
const ORPHAN_JS: &str = include_str!("../../panel-shim/orphan.js");
const PAGED_LIST_JS: &str = include_str!("../../panel-shim/paged-list.js");
const VALUE_WIDGET_JS: &str = include_str!("../../panel-shim/value-widget.js");
const HELP_JS: &str = include_str!("../../panel-shim/help.js");

#[derive(Clone)]
pub struct ServerState {
    pub config: Arc<ConfigDir>,
    pub gui: Arc<GuiState>,
    pub daemon: Arc<DaemonProxy>,
    pub keybindings: Arc<Mutex<KeybindingSet>>,
    pub input: Arc<InputWait>,
    pub commands: Arc<CommandWait>,
    pub bench: Arc<BenchBuffer>,
}

pub fn build_router(state: ServerState) -> Router {
    Router::new()
        .route("/__ui.js", get(|| async { javascript(UI_JS) }))
        .route("/__menu.js", get(|| async { javascript(MENU_JS) }))
        .route("/__orphan.js", get(|| async { javascript(ORPHAN_JS) }))
        .route("/__paged-list.js", get(|| async { javascript(PAGED_LIST_JS) }))
        .route("/__value-widget.js", get(|| async { javascript(VALUE_WIDGET_JS) }))
        .route("/__help.js", get(|| async { javascript(HELP_JS) }))
        .route(
            "/__style.css",
            get(|axum::extract::State(state): axum::extract::State<ServerState>| async move {
                use axum::response::IntoResponse;
                let css = state.config.load_style().unwrap_or_default();
                ([("content-type", "text/css")], css).into_response()
            }),
        )
        .route(
            "/__media-support",
            get(|| async {
                use axum::response::IntoResponse;
                axum::Json(crate::media_support::system().clone()).into_response()
            }),
        )
        .route("/__media-probe", get(media_probe))
        .route("/panel/:name/*path", get(panel_assets::serve))
        .route("/fsraw", get(fsraw::serve))
        .route("/thumbnail", get(thumbnail::serve))
        .route(
            "/gui/workspaces",
            get(gui_api::list_workspaces).post(gui_api::create_workspace),
        )
        .route("/gui/workspaces/:id", delete(gui_api::delete_workspace))
        .route("/gui/layout", get(gui_api::get_layout).put(gui_api::put_layout))
        .route(
            "/gui/panels/:slot/view",
            put(gui_api::put_panel_view).get(gui_api::get_panel_view),
        )
        .route("/gui/command", post(gui_api::post_command))
        .route("/gui/bench", get(gui_api::get_bench))
        .route("/gui/bench/clear", post(gui_api::clear_bench))
        .route("/gui/message", post(gui_api::post_message))
        .route("/gui/input", post(gui_api::post_input))
        .route("/gui/prompt", post(gui_api::post_prompt))
        .route("/gui/status", get(gui_api::get_status))
        // Panels run in the Svelte shell's (Tauri) origin but fetch their
        // HTML and `import()` their modules from this server's origin, so the
        // panel assets and helper modules must be CORS-readable. Permissive
        // CORS is safe because the *sensitive* routes (file contents, the
        // scripting API) are gated by the session token (spec-auth); the open
        // routes only serve shipped panel code and styling.
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(state)
}

/// The router with the session-token authentication layer (spec-auth). Only
/// the sensitive routes are gated; the static panel assets and helper modules
/// stay open because they are loaded via `import()`/`<link>` which cannot
/// carry an `Authorization` header (and they serve no private data).
///
/// Used by the GUI binary; tests drive [`build_router`] directly (no token).
pub fn build_router_authenticated(state: ServerState, token: Arc<str>) -> Router {
    build_router(state).layer(axum::middleware::from_fn_with_state(token, require_token))
}

/// Routes that expose file contents or drive the GUI, and so require the
/// session token. Everything else (panel code, styles, sink availability) is
/// open. `/fsraw`, `/thumbnail` and `/__media-probe` accept the token as a
/// `?token=` query parameter (they are loaded as `<img>/<video>` `src` or via
/// a simple GET that cannot set a header); the rest require the header.
fn is_protected(path: &str) -> bool {
    matches!(path, "/fsraw" | "/thumbnail" | "/__media-probe") || path.starts_with("/gui/")
}

fn accepts_query_token(path: &str) -> bool {
    matches!(path, "/fsraw" | "/thumbnail" | "/__media-probe")
}

async fn require_token(
    axum::extract::State(token): axum::extract::State<Arc<str>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let path = request.uri().path();
    // CORS preflight carries no Authorization header; let the CORS layer answer
    // it. Open routes pass straight through.
    if request.method() == axum::http::Method::OPTIONS || !is_protected(path) {
        return next.run(request).await;
    }

    let header = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    let mut authorized = metafolder_core::auth::bearer_token(header)
        .map(|provided| metafolder_core::auth::constant_time_eq(provided, &token))
        .unwrap_or(false);

    if !authorized && accepts_query_token(path) {
        authorized = request
            .uri()
            .query()
            .and_then(query_token)
            .map(|provided| metafolder_core::auth::constant_time_eq(provided, &token))
            .unwrap_or(false);
    }

    if authorized {
        next.run(request).await
    } else {
        (
            axum::http::StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({ "error": "missing or invalid session token" })),
        )
            .into_response()
    }
}

/// The `token` parameter from a raw query string (tokens are hex, so no
/// percent-decoding is needed).
fn query_token(query: &str) -> Option<&str> {
    query.split('&').find_map(|pair| pair.strip_prefix("token="))
}

fn javascript(source: &'static str) -> axum::response::Response {
    use axum::response::IntoResponse;
    // Panel `main.js` is cache-busted per session, but its static
    // `import '/__ui.js'` (and the other shim modules) is not, so the WebView
    // must always revalidate these or a rebuilt helper would be masked by a
    // stale cached copy.
    (
        [
            ("content-type", "text/javascript"),
            ("cache-control", "no-cache"),
        ],
        source,
    )
        .into_response()
}

#[derive(serde::Deserialize)]
struct MediaProbeParams {
    path: String,
}

/// `GET /__media-probe?path=…` — per-file codec probe for the `file`
/// panel, requested only after a media element has failed to play, to
/// report the missing decoders. `gst-discoverer-1.0` runs out of process,
/// so off the async runtime via `spawn_blocking`.
async fn media_probe(
    axum::extract::Query(params): axum::extract::Query<MediaProbeParams>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let probe = tokio::task::spawn_blocking(move || {
        crate::media_support::probe_file(std::path::Path::new(&params.path))
    })
    .await;
    match probe {
        Ok(probe) => axum::Json(probe).into_response(),
        Err(_) => axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
