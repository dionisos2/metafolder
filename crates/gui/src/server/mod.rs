//! The GUI HTTP server (default 127.0.0.1:7524). Serves panel-type
//! directories from the config dir (shim injected into HTML), raw local
//! files for the `file` panel type, and the `/gui/*` scripting API.
//! Built as a plain `axum::Router` so tests can drive it with
//! `tower::ServiceExt::oneshot`.

mod fsraw;
mod gui_api;
pub mod input_wait;
mod panel_assets;

use crate::config::ConfigDir;
use crate::daemon_proxy::DaemonProxy;
use crate::keybindings::KeybindingSet;
use crate::state::GuiState;
use axum::routing::{delete, get, post, put};
use axum::Router;
use input_wait::InputWait;
use std::sync::{Arc, Mutex};

const SHIM_JS: &str = include_str!("../../panel-shim/shim.js");
const KEYMATCH_JS: &str = include_str!("../../panel-shim/keymatch.js");
const RESOLVE_JS: &str = include_str!("../../panel-shim/resolve.js");
const UI_JS: &str = include_str!("../../panel-shim/ui.js");
const MENU_JS: &str = include_str!("../../panel-shim/menu.js");

#[derive(Clone)]
pub struct ServerState {
    pub config: Arc<ConfigDir>,
    pub gui: Arc<GuiState>,
    pub daemon: Arc<DaemonProxy>,
    pub keybindings: Arc<Mutex<KeybindingSet>>,
    pub input: Arc<InputWait>,
}

pub fn build_router(state: ServerState) -> Router {
    Router::new()
        .route("/__shim.js", get(|| async { javascript(SHIM_JS) }))
        .route("/__keymatch.js", get(|| async { javascript(KEYMATCH_JS) }))
        .route("/__resolve.js", get(|| async { javascript(RESOLVE_JS) }))
        .route("/__ui.js", get(|| async { javascript(UI_JS) }))
        .route("/__menu.js", get(|| async { javascript(MENU_JS) }))
        .route(
            "/__style.css",
            get(|axum::extract::State(state): axum::extract::State<ServerState>| async move {
                use axum::response::IntoResponse;
                ([("content-type", "text/css")], state.config.load_style()).into_response()
            }),
        )
        .route(
            "/__media-support",
            get(|| async {
                use axum::response::IntoResponse;
                axum::Json(crate::media_support::system().clone()).into_response()
            }),
        )
        .route("/panel/:name/*path", get(panel_assets::serve))
        .route("/fsraw", get(fsraw::serve))
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
        .route("/gui/message", post(gui_api::post_message))
        .route("/gui/input", post(gui_api::post_input))
        .route("/gui/prompt", post(gui_api::post_prompt))
        .route("/gui/status", get(gui_api::get_status))
        .with_state(state)
}

fn javascript(source: &'static str) -> axum::response::Response {
    use axum::response::IntoResponse;
    ([("content-type", "text/javascript")], source).into_response()
}
