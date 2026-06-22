//! Filesystem watcher (spec-file-tracking "File Watcher"): translates notify
//! events into [`crate::executor::FsEvent`]s, enqueues them in the persistent
//! buffer and pings the executor. Events under `.metafolder/internal/` (the
//! daemon's own database writes) and non-UTF-8 names are skipped.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use notify::Watcher as _;

use metafolder_core::sync::MutexExt;

use crate::executor::{self, ExecutorPinger, FsEvent};
use crate::state::RepoState;

pub struct WatcherHandle {
    // Dropping the watcher stops event delivery.
    _watcher: notify::RecommendedWatcher,
}

pub fn start(repo: &Arc<RepoState>, pinger: ExecutorPinger) -> Result<WatcherHandle> {
    let watch_root = repo.config.root.clone();
    let root = repo.config.root.clone();
    let internal_dir = repo.internal_dir();
    // Weak: the watcher is owned by the RepoState; an Arc here would be a
    // reference cycle keeping the repository loaded forever.
    let repo = Arc::downgrade(repo);

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        match res {
            Ok(event) => {
                let Some(repo) = repo.upgrade() else {
                    return; // Repository unloaded.
                };
                handle_event(&repo, &root, &internal_dir, &pinger, event)
            }
            Err(err) => eprintln!("[watcher] backend error: {err}"),
        }
    })
    .context("Failed to create the filesystem watcher")?;

    watcher
        .watch(&watch_root, notify::RecursiveMode::Recursive)
        .context("Failed to watch the repository root (inotify limit reached?)")?;

    Ok(WatcherHandle { _watcher: watcher })
}

/// Converts an absolute path to the internal repo-root-relative form
/// (leading `/`, `/` separators). None for paths outside the root, under
/// `.metafolder/internal/`, or with non-UTF-8 names (skipped with a warning).
fn relative(root: &Path, internal_dir: &Path, abs: &Path) -> Option<String> {
    if abs.starts_with(internal_dir) {
        return None;
    }
    let rel = abs.strip_prefix(root).ok()?;
    let mut out = String::new();
    for comp in rel.components() {
        let std::path::Component::Normal(name) = comp else {
            return None;
        };
        let Some(name) = name.to_str() else {
            eprintln!("[watcher] skipping non-UTF-8 name under {abs:?}");
            return None;
        };
        out.push('/');
        out.push_str(name);
    }
    if out.is_empty() {
        None // The root itself.
    } else {
        Some(out)
    }
}

fn handle_event(
    repo: &RepoState,
    root: &Path,
    internal_dir: &Path,
    pinger: &ExecutorPinger,
    event: notify::Event,
) {
    use notify::event::{ModifyKind, RenameMode};

    let rel = |p: &Path| relative(root, internal_dir, p);
    // The inotify rename cookie correlates a split From/To pair; carried so the
    // executor can fuse them back into one rename (see `correlate_renames`).
    let cookie = event.attrs.tracker().map(|c| c as i64);
    let mut events: Vec<(FsEvent, Option<i64>)> = Vec::new();
    match event.kind {
        notify::EventKind::Create(_) => {
            events.extend(event.paths.iter().filter_map(|p| rel(p)).map(|p| (FsEvent::Create(p), None)));
        }
        notify::EventKind::Remove(_) => {
            events.extend(event.paths.iter().filter_map(|p| rel(p)).map(|p| (FsEvent::Remove(p), None)));
        }
        notify::EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
            if let [from, to] = event.paths.as_slice() {
                match (rel(from), rel(to)) {
                    (Some(a), Some(b)) => events.push((FsEvent::Rename(a, b), None)),
                    // One side is outside the watched scope (e.g. into
                    // .metafolder/internal/): degrade to the one-sided forms.
                    (Some(a), None) => events.push((FsEvent::RenameFrom(a), cookie)),
                    (None, Some(b)) => events.push((FsEvent::RenameTo(b), cookie)),
                    (None, None) => {}
                }
            }
        }
        notify::EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
            events.extend(event.paths.iter().filter_map(|p| rel(p)).map(|p| (FsEvent::RenameFrom(p), cookie)));
        }
        notify::EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
            events.extend(event.paths.iter().filter_map(|p| rel(p)).map(|p| (FsEvent::RenameTo(p), cookie)));
        }
        notify::EventKind::Modify(ModifyKind::Metadata(_)) => {
            events.extend(event.paths.iter().filter_map(|p| rel(p)).map(|p| (FsEvent::ModifyMeta(p), None)));
        }
        // Data modifications; unknown Modify kinds fall back to Data
        // semantics (full refresh + hash invalidation, spec-platform).
        notify::EventKind::Modify(ModifyKind::Data(_))
        | notify::EventKind::Modify(ModifyKind::Any) => {
            events.extend(event.paths.iter().filter_map(|p| rel(p)).map(|p| (FsEvent::ModifyData(p), None)));
        }
        _ => {}
    }

    if events.is_empty() {
        return;
    }
    let conn = repo.conn.lock_recover();
    for (ev, tracker) in &events {
        if let Err(err) = executor::enqueue(&conn, ev, *tracker) {
            eprintln!("[watcher] failed to enqueue {ev:?}: {err:#}");
        }
    }
    drop(conn);
    pinger.ping();
}

#[cfg(test)]
mod tests {
    use super::relative;
    use std::path::Path;

    #[test]
    fn test_relative_skips_internal_dir_only() {
        let root = Path::new("/repo");
        let internal = Path::new("/repo/.metafolder/internal");
        let rel = |p: &str| relative(root, internal, Path::new(p));

        assert_eq!(rel("/repo/a.txt").as_deref(), Some("/a.txt"));
        assert_eq!(
            rel("/repo/.metafolder/config.json").as_deref(),
            Some("/.metafolder/config.json")
        );
        assert_eq!(rel("/repo/.metafolder/internal/db.sqlite"), None);
        assert_eq!(rel("/repo/.metafolder/internal/db.sqlite-wal"), None);
        assert_eq!(rel("/elsewhere/x"), None);
        assert_eq!(rel("/repo"), None);
    }

    #[test]
    fn test_relative_handles_external_metafolder_inside_root() {
        // root = "/" with the metafolder elsewhere inside it: only the
        // internal/ directory is excluded, by absolute path.
        let root = Path::new("/");
        let internal = Path::new("/home/.metafolder/internal");
        let rel = |p: &str| relative(root, internal, Path::new(p));

        assert_eq!(rel("/etc/hosts").as_deref(), Some("/etc/hosts"));
        assert_eq!(
            rel("/home/.metafolder/config.json").as_deref(),
            Some("/home/.metafolder/config.json")
        );
        assert_eq!(rel("/home/.metafolder/internal/db.sqlite"), None);
    }
}
