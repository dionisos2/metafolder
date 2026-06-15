//! `GET /panel/:name/*path` — files of a panel type directory, served
//! verbatim. The shell fetches `index.html` and `import()`s `main.js` into a
//! Shadow DOM root (it injects the API and stylesheet itself), so no markup
//! is rewritten here.

use super::ServerState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use std::path::Component;

pub async fn serve(
    State(state): State<ServerState>,
    Path((name, path)): Path<(String, String)>,
) -> Response {
    let Some(panel_dir) = state.config.panel_dir(&name) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    // Only plain file-name components: no '..', no absolute segments.
    let relative = std::path::Path::new(&path);
    if !relative.components().all(|c| matches!(c, Component::Normal(_))) {
        return StatusCode::NOT_FOUND.into_response();
    }

    let file = panel_dir.join(relative);
    let Ok(content) = std::fs::read(&file) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let mime = mime_for(&file);
    ([("content-type", mime)], content).into_response()
}

fn mime_for(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" | "htm" => "text/html",
        "js" | "mjs" => "text/javascript",
        "css" => "text/css",
        "json" => "application/json",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "txt" | "md" => "text/plain",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}
