//! Filesystem watcher (spec-file-tracking "File Watcher"): translates notify
//! events into [`crate::executor::FsEvent`]s, enqueues them in the persistent
//! buffer and pings the executor. Events under `.metafolder/internal/` (the
//! daemon's own database writes) and non-UTF-8 names are skipped.
//!
//! Watches are placed **only on eligible directories** (the opt-in
//! `mf_watch`/`mf_ignore` scope, spec-file-tracking "Watch and Ignore"), one
//! non-recursive inotify watch per directory. This matches the semantics of the
//! reconcile walk ([`crate::reconcile`]): symlinked directories are never
//! followed (so the watch cannot escape the repository root) and an unreadable
//! directory is skipped rather than aborting the whole watch. A fresh repository
//! (`mf_watch = false`) is therefore watched nowhere at all. The set is
//! recomputed by [`WatcherHandle::refresh`] whenever a manual write changes
//! eligibility, and maintained incrementally as directories appear/disappear.
//!
//! **The notify event callback must never block.** notify's inotify backend
//! serves `watch()`/`unwatch()` requests from the same thread that delivers
//! events, so a callback that waits on a lock also stops every watch placement —
//! and any thread that asks for one while holding that lock deadlocks with it
//! (the repository connection is held across `refresh` at every route commit
//! site). The callback therefore only translates the event and hands it to the
//! ingest thread ([`start`]), which does the database and watch work.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use notify::Watcher as _;
use rusqlite::Connection;

use metafolder_core::sync::MutexExt;

use crate::eligibility::{self, EligibilityCache};
use crate::executor::{self, ExecutorPinger, FsEvent};
use crate::state::RepoState;
use crate::tree_cache::TreeCache;

/// Shared watcher state. Behind mutexes so both the refresh path (manual writes
/// changing eligibility) and the event callback (directories created/removed at
/// runtime) can adjust the live watch set. `watcher` is `Option` only during
/// construction: it is set once, right after the notify watcher is created.
struct WatcherInner {
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
    /// Absolute paths of the directories currently watched.
    watched: Mutex<HashSet<PathBuf>>,
}

impl WatcherInner {
    /// Adds a non-recursive watch on `dir` (idempotent). Failures warn and are
    /// swallowed: one unreadable/racing directory must never abort watching the
    /// rest (spec-file-tracking "File Watcher").
    fn watch_dir(&self, dir: &Path) {
        let mut watched = self.watched.lock_recover();
        if !watched.insert(dir.to_path_buf()) {
            return; // Already watched.
        }
        if let Some(w) = self.watcher.lock_recover().as_mut() {
            if let Err(err) = w.watch(dir, notify::RecursiveMode::NonRecursive) {
                eprintln!("[watcher] failed to watch {dir:?}: {err}");
                watched.remove(dir);
            }
        }
    }

    /// Drops the watch on `dir` (its subtree left the eligible scope).
    fn unwatch_dir(&self, dir: &Path) {
        if self.watched.lock_recover().remove(dir) {
            if let Some(w) = self.watcher.lock_recover().as_mut() {
                let _ = w.unwatch(dir); // WatchNotFound is fine (already gone).
            }
        }
    }

    /// Forgets `dir` and every descendant from the watched set *without*
    /// calling unwatch — used when the kernel already dropped the watches
    /// because the directory was deleted or moved away.
    fn forget_subtree(&self, dir: &Path) {
        self.watched.lock_recover().retain(|p| !p.starts_with(dir));
    }

    /// Reconciles the live watches to `target`: unwatch what is no longer
    /// wanted, watch what is newly wanted.
    fn apply(&self, target: &HashSet<PathBuf>) {
        let current: Vec<PathBuf> = self.watched.lock_recover().iter().cloned().collect();
        for dir in &current {
            if !target.contains(dir) {
                self.unwatch_dir(dir);
            }
        }
        for dir in target {
            self.watch_dir(dir);
        }
    }
}

pub struct WatcherHandle {
    // Dropping the last strong `Arc` drops the notify watcher (stopping event
    // delivery). The event callback holds only a `Weak`, so it is not a cycle.
    inner: Arc<WatcherInner>,
}

impl WatcherHandle {
    /// Recomputes the eligible-directory set and reconciles the live watches to
    /// it. Called after a manual write changes `mf_watch`/`mf_ignore`. Takes the
    /// already-locked connection and tree cache to avoid re-locking them.
    pub fn refresh(
        &self,
        conn: &Connection,
        cache: &mut TreeCache,
        root: &Path,
        internal_dir: &Path,
    ) {
        let (target, total, elig) = compute_watched_dirs_timed(conn, cache, root, internal_dir);
        // A persistent diagnostic for the initial (load-time) walk and any large
        // watch reconfiguration: the filesystem read_dir cost vs the per-directory
        // eligibility cost (served from the tree cache once warm).
        if total.as_millis() >= 100 {
            eprintln!(
                "[watcher] walk: {} dirs in {total:?} (fs {:?} + eligibility {elig:?})",
                target.len(),
                total.saturating_sub(elig)
            );
        }
        self.inner.apply(&target);
    }
}

pub fn start(repo: &Arc<RepoState>, pinger: ExecutorPinger) -> Result<WatcherHandle> {
    let root = repo.config.root.clone();
    let internal_dir = repo.internal_dir();

    let inner = Arc::new(WatcherInner {
        watcher: Mutex::new(None),
        watched: Mutex::new(HashSet::new()),
    });

    // Weaks: neither the ingest thread nor the callback may keep the repository
    // (and its exclusive lock) or the watcher alive.
    let repo_weak = Arc::downgrade(repo);
    let inner_weak = Arc::downgrade(&inner);

    // The ingest thread does everything that can block — the database enqueue
    // and the watch maintenance for new directories. It ends when the sender
    // dies with the notify watcher (repository unloaded).
    let (tx, rx) = std::sync::mpsc::channel::<Vec<(FsEvent, Option<i64>)>>();
    let ingest_root = root.clone();
    let ingest_internal = internal_dir.clone();
    std::thread::spawn(move || {
        while let Ok(events) = rx.recv() {
            let Some(repo) = repo_weak.upgrade() else {
                return; // Repository unloaded.
            };
            let inner = inner_weak.upgrade();
            ingest(&repo, &ingest_root, &ingest_internal, &pinger, inner.as_deref(), events);
        }
    });

    let cb_root = root.clone();
    let cb_internal = internal_dir.clone();
    let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        // Runs on notify's event-loop thread: translate and hand off, never
        // block (see the module documentation).
        match res {
            Ok(event) => {
                let events = translate(&cb_root, &cb_internal, event);
                if !events.is_empty() {
                    let _ = tx.send(events); // The ingest thread is gone: unloaded.
                }
            }
            Err(err) => eprintln!("[watcher] backend error: {err}"),
        }
    })
    .context("Failed to create the filesystem watcher")?;
    *inner.watcher.lock_recover() = Some(watcher);

    // No initial placement here: the eligible-directory walk needs the tree
    // cache, so it is deferred to the end of the load warmup (which populates the
    // cache) via `RepoState::refresh_watches` — there each directory's
    // eligibility is served from memory instead of a per-directory DB walk. Until
    // then the watcher holds no watches (a fresh repo watches nothing anyway).
    Ok(WatcherHandle { inner })
}

/// The set of directories that should be watched: every eligible directory,
/// reached by descending only through eligible directories (matching
/// [`crate::reconcile`]'s walk). Symlinked directories are not followed
/// (`file_type().is_dir()` is false for a symlink), unreadable directories are
/// skipped, and `.metafolder/internal/` is always excluded. Read-only.
/// Computes the set of directories that should be watched, returning the wall
/// time spent in eligibility checks (the DB / tree-cache part) alongside the
/// total, so the filesystem-walk part (`total − eligibility`) can be told apart
/// — a persistent diagnostic (see [`WatcherHandle::refresh`]) and the basis for
/// deferring the initial walk until the tree cache is warm (eligibility served
/// from memory rather than a per-directory DB walk).
pub fn compute_watched_dirs_timed(
    conn: &Connection,
    cache: &mut TreeCache,
    root: &Path,
    internal_dir: &Path,
) -> (HashSet<PathBuf>, std::time::Duration, std::time::Duration) {
    let start = std::time::Instant::now();
    let mut elig = std::time::Duration::ZERO;
    let mut ec = EligibilityCache::default();
    let mut out = HashSet::new();
    // The root directory is watched iff the root metarecord is eligible
    // (`mf_watch = true` set directly on it — the opt-in default is false).
    let t = std::time::Instant::now();
    let root_eligible = eligibility::is_eligible_cached(conn, cache, "", &mut ec);
    elig += t.elapsed();
    match root_eligible {
        Ok(true) => {
            out.insert(root.to_path_buf());
        }
        Ok(false) => return (out, start.elapsed(), elig),
        Err(err) => {
            eprintln!("[watcher] root eligibility check failed: {err:#}");
            return (out, start.elapsed(), elig);
        }
    }
    collect_eligible_dirs(conn, cache, root, internal_dir, "", &mut ec, &mut out, &mut elig);
    let total = start.elapsed();
    (out, total, elig)
}

/// Depth-first descent from the eligible directory `base` (repo-root-relative,
/// `""` for the root), inserting the absolute path of every eligible descendant
/// directory into `out`.
#[allow(clippy::too_many_arguments)]
fn collect_eligible_dirs(
    conn: &Connection,
    cache: &mut TreeCache,
    root: &Path,
    internal_dir: &Path,
    base: &str,
    ec: &mut EligibilityCache,
    out: &mut HashSet<PathBuf>,
    elig: &mut std::time::Duration,
) {
    let mut stack = vec![base.to_string()];
    while let Some(dir) = stack.pop() {
        let abs = if dir.is_empty() {
            root.to_path_buf()
        } else {
            root.join(dir.trim_start_matches('/'))
        };
        let entries = match std::fs::read_dir(&abs) {
            Ok(entries) => entries,
            Err(_) => continue, // Not a directory, or unreadable (EACCES): skip.
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path == internal_dir {
                continue;
            }
            // `file_type` comes from the dir entry (no stat, no symlink follow):
            // a symlinked directory reports `is_symlink()`/`!is_dir()`, so it is
            // never descended into or watched — the watch cannot escape the root.
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue; // Non-UTF-8 name: skip (a reconcile can handle it).
            };
            let rel = format!("{dir}/{name}");
            let t = std::time::Instant::now();
            let eligible = eligibility::is_eligible_cached(conn, cache, &rel, ec);
            *elig += t.elapsed();
            match eligible {
                Ok(true) => {
                    out.insert(path);
                    stack.push(rel);
                }
                Ok(false) => {} // Ineligible directory: pruned (cascading skip).
                Err(err) => eprintln!("[watcher] eligibility check for {rel:?} failed: {err:#}"),
            }
        }
    }
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

/// Translates one notify event into the internal [`FsEvent`] forms. Pure: no
/// locks, no database, no watch calls — it runs on notify's event-loop thread.
fn translate(root: &Path, internal_dir: &Path, event: notify::Event) -> Vec<(FsEvent, Option<i64>)> {
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

    events
}

/// Persists a batch of translated events and keeps the live watch set in step.
/// Runs on the ingest thread — never on notify's event-loop thread.
fn ingest(
    repo: &RepoState,
    root: &Path,
    internal_dir: &Path,
    pinger: &ExecutorPinger,
    inner: Option<&WatcherInner>,
    events: Vec<(FsEvent, Option<i64>)>,
) {
    let conn = repo.conn.lock_recover();
    for (ev, tracker) in &events {
        if let Err(err) = executor::enqueue(&conn, ev, *tracker) {
            eprintln!("[watcher] failed to enqueue {ev:?}: {err:#}");
        }
    }
    drop(conn);
    pinger.ping();

    // Keep the live watch set in step with directories that appeared or vanished
    // (recursive watching is re-implemented here per-directory, so unlike
    // notify's own recursive mode this must be done by hand). The executor's
    // `scan_dir` ingests any content already inside a new directory; here we only
    // register the inotify watches for its *future* events.
    if let Some(inner) = inner {
        maintain_watches(repo, root, internal_dir, inner, &events);
    }
}

/// Adds/removes watches for directory arrivals/departures in `events`. An
/// arrival that is an eligible directory gets a watch on its whole (eligible)
/// subtree; a departure has its subtree forgotten (the kernel already dropped
/// those watches when the directory was removed or moved away).
fn maintain_watches(
    repo: &RepoState,
    root: &Path,
    internal_dir: &Path,
    inner: &WatcherInner,
    events: &[(FsEvent, Option<i64>)],
) {
    let mut arrivals: Vec<&str> = Vec::new();
    let mut departures: Vec<&str> = Vec::new();
    for (ev, _) in events {
        match ev {
            FsEvent::Create(p) | FsEvent::RenameTo(p) => arrivals.push(p),
            FsEvent::Remove(p) | FsEvent::RenameFrom(p) => departures.push(p),
            FsEvent::Rename(from, to) => {
                departures.push(from);
                arrivals.push(to);
            }
            FsEvent::ModifyData(_) | FsEvent::ModifyMeta(_) => {}
        }
    }
    if arrivals.is_empty() && departures.is_empty() {
        return;
    }
    for rel in departures {
        inner.forget_subtree(&root.join(rel.trim_start_matches('/')));
    }
    if arrivals.is_empty() {
        return;
    }
    // Arrivals need eligibility checks (conn + cache). Collect the whole set
    // first and release both locks before placing any watch: `watch()` waits for
    // notify's event loop, which must never be made to wait for the connection
    // (see the module documentation).
    let mut subtree = HashSet::new();
    {
        let conn = repo.conn.lock_recover();
        let mut cache = repo.lock_cache();
        let mut ec = EligibilityCache::default();
        for rel in arrivals {
            let abs = root.join(rel.trim_start_matches('/'));
            // Only a real directory (not a symlink) that is eligible is watched.
            match std::fs::symlink_metadata(&abs) {
                Ok(md) if md.file_type().is_dir() => {}
                _ => continue,
            }
            match eligibility::is_eligible_cached(&conn, &mut cache, rel, &mut ec) {
                Ok(true) => {}
                _ => continue,
            }
            subtree.insert(abs);
            let mut elig = std::time::Duration::ZERO;
            collect_eligible_dirs(
                &conn, &mut cache, root, internal_dir, rel, &mut ec, &mut subtree, &mut elig,
            );
        }
    }
    for dir in &subtree {
        inner.watch_dir(dir);
    }
}

#[cfg(test)]
mod tests {
    use super::{compute_watched_dirs_timed, relative};
    use crate::db;
    use crate::log::Writer;
    use crate::tree_cache::TreeCache;
    use metafolder_core::metarecord::{Field, Value};
    use rusqlite::Connection;
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};

    /// A repository whose root metarecord carries `mf_watch = watch` (and no
    /// `mf_ignore`), backed by a real temporary directory on disk. Child paths
    /// need no metarecords: eligibility is inherited from the root, exactly as
    /// during a first reconcile.
    struct Fixture {
        conn: Connection,
        cache: TreeCache,
        root: PathBuf,
    }

    impl Fixture {
        fn new(watch: bool) -> Self {
            let root = std::env::temp_dir().join("metafolder-tests")
                .join(format!("metafolder_watch_{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            let mut conn = db::open_in_memory().unwrap();
            db::init_schema(&conn).unwrap();
            let mut w = Writer::begin(&mut conn, None).unwrap();
            w.create_metarecord(vec![
                Field::new("mfr_path", Value::TreeRef { parent: None, name: "".into() }),
                Field::new("mf_watch", Value::Bool(watch)),
            ])
            .unwrap();
            w.commit().unwrap();
            Self { conn, cache: TreeCache::new(false), root }
        }

        fn internal_dir(&self) -> PathBuf {
            self.root.join(".metafolder").join("internal")
        }

        fn watched(&mut self) -> HashSet<PathBuf> {
            let internal = self.internal_dir();
            let root = self.root.clone();
            compute_watched_dirs_timed(&self.conn, &mut self.cache, &root, &internal).0
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            // Restore perms so an unreadable test dir can be removed.
            for entry in walk_all(&self.root) {
                if let Ok(md) = std::fs::symlink_metadata(&entry) {
                    if md.is_dir() {
                        use std::os::unix::fs::PermissionsExt;
                        let mut p = md.permissions();
                        p.set_mode(0o755);
                        let _ = std::fs::set_permissions(&entry, p);
                    }
                }
            }
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn walk_all(root: &Path) -> Vec<PathBuf> {
        let mut out = vec![root.to_path_buf()];
        if let Ok(entries) = std::fs::read_dir(root) {
            for e in entries.flatten() {
                let p = e.path();
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    out.extend(walk_all(&p));
                }
            }
        }
        out
    }

    #[test]
    fn test_nothing_watched_when_root_mf_watch_false() {
        let mut fx = Fixture::new(false);
        std::fs::create_dir_all(fx.root.join("a/b")).unwrap();
        assert!(fx.watched().is_empty(), "a fresh repo (mf_watch=false) watches nothing");
    }

    #[test]
    fn test_watches_only_eligible_directories() {
        let mut fx = Fixture::new(true);
        std::fs::create_dir_all(fx.root.join("a/b")).unwrap();
        std::fs::create_dir_all(fx.root.join("c")).unwrap();
        std::fs::write(fx.root.join("f.txt"), b"x").unwrap();

        let watched = fx.watched();
        let expect: HashSet<PathBuf> = ["", "a", "a/b", "c"]
            .iter()
            .map(|p| if p.is_empty() { fx.root.clone() } else { fx.root.join(p) })
            .collect();
        assert_eq!(watched, expect, "watch every directory (root incl.), no files");
    }

    #[test]
    fn test_symlinked_directory_is_not_followed() {
        let mut fx = Fixture::new(true);
        std::fs::create_dir_all(fx.root.join("real")).unwrap();
        // A symlink pointing at an unreadable directory outside the repo — the
        // Wine `z: -> /` case. It must be neither watched nor followed, and must
        // not raise an error.
        let outside =
            std::env::temp_dir().join("metafolder-tests").join(format!("metafolder_out_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, fx.root.join("link")).unwrap();

        let watched = fx.watched();
        assert!(watched.contains(&fx.root.join("real")), "the real directory is watched");
        assert!(
            !watched.contains(&fx.root.join("link")),
            "the symlink is not watched (not followed)"
        );
        std::fs::remove_dir_all(&outside).ok();
    }

    #[test]
    fn test_unreadable_directory_does_not_abort_the_walk() {
        use std::os::unix::fs::PermissionsExt;
        let mut fx = Fixture::new(true);
        std::fs::create_dir_all(fx.root.join("ok")).unwrap();
        let locked = fx.root.join("locked");
        std::fs::create_dir_all(locked.join("inner")).unwrap();
        let mut perm = std::fs::metadata(&locked).unwrap().permissions();
        perm.set_mode(0o000);
        std::fs::set_permissions(&locked, perm).unwrap();

        // Must not panic; the readable sibling is still returned even though the
        // locked directory cannot be descended into.
        let watched = fx.watched();
        assert!(watched.contains(&fx.root.join("ok")), "the readable sibling is watched");
        assert!(
            !watched.contains(&locked.join("inner")),
            "the unreadable directory's contents are not reached"
        );
    }

    #[test]
    fn test_internal_dir_is_never_watched() {
        let mut fx = Fixture::new(true);
        std::fs::create_dir_all(fx.internal_dir().join("sub")).unwrap();
        std::fs::create_dir_all(fx.root.join("data")).unwrap();

        let watched = fx.watched();
        assert!(watched.contains(&fx.root.join("data")));
        assert!(
            !watched.contains(&fx.internal_dir()),
            ".metafolder/internal/ is always excluded from watching"
        );
    }

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
