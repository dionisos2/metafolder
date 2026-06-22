//! `GET /thumbnail?path=/absolute/path` — a poster-frame PNG for a video
//! file, for the `file`/`metarecord-list` thumbnail grids. Videos must never
//! be served to an `<img>` directly (WebKit would decode the whole file and
//! crash); this returns a small PNG extracted with `ffmpeg`, cached inside the
//! file's repository (`<repo>/.metafolder/internal/thumbnails`).
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
use metafolder_core::sync::MutexExt;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(serde::Deserialize)]
pub struct Params {
    path: String,
}

pub async fn serve(State(state): State<ServerState>, Query(params): Query<Params>) -> Response {
    let path = PathBuf::from(params.path);

    // Resolve the file's repository (its cache directory). A file outside any
    // repo gets no thumbnail — no ffmpeg, nothing written; the panel shows a
    // glyph.
    let Some(cache_dir) = resolve_cache_dir(&state.daemon, &path).await else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let result =
        tokio::task::spawn_blocking(move || thumbnails::generate(&path, &cache_dir)).await;
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

/// The thumbnail cache directory for `path`, or `None` when it is in no repo.
async fn resolve_cache_dir(daemon: &DaemonProxy, path: &std::path::Path) -> Option<PathBuf> {
    let repos = repo_dirs(daemon).await;
    thumbnails::match_internal_dir(&repos, path).map(|internal| internal.join("thumbnails"))
}

/// How long a fetched repository list is reused before re-querying the daemon.
/// Repos change rarely; this keeps a thumbnail grid from hitting `GET /repos`
/// once per tile.
const REPO_TTL: Duration = Duration::from_secs(3);

type RepoDirs = Vec<(PathBuf, PathBuf)>;

fn repo_cache() -> &'static Mutex<Option<(Instant, RepoDirs)>> {
    static CACHE: OnceLock<Mutex<Option<(Instant, RepoDirs)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// The loaded repositories as `(root, internal_dir)` pairs, cached for
/// [`REPO_TTL`]. A failed fetch is not cached (so a transient daemon outage
/// does not blank thumbnails for the whole TTL), and returns an empty list.
async fn repo_dirs(daemon: &DaemonProxy) -> RepoDirs {
    {
        let guard = repo_cache().lock_recover();
        if let Some((fetched, dirs)) = guard.as_ref() {
            if fetched.elapsed() < REPO_TTL {
                return dirs.clone();
            }
        }
    }
    match fetch_repo_dirs(daemon).await {
        Some(dirs) => {
            *repo_cache().lock_recover() = Some((Instant::now(), dirs.clone()));
            dirs
        }
        None => Vec::new(),
    }
}

/// Queries `GET /repos` and extracts the `(root, internal_dir)` of each loaded
/// repository. `None` on a transport/daemon failure.
async fn fetch_repo_dirs(daemon: &DaemonProxy) -> Option<RepoDirs> {
    let response = daemon.request("GET", "/repos", None).await.ok()?;
    if response.status != 200 {
        return None;
    }
    let dirs = response
        .body
        .as_array()?
        .iter()
        .filter_map(|repo| {
            let root = repo.get("root")?.as_str()?;
            let internal = repo.get("internal_dir")?.as_str()?;
            Some((PathBuf::from(root), PathBuf::from(internal)))
        })
        .collect();
    Some(dirs)
}
