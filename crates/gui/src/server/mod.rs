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
        // panel assets and helper modules must be CORS-readable. The server is
        // bound to 127.0.0.1, so a permissive policy is acceptable.
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(state)
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
