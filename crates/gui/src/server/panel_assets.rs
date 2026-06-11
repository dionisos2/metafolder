//! `GET /panel/:name/*path` — files of a panel type directory, with the
//! metafolder shim `<script>` injected into HTML documents.

use super::ServerState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use std::path::Component;

const SHIM_TAG: &str = concat!(
    r#"<link rel="stylesheet" href="/__style.css">"#,
    r#"<script type="module" src="/__shim.js"></script>"#
);

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
    if mime == "text/html" {
        let html = String::from_utf8_lossy(&content);
        return ([("content-type", mime)], inject_shim(&html)).into_response();
    }
    ([("content-type", mime)], content).into_response()
}

/// Injects the shim script tag right after `<head>`, or prepends it when
/// the document has no head element.
fn inject_shim(html: &str) -> String {
    match html.find("<head>") {
        Some(index) => {
            let insert_at = index + "<head>".len();
            format!("{}{}{}", &html[..insert_at], SHIM_TAG, &html[insert_at..])
        }
        None => format!("{SHIM_TAG}{html}"),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inject_shim_after_head() {
        let html = "<html><head><title>t</title></head><body></body></html>";
        let injected = inject_shim(html);
        let expected = format!("<html><head>{SHIM_TAG}<title>");
        assert!(injected.starts_with(&expected));
    }

    #[test]
    fn test_inject_shim_without_head_prepends() {
        let injected = inject_shim("<p>bare</p>");
        assert!(injected.starts_with(SHIM_TAG));
        assert!(injected.ends_with("<p>bare</p>"));
    }
}
