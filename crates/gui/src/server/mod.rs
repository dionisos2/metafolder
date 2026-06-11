//! The GUI HTTP server (default 127.0.0.1:7524). Serves panel-type
//! directories from the config dir (shim injected into HTML), raw local
//! files for the `file` panel type, and — in v1 milestone 4 — the
//! `/gui/*` scripting API. Built as a plain `axum::Router` so tests can
//! drive it with `tower::ServiceExt::oneshot`.

mod fsraw;
mod panel_assets;

use crate::config::ConfigDir;
use axum::routing::get;
use axum::Router;
use std::sync::Arc;

const SHIM_JS: &str = include_str!("../../panel-shim/shim.js");
const KEYMATCH_JS: &str = include_str!("../../panel-shim/keymatch.js");
const RESOLVE_JS: &str = include_str!("../../panel-shim/resolve.js");

pub fn build_router(config: Arc<ConfigDir>) -> Router {
    Router::new()
        .route("/__shim.js", get(|| async { javascript(SHIM_JS) }))
        .route("/__keymatch.js", get(|| async { javascript(KEYMATCH_JS) }))
        .route("/__resolve.js", get(|| async { javascript(RESOLVE_JS) }))
        .route(
            "/__style.css",
            get(|axum::extract::State(config): axum::extract::State<Arc<ConfigDir>>| async move {
                use axum::response::IntoResponse;
                ([("content-type", "text/css")], config.load_style()).into_response()
            }),
        )
        .route("/panel/:name/*path", get(panel_assets::serve))
        .route("/fsraw", get(fsraw::serve))
        .with_state(config)
}

fn javascript(source: &'static str) -> axum::response::Response {
    use axum::response::IntoResponse;
    ([("content-type", "text/javascript")], source).into_response()
}
