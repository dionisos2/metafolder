//! `GET /docs/*path` — the rendered documentation (`~/.config/metafolder/docs/`,
//! built from the checkout's docs/wiki/ and installed by
//! `metafolder-sync-config`), served verbatim. The help panel reads its pages
//! here; the directory belongs to no panel so that other readers can share it.
//! Open like the panel assets: shipped content, nothing private.

use super::panel_assets::serve_file;
use super::ServerState;
use axum::extract::{Path, State};
use axum::response::Response;

pub async fn serve(State(state): State<ServerState>, Path(path): Path<String>) -> Response {
    serve_file(&state.config.docs_dir(), &path)
}
