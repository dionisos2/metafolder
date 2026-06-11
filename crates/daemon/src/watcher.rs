//! Filesystem watcher (spec-file-tracking "File Watcher"): translates notify
//! events into [`crate::executor::FsEvent`]s, enqueues them in the persistent
//! buffer and pings the executor. Events under `.metafolder/` and non-UTF-8
//! names are skipped.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use notify::Watcher as _;

use crate::executor::{self, ExecutorPinger, FsEvent};
use crate::state::RepoState;

pub struct WatcherHandle {
    // Dropping the watcher stops event delivery.
    _watcher: notify::RecommendedWatcher,
}

pub fn start(repo: &Arc<RepoState>, pinger: ExecutorPinger) -> Result<WatcherHandle> {
    let watch_root = repo.config.root.clone();
    let root = repo.config.root.clone();
    let metafolder_dir = repo.metafolder_dir.clone();
    // Weak: the watcher is owned by the RepoState; an Arc here would be a
    // reference cycle keeping the repository loaded forever.
    let repo = Arc::downgrade(repo);

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        match res {
            Ok(event) => {
                let Some(repo) = repo.upgrade() else {
                    return; // Repository unloaded.
                };
                handle_event(&repo, &root, &metafolder_dir, &pinger, event)
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
/// `.metafolder/`, or with non-UTF-8 names (skipped with a warning).
fn relative(root: &Path, metafolder_dir: &Path, abs: &Path) -> Option<String> {
    if abs.starts_with(metafolder_dir) {
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
    metafolder_dir: &Path,
    pinger: &ExecutorPinger,
    event: notify::Event,
) {
    use notify::event::{ModifyKind, RenameMode};

    let rel = |p: &Path| relative(root, metafolder_dir, p);
    let mut events: Vec<FsEvent> = Vec::new();
    match event.kind {
        notify::EventKind::Create(_) => {
            events.extend(event.paths.iter().filter_map(|p| rel(p)).map(FsEvent::Create));
        }
        notify::EventKind::Remove(_) => {
            events.extend(event.paths.iter().filter_map(|p| rel(p)).map(FsEvent::Remove));
        }
        notify::EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
            if let [from, to] = event.paths.as_slice() {
                match (rel(from), rel(to)) {
                    (Some(a), Some(b)) => events.push(FsEvent::Rename(a, b)),
                    // One side is outside the watched scope (e.g. into
                    // .metafolder/): degrade to the one-sided forms.
                    (Some(a), None) => events.push(FsEvent::RenameFrom(a)),
                    (None, Some(b)) => events.push(FsEvent::RenameTo(b)),
                    (None, None) => {}
                }
            }
        }
        notify::EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
            events.extend(event.paths.iter().filter_map(|p| rel(p)).map(FsEvent::RenameFrom));
        }
        notify::EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
            events.extend(event.paths.iter().filter_map(|p| rel(p)).map(FsEvent::RenameTo));
        }
        notify::EventKind::Modify(ModifyKind::Metadata(_)) => {
            events.extend(event.paths.iter().filter_map(|p| rel(p)).map(FsEvent::ModifyMeta));
        }
        // Data modifications; unknown Modify kinds fall back to Data
        // semantics (full refresh + hash invalidation, spec-platform).
        notify::EventKind::Modify(ModifyKind::Data(_))
        | notify::EventKind::Modify(ModifyKind::Any) => {
            events.extend(event.paths.iter().filter_map(|p| rel(p)).map(FsEvent::ModifyData));
        }
        _ => {}
    }

    if events.is_empty() {
        return;
    }
    let conn = repo.conn.lock().unwrap();
    for ev in &events {
        if let Err(err) = executor::enqueue(&conn, ev) {
            eprintln!("[watcher] failed to enqueue {ev:?}: {err:#}");
        }
    }
    drop(conn);
    pinger.ping();
}
