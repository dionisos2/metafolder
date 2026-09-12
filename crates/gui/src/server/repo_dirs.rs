//! Where a local file's generated artifacts are cached: the `internal_dir` of
//! the repository holding it.
//!
//! Both generators (`/thumbnail` poster frames, `/document` page renders) write
//! beside the repository's database rather than into a global cache, so the
//! artifacts travel and vanish with the repository. The owning repository comes
//! from the daemon's `GET /repos` — the authority on repository layout, so no
//! filesystem walk is needed — and the answer is cached briefly, since a
//! thumbnail grid would otherwise ask once per tile.

use crate::daemon_proxy::DaemonProxy;
use metafolder_core::sync::MutexExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

type RepoDirs = Vec<(PathBuf, PathBuf)>;

/// The internal directory of the repository holding `path`, or `None` when it
/// lies inside none (the caller then produces nothing and the panel falls back
/// to a glyph).
pub async fn internal_dir(daemon: &DaemonProxy, path: &Path, ttl: Duration) -> Option<PathBuf> {
    let repos = repo_dirs(daemon, ttl).await;
    crate::thumbnails::match_internal_dir(&repos, path)
}

fn repo_cache() -> &'static Mutex<Option<(Instant, RepoDirs)>> {
    static CACHE: OnceLock<Mutex<Option<(Instant, RepoDirs)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// The loaded repositories as `(root, internal_dir)` pairs, cached for `ttl`
/// (config.toml `[settings] repo-list-cache-ttl-secs`). A failed fetch is not
/// cached (so a transient daemon outage does not blank thumbnails for the whole
/// TTL), and returns an empty list.
async fn repo_dirs(daemon: &DaemonProxy, ttl: Duration) -> RepoDirs {
    {
        let guard = repo_cache().lock_recover();
        if let Some((fetched, dirs)) = guard.as_ref() {
            if fetched.elapsed() < ttl {
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
