//! `GET /thumbnail?path=/absolute/path` — a poster-frame PNG for a video
//! file, for the `file`/`metarecord-list` thumbnail grids. Videos must never
//! be served to an `<img>` directly (WebKit would decode the whole file and
//! crash); this returns a small, cached PNG extracted with `ffmpeg`
//! (see [`crate::thumbnails`]). Any non-2xx makes the panel fall back to a
//! glyph, so the failure modes map to plain statuses.

use crate::thumbnails::{self, ThumbError};
use axum::extract::Query;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use std::path::PathBuf;

#[derive(serde::Deserialize)]
pub struct Params {
    path: String,
}

pub async fn serve(Query(params): Query<Params>) -> Response {
    let path = PathBuf::from(params.path);
    let result = tokio::task::spawn_blocking(move || thumbnails::generate(&path)).await;
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
        Ok(Err(ThumbError::NotVideo)) => StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response(),
        Ok(Err(ThumbError::NotFound)) => StatusCode::NOT_FOUND.into_response(),
        Ok(Err(ThumbError::Failed)) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
