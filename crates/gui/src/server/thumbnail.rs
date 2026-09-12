//! `GET /thumbnail?path=/absolute/path` — a poster-frame PNG for a video or
//! GIF file, for the `file`/`metarecord-list` thumbnail grids. Videos must
//! never be served to an `<img>` directly (WebKit would decode the whole file
//! and crash), and a GIF served raw would animate in the grid; this returns a
//! small still PNG extracted with `ffmpeg`, cached inside the file's
//! repository (`<repo>/.metafolder/internal/thumbnails`).
//!
//! The owning repository is resolved from the daemon's `GET /repos` (root +
//! `internal_dir`), the authority on repository layout — no filesystem walk.
//! A file inside no repository gets no thumbnail (the panel falls back to a
//! glyph) and nothing is written. Any non-2xx maps the failure to a plain
//! status the panel's `<img>` `onerror` treats as "show a glyph".

use super::ServerState;
use crate::daemon_proxy::DaemonProxy;
use crate::thumbnails::{self, ThumbError};
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use std::path::PathBuf;
use std::time::Duration;

#[derive(serde::Deserialize)]
pub struct Params {
    path: String,
}

pub async fn serve(State(state): State<ServerState>, Query(params): Query<Params>) -> Response {
    // The panels hand back the escaped handle they were given, so the exact
    // bytes come back here (spec-data-model "Tree names").
    let path = crate::fs_path::from_handle(&params.path);

    // Resolve the file's repository (its cache directory). A file outside any
    // repo gets no thumbnail — no ffmpeg, nothing written; the panel shows a
    // glyph.
    let Some(cache_dir) = resolve_cache_dir(&state.daemon, &path, state.repo_list_cache_ttl).await
    else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let result = tokio::task::spawn_blocking(move || thumbnails::generate(&path, &cache_dir)).await;
    match result {
        Ok(Ok(png)) => match tokio::fs::read(&png).await {
            Ok(bytes) => (
                [
                    (header::CONTENT_TYPE, "image/png"),
                    // The cache key already encodes the source's mtime/size, so
                    // a generated PNG is immutable for its URL's lifetime.
                    (header::CACHE_CONTROL, "private, max-age=86400"),
                ],
                bytes,
            )
                .into_response(),
            Err(_) => StatusCode::NOT_FOUND.into_response(),
        },
        Ok(Err(ThumbError::Unsupported)) => StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response(),
        Ok(Err(ThumbError::NotFound)) => StatusCode::NOT_FOUND.into_response(),
        Ok(Err(ThumbError::Failed)) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// The thumbnail cache directory for `path`, or `None` when it is in no repo.
async fn resolve_cache_dir(
    daemon: &DaemonProxy,
    path: &std::path::Path,
    ttl: Duration,
) -> Option<PathBuf> {
    super::repo_dirs::internal_dir(daemon, path, ttl)
        .await
        .map(|internal| internal.join("thumbnails"))
}
