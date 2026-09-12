//! `GET /document?path=…&page=N&dpi=D` — one page of a document (PDF) as a
//! PNG, and `GET /document/info?path=…` — its page count.
//!
//! The `file` panel previews a document by showing these images. It cannot show
//! the file itself: `/fsraw` carries the session token in the URL, so anything
//! loaded from it *as a document* (an `<iframe>`, where WebKit's own PDF.js
//! would take over) would run as code in this server's origin and could lift
//! the token — see `fsraw.rs` and `tests/panel_invariants.rs`. Poppler renders
//! the page out of process under `bwrap` instead, and only the PNG crosses into
//! the web process.
//!
//! Rendering writes a cache, so it needs the file's repository
//! (`<repo>/.metafolder/internal/documents`) exactly as `/thumbnail` does; a
//! file inside no repository gets no page. Counting pages writes nothing, so it
//! answers for any readable file.

use super::ServerState;
use crate::documents::{self, DocError};
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

#[derive(serde::Deserialize)]
pub struct PageParams {
    path: String,
    /// 1-based page number; the first page when absent.
    page: Option<u32>,
    /// Render resolution, clamped by `documents`; the panel's configured
    /// default when absent.
    dpi: Option<u32>,
}

#[derive(serde::Deserialize)]
pub struct InfoParams {
    path: String,
}

/// Resolution used when the panel names none.
const DEFAULT_DPI: u32 = 150;

pub async fn page(State(state): State<ServerState>, Query(params): Query<PageParams>) -> Response {
    // The panels hand back the escaped handle they were given, so the exact
    // bytes come back here (spec-data-model "Tree names").
    let path = crate::fs_path::from_handle(&params.path);
    if !documents::is_document(&path) {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    }
    let Some(internal) =
        super::repo_dirs::internal_dir(&state.daemon, &path, state.repo_list_cache_ttl).await
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let cache_dir = internal.join("documents");
    let page = params.page.unwrap_or(1);
    let dpi = params.dpi.unwrap_or(DEFAULT_DPI);

    let rendered =
        tokio::task::spawn_blocking(move || documents::render_page(&path, page, dpi, &cache_dir))
            .await;
    match rendered {
        Ok(Ok(png)) => match tokio::fs::read(&png).await {
            Ok(bytes) => (
                [
                    (header::CONTENT_TYPE, "image/png"),
                    // The cache key already encodes the source's mtime/size and
                    // the rendering, so the PNG is immutable for its URL.
                    (header::CACHE_CONTROL, "private, max-age=86400"),
                ],
                bytes,
            )
                .into_response(),
            Err(_) => StatusCode::NOT_FOUND.into_response(),
        },
        Ok(Err(error)) => status_of(error).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub async fn info(Query(params): Query<InfoParams>) -> Response {
    let path = crate::fs_path::from_handle(&params.path);
    if !documents::is_document(&path) {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    }
    match tokio::task::spawn_blocking(move || documents::page_count(&path)).await {
        Ok(Ok(pages)) => axum::Json(serde_json::json!({ "pages": pages })).into_response(),
        Ok(Err(error)) => status_of(error).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Any non-2xx makes the panel fall back to a message or a glyph; the
/// distinction is for the developer reading the network log.
fn status_of(error: DocError) -> StatusCode {
    match error {
        DocError::Unsupported => StatusCode::UNSUPPORTED_MEDIA_TYPE,
        DocError::NotFound => StatusCode::NOT_FOUND,
        DocError::Failed => StatusCode::INTERNAL_SERVER_ERROR,
    }
}
