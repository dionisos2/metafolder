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

use metafolder_core::metarecord::TreeName;
use metafolder_core::sync::MutexExt;

use crate::db;
use crate::eligibility::{self, EligibilityCache};
use crate::executor::{self, ExecutorPinger, FsEvent};
use crate::relpath::RelPath;
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
    /// Adds a non-recursive watch on `dir` (idempotent). Returns whether the
    /// directory is now watched. Failures are swallowed: one unreadable/racing
    /// directory must never abort watching the rest (spec-file-tracking "File
    /// Watcher").
    ///
    /// `quiet` suppresses the per-directory warning, for the bulk placement
    /// that reports its failures as one summary instead.
    fn watch_dir_reporting(&self, dir: &Path, quiet: bool) -> Result<(), notify::Error> {
        let mut watched = self.watched.lock_recover();
        if !watched.insert(dir.to_path_buf()) {
            return Ok(()); // Already watched.
        }
        if let Some(w) = self.watcher.lock_recover().as_mut() {
            if let Err(err) = w.watch(dir, notify::RecursiveMode::NonRecursive) {
                if !quiet {
                    crate::diagnostics::warn("watcher", format!("failed to watch {dir:?}: {err}"));
                }
                watched.remove(dir);
                return Err(err);
            }
        }
        Ok(())
    }

    fn watch_dir(&self, dir: &Path) {
        let _ = self.watch_dir_reporting(dir, false);
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
    /// Brings the watch set to `target`, returning how many directories could
    /// *not* be watched because the kernel's per-user budget is exhausted.
    ///
    /// Those failures are counted rather than each warned about: exhausting the
    /// budget fails for every remaining directory, and the diagnostics ring
    /// holds 500 entries — the message that matters would be evicted by its own
    /// repetitions. The caller reports the total once (`budget_report`).
    fn apply(&self, target: &HashSet<PathBuf>) -> usize {
        let current: Vec<PathBuf> = self.watched.lock_recover().iter().cloned().collect();
        for dir in &current {
            if !target.contains(dir) {
                self.unwatch_dir(dir);
            }
        }
        let mut exhausted = 0;
        for dir in target {
            if let Err(err) = self.watch_dir_reporting(dir, true) {
                if is_watch_budget_exhausted(&err) {
                    exhausted += 1;
                } else {
                    crate::diagnostics::warn("watcher", format!("failed to watch {dir:?}: {err}"));
                }
            }
        }
        exhausted
    }

    /// How many directories this watcher currently holds a watch on.
    fn watched_count(&self) -> usize {
        self.watched.lock_recover().len()
    }
}

/// The kernel's per-user watch limit (`fs.inotify.max_user_watches`), or `None`
/// where it cannot be read — every backend that is not inotify, and a Linux
/// without `/proc`. Read once per placement; it does not change under us in any
/// way worth polling for.
pub fn kernel_watch_limit() -> Option<usize> {
    std::fs::read_to_string("/proc/sys/fs/inotify/max_user_watches").ok()?.trim().parse().ok()
}

/// How many watches this daemon will spend: `share` percent of the kernel's
/// limit (spec-file-tracking "The watch budget").
///
/// A ceiling it imposes on itself, not a reservation: the kernel exposes the
/// limit, never what is still available, and nothing can be reserved. The floor
/// of one keeps a `share` of zero from producing a repository that watches
/// literally nothing, which reads as a broken daemon rather than as a setting.
fn budget_cap(limit: Option<usize>, share: u8) -> Option<usize> {
    limit.map(|limit| (limit.saturating_mul(share as usize) / 100).max(1))
}

/// The cap for a given share, read from the kernel now.
pub fn budget_cap_for(share: u8) -> Option<usize> {
    budget_cap(kernel_watch_limit(), share)
}

/// Whether a watch failure is the kernel's per-user watch budget being
/// exhausted (`ENOSPC` from `inotify_add_watch`).
///
/// inotify holds one watch per *directory* — there is no recursive watch, so
/// this is the model and not a choice: a tree of N directories costs N watches,
/// and the budget (`fs.inotify.max_user_watches`) is shared with every other
/// program on the machine that watches files.
fn is_watch_budget_exhausted(err: &notify::Error) -> bool {
    const ENOSPC: i32 = 28;
    match &err.kind {
        notify::ErrorKind::Io(io) => io.raw_os_error() == Some(ENOSPC),
        _ => false,
    }
}

/// The single line reporting an exhausted watch budget, or `None` when every
/// directory was watched. Names the limit, so the reader has the remedy and not
/// just the symptom.
fn budget_report(unwatched: usize, watched: usize) -> Option<String> {
    (unwatched > 0).then(|| {
        format!(
            "out of inotify watches: {unwatched} directory(ies) are NOT being watched \
             ({watched} are). Changes under them go unnoticed until a reconcile. \
             Raise fs.inotify.max_user_watches (it is a per-user budget, shared with \
             every other program watching files)"
        )
    })
}

/// The most events the ingest thread folds into a single hand-over. Large
/// enough that a mass arrival costs a handful of locks rather than one per
/// event, small enough that the executor still sees the first events of a long
/// stream without waiting for it to end.
const MAX_INGEST_BATCH: usize = 4096;

/// What one placement achieved (spec-file-tracking "The watch budget").
pub struct Placement {
    /// Directories now watched.
    pub watched: usize,
    /// Directories the *kernel* refused although the daemon was under its own
    /// ceiling — someone else holds the budget. Nothing is recorded for these.
    pub starved: usize,
    /// Subtree roots the daemon's own ceiling could not afford, to record as
    /// `mfr_watch_exceeded`.
    pub frontier: Vec<String>,
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
    /// How many directories are currently watched — one inotify watch each
    /// (see [`is_watch_budget_exhausted`]).
    pub fn watched(&self) -> usize {
        self.inner.watched_count()
    }

    /// Brings the watch set in line with the repository's eligibility, within
    /// the budget `cap` (`None` = uncapped).
    pub fn refresh(
        &self,
        conn: &Connection,
        cache: &mut TreeCache,
        root: &Path,
        internal_dir: &Path,
        cap: Option<usize>,
    ) -> Placement {
        let plan = compute_watched_dirs_timed(conn, cache, root, internal_dir, cap);
        // A persistent diagnostic for the initial (load-time) walk and any large
        // watch reconfiguration: the filesystem read_dir cost vs the per-directory
        // eligibility cost (served from the tree cache once warm).
        if plan.total.as_millis() >= 100 {
            eprintln!(
                "[watcher] walk: {} dirs in {:?} (fs {:?} + eligibility {:?})",
                plan.dirs.len(),
                plan.total,
                plan.total.saturating_sub(plan.eligibility),
                plan.eligibility,
            );
        }
        let starved = self.inner.apply(&plan.dirs);
        let watched = self.inner.watched_count();
        // Refused by the kernel while under our own ceiling: another program is
        // holding the budget. Transient and external — reported, never recorded
        // (spec-file-tracking "Two different failures").
        if let Some(report) = budget_report(starved, watched) {
            crate::diagnostics::error("watcher", report);
        }
        Placement { watched, starved, frontier: plan.frontier }
    }
}

pub fn start(repo: &Arc<RepoState>, pinger: ExecutorPinger) -> Result<WatcherHandle> {
    let root = repo.config.root.clone();
    let internal_dir = repo.internal_dir();

    let inner =
        Arc::new(WatcherInner { watcher: Mutex::new(None), watched: Mutex::new(HashSet::new()) });

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
            // Take everything already queued behind this delivery: notify hands
            // events over a few at a time, and each delivery costs a lock and a
            // ping. Coalescing them turns a mass arrival into one of each per
            // batch. Capped so a continuous stream still reaches the executor
            // promptly instead of growing one unbounded batch.
            let mut events = events;
            while events.len() < MAX_INGEST_BATCH {
                match rx.try_recv() {
                    Ok(more) => events.extend(more),
                    Err(_) => break,
                }
            }
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
            Err(err) => crate::diagnostics::error("watcher", format!("backend error: {err}")),
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
/// What a placement decided: the directories to watch, and the subtree roots the
/// budget could not afford (spec-file-tracking "The watch budget").
pub struct WatchPlan {
    pub dirs: HashSet<PathBuf>,
    /// Repo-root-relative paths to record as `mfr_watch_exceeded`. Only the
    /// *frontier* — the subtrees the walk did not enter — never every directory
    /// beneath: a repository too large to watch is also too large to annotate
    /// one metarecord at a time.
    pub frontier: Vec<String>,
    pub total: std::time::Duration,
    pub eligibility: std::time::Duration,
}

pub fn compute_watched_dirs_timed(
    conn: &Connection,
    cache: &mut TreeCache,
    root: &Path,
    internal_dir: &Path,
    cap: Option<usize>,
) -> WatchPlan {
    let start = std::time::Instant::now();
    let mut elig = std::time::Duration::ZERO;
    let mut ec = EligibilityCache::default();
    let mut out = HashSet::new();
    let mut frontier = Vec::new();
    // Paths that override an inherited exclusion back to `false`. Few by
    // construction — each is a deliberate user choice — and knowing them lets
    // an excluded subtree be pruned instead of walked in search of an override
    // that is not there.
    let overrides = watch_exceeded_overrides(conn, cache);
    // The root directory is watched iff the root metarecord is eligible
    // (`mf_watch = true` set directly on it — the opt-in default is false).
    let t = std::time::Instant::now();
    let root_eligible = eligibility::is_eligible_cached(conn, cache, "", &mut ec);
    elig += t.elapsed();
    macro_rules! done {
        () => {
            return WatchPlan { dirs: out, frontier, total: start.elapsed(), eligibility: elig }
        };
    }
    match root_eligible {
        Ok(true) => {
            out.insert(root.to_path_buf());
        }
        Ok(false) => done!(),
        Err(err) => {
            crate::diagnostics::warn("watcher", format!("root eligibility check failed: {err:#}"));
            done!()
        }
    }
    // Declared mount points with nothing mounted: no watch on them, none below
    // (spec-file-tracking "Offline subtrees"). Recomputed on every refresh, so
    // a remounted volume is watched again without restarting the daemon.
    let offline = crate::mount::offline(conn, cache, root).unwrap_or_default();
    let mut walk = Walk {
        internal_dir,
        offline: &offline,
        overrides: &overrides,
        cap,
        out: &mut out,
        frontier: &mut frontier,
        elig: &mut elig,
    };
    collect_eligible_dirs(conn, cache, root, &RelPath::root(), &mut ec, &mut walk);
    done!()
}

/// The paths carrying `mfr_watch_exceeded = false` — the deliberate overrides
/// inside an excluded subtree.
fn watch_exceeded_overrides(conn: &Connection, cache: &mut TreeCache) -> HashSet<String> {
    let mut out = HashSet::new();
    let uuids = match db::metarecords_with_bool(conn, eligibility::WATCH_EXCEEDED, false) {
        Ok(uuids) => uuids,
        Err(err) => {
            crate::diagnostics::warn("watcher", format!("reading watch overrides failed: {err:#}"));
            return out;
        }
    };
    for uuid in uuids {
        if let Ok(Some(path)) = cache.path_of(conn, "mfr_path", uuid) {
            out.insert(path);
        }
    }
    out
}

/// The mutable state of one placement walk, kept together so the descent takes
/// an argument list a person can read.
struct Walk<'a> {
    internal_dir: &'a Path,
    offline: &'a crate::mount::OfflineMounts,
    overrides: &'a HashSet<String>,
    cap: Option<usize>,
    out: &'a mut HashSet<PathBuf>,
    frontier: &'a mut Vec<String>,
    elig: &'a mut std::time::Duration,
}

impl Walk<'_> {
    /// Whether the budget still allows another watch.
    fn has_room(&self) -> bool {
        self.cap.is_none_or(|cap| self.out.len() < cap)
    }

    /// Whether an override lies inside `dir`, so an excluded subtree has to be
    /// descended after all.
    fn holds_override(&self, dir: &str) -> bool {
        self.overrides.iter().any(|p| p == dir || p.starts_with(&format!("{dir}/")))
    }
}

/// Depth-first descent from the eligible directory `base` (repo-root-relative,
/// `""` for the root), inserting the absolute path of every eligible descendant
/// directory into `out`.
#[allow(clippy::too_many_arguments)]
fn collect_eligible_dirs(
    conn: &Connection,
    cache: &mut TreeCache,
    root: &Path,
    base: &RelPath,
    ec: &mut EligibilityCache,
    walk: &mut Walk,
) {
    // Each entry carries the exclusion inherited from its ancestors, so the
    // nearest-ancestor rule costs one field read per directory and not one
    // ancestor chain (spec-file-tracking "The watch budget").
    let mut stack = vec![(base.clone(), false)];
    while let Some((dir, inherited_excluded)) = stack.pop() {
        let abs = dir.to_abs(root);
        let entries = match std::fs::read_dir(&abs) {
            Ok(entries) => entries,
            Err(_) => continue, // Not a directory, or unreadable (EACCES): skip.
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path == *walk.internal_dir {
                continue;
            }
            // `file_type` comes from the dir entry (no stat, no symlink follow):
            // a symlinked directory reports `is_symlink()`/`!is_dir()`, so it is
            // never descended into or watched — the watch cannot escape the root.
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let rel = dir
                .child(TreeName::from_bytes(crate::relpath::file_name_bytes(&entry.file_name())));
            // Ignore patterns are regexes over text, so they see the displayed
            // path — the same one the user wrote them against.
            let display = rel.display();
            let t = std::time::Instant::now();
            let eligible = eligibility::is_eligible_cached(conn, cache, &display, ec);
            *walk.elig += t.elapsed();
            match eligible {
                Ok(true) if walk.offline.contains(&display) => {} // Unplugged volume: frozen.
                Ok(true) => {
                    let excluded = exclusion_of(conn, cache, ec, &display, inherited_excluded);
                    if excluded {
                        // Not watched. Descended only when a deliberate override
                        // is known to be inside, so giving up a subtree does not
                        // cost a walk of it at every load.
                        if walk.holds_override(&display) {
                            stack.push((rel, true));
                        }
                    } else if walk.has_room() {
                        walk.out.insert(path);
                        stack.push((rel, false));
                    } else {
                        // The budget is spent: this subtree is the frontier —
                        // recorded, and not entered.
                        walk.frontier.push(display);
                    }
                }
                Ok(false) => {} // Ineligible directory: pruned (cascading skip).
                Err(err) => crate::diagnostics::warn(
                    "watcher",
                    format!("eligibility check for {rel:?} failed: {err:#}"),
                ),
            }
        }
    }
}

/// The effective `mfr_watch_exceeded` of `display`: its own value when it has
/// one, else the value inherited from its ancestors.
fn exclusion_of(
    conn: &Connection,
    cache: &mut TreeCache,
    ec: &mut EligibilityCache,
    display: &str,
    inherited: bool,
) -> bool {
    let Ok(Some(uuid)) = cache.resolve_path(conn, "mfr_path", display) else {
        return inherited; // Not tracked yet: it can carry no value of its own.
    };
    match eligibility::cached_watch_exceeded(conn, ec, uuid) {
        Ok(Some(own)) => own,
        Ok(None) => inherited,
        Err(err) => {
            crate::diagnostics::warn(
                "watcher",
                format!("reading {} for {display:?} failed: {err:#}", eligibility::WATCH_EXCEEDED),
            );
            inherited
        }
    }
}

/// Converts an absolute path to the internal repo-root-relative form, keeping
/// each component's exact bytes — a POSIX name need not be UTF-8, and such a
/// file is watched like any other (spec-data-model "Tree names"). None for
/// paths outside the root, under `.metafolder/internal/`, or for the root.
fn relative(root: &Path, internal_dir: &Path, abs: &Path) -> Option<RelPath> {
    if abs.starts_with(internal_dir) {
        return None;
    }
    let rel = abs.strip_prefix(root).ok()?;
    let mut out = RelPath::root();
    for comp in rel.components() {
        let std::path::Component::Normal(name) = comp else {
            return None;
        };
        out = out.child(TreeName::from_bytes(crate::relpath::file_name_bytes(name)));
    }
    if out.is_root() {
        None // The root itself.
    } else {
        Some(out)
    }
}

/// Translates one notify event into the internal [`FsEvent`] forms. Pure: no
/// locks, no database, no watch calls — it runs on notify's event-loop thread.
fn translate(
    root: &Path,
    internal_dir: &Path,
    event: notify::Event,
) -> Vec<(FsEvent, Option<i64>)> {
    use notify::event::{ModifyKind, RenameMode};

    let rel = |p: &Path| relative(root, internal_dir, p);
    // The inotify rename cookie correlates a split From/To pair; carried so the
    // executor can fuse them back into one rename (see `correlate_renames`).
    let cookie = event.attrs.tracker().map(|c| c as i64);
    let mut events: Vec<(FsEvent, Option<i64>)> = Vec::new();
    match event.kind {
        notify::EventKind::Create(_) => {
            events.extend(
                event.paths.iter().filter_map(|p| rel(p)).map(|p| (FsEvent::Create(p), None)),
            );
        }
        notify::EventKind::Remove(_) => {
            events.extend(
                event.paths.iter().filter_map(|p| rel(p)).map(|p| (FsEvent::Remove(p), None)),
            );
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
            events.extend(
                event.paths.iter().filter_map(|p| rel(p)).map(|p| (FsEvent::RenameFrom(p), cookie)),
            );
        }
        notify::EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
            events.extend(
                event.paths.iter().filter_map(|p| rel(p)).map(|p| (FsEvent::RenameTo(p), cookie)),
            );
        }
        notify::EventKind::Modify(ModifyKind::Metadata(_)) => {
            events.extend(
                event.paths.iter().filter_map(|p| rel(p)).map(|p| (FsEvent::ModifyMeta(p), None)),
            );
        }
        // Data modifications; unknown Modify kinds fall back to Data
        // semantics (full refresh + hash invalidation, spec-platform).
        notify::EventKind::Modify(ModifyKind::Data(_))
        | notify::EventKind::Modify(ModifyKind::Any) => {
            events.extend(
                event.paths.iter().filter_map(|p| rel(p)).map(|p| (FsEvent::ModifyData(p), None)),
            );
        }
        _ => {}
    }

    events
}

/// Buffers a batch of translated events and keeps the live watch set in step.
/// Runs on the ingest thread — never on notify's event-loop thread.
fn ingest(
    repo: &RepoState,
    root: &Path,
    internal_dir: &Path,
    pinger: &ExecutorPinger,
    inner: Option<&WatcherInner>,
    events: Vec<(FsEvent, Option<i64>)>,
) {
    // Buffering is a push onto an in-memory vector: it cannot fail, and it does
    // not touch the repository's connection — so a mass arrival no longer
    // queues behind whatever holds it.
    executor::enqueue_all(repo, events.clone());
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
    let mut arrivals: Vec<&RelPath> = Vec::new();
    let mut departures: Vec<&RelPath> = Vec::new();
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
        inner.forget_subtree(&rel.to_abs(root));
    }
    if arrivals.is_empty() {
        return;
    }
    // Arrivals need eligibility checks (conn + cache). Collect the whole set
    // first and release both locks before placing any watch: `watch()` waits for
    // notify's event loop, which must never be made to wait for the connection
    // (see the module documentation).
    let mut subtree = HashSet::new();
    let mut frontier: Vec<String> = Vec::new();
    let cap = budget_cap_for(repo.watch_budget_share());
    {
        let conn = repo.conn.lock_recover();
        let mut cache = repo.lock_cache();
        let mut ec = EligibilityCache::default();
        let offline = crate::mount::offline(&conn, &mut cache, root).unwrap_or_default();
        let overrides = watch_exceeded_overrides(&conn, &mut cache);
        for rel in arrivals {
            let abs = rel.to_abs(root);
            // Only a real directory (not a symlink) that is eligible is watched.
            match std::fs::symlink_metadata(&abs) {
                Ok(md) if md.file_type().is_dir() => {}
                _ => continue,
            }
            let display = rel.display();
            match eligibility::is_eligible_cached(&conn, &mut cache, &display, &mut ec) {
                Ok(true) if !offline.contains(&display) => {}
                _ => continue,
            }
            subtree.insert(abs);
            let mut elig = std::time::Duration::ZERO;
            let mut walk = Walk {
                internal_dir,
                offline: &offline,
                overrides: &overrides,
                // The ceiling counts the *whole* repository, not this subtree,
                // so what is already watched is charged against it.
                cap: cap.map(|cap| cap.saturating_sub(inner.watched_count())),
                out: &mut subtree,
                frontier: &mut frontier,
                elig: &mut elig,
            };
            collect_eligible_dirs(&conn, &mut cache, root, rel, &mut ec, &mut walk);
        }
    }
    for dir in &subtree {
        inner.watch_dir(dir);
    }
    // A directory that arrived into a full budget is recorded like any other
    // frontier, so the choice survives the session (spec-file-tracking "The
    // watch budget").
    if !frontier.is_empty() {
        repo.record_watch_frontier(&frontier);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        budget_cap, budget_report, compute_watched_dirs_timed, is_watch_budget_exhausted, relative,
    };
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
            let root = std::env::temp_dir()
                .join("metafolder-tests")
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
            self.plan(None).dirs
        }

        fn plan(&mut self, cap: Option<usize>) -> super::WatchPlan {
            let internal = self.internal_dir();
            let root = self.root.clone();
            compute_watched_dirs_timed(&self.conn, &mut self.cache, &root, &internal, cap)
        }

        /// Gives `rel` (repo-root-relative, leading `/`) a metarecord carrying
        /// `mfr_watch_exceeded = value`, as the daemon or the user would.
        fn mark_exceeded(&mut self, rel: &str, value: bool) {
            let parent_rel = rel.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
            let parent = self
                .cache
                .resolve_path(&self.conn, "mfr_path", parent_rel)
                .unwrap()
                .expect("parent tracked");
            let name = rel.rsplit('/').next().unwrap().to_string();
            let mut w = Writer::begin(&mut self.conn, None).unwrap();
            let created = w
                .create_metarecord(vec![
                    Field::new(
                        "mfr_path",
                        Value::TreeRef { parent: Some(parent), name: name.as_str().into() },
                    ),
                    Field::new("mfr_type", Value::String("dir".into())),
                ])
                .unwrap();
            w.set_field(created.uuid, super::eligibility::WATCH_EXCEEDED, Value::Bool(value))
                .unwrap();
            w.commit().unwrap();
            self.cache.clear();
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
        let outside = std::env::temp_dir()
            .join("metafolder-tests")
            .join(format!("metafolder_out_{}", uuid::Uuid::new_v4()));
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
        let rel = |p: &str| relative(root, internal, Path::new(p)).map(|r| r.display());

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
        let rel = |p: &str| relative(root, internal, Path::new(p)).map(|r| r.display());

        assert_eq!(rel("/etc/hosts").as_deref(), Some("/etc/hosts"));
        assert_eq!(
            rel("/home/.metafolder/config.json").as_deref(),
            Some("/home/.metafolder/config.json")
        );
        assert_eq!(rel("/home/.metafolder/internal/db.sqlite"), None);
    }

    // ── The kernel's watch budget ─────────────────────────────────────────────

    fn enospc() -> notify::Error {
        notify::Error::io(std::io::Error::from_raw_os_error(28))
    }

    #[test]
    fn test_running_out_of_watches_is_recognised_for_what_it_is() {
        // inotify holds one watch per *directory* and the budget is per user,
        // shared with every other program that watches files. Exhausting it is
        // not "a directory could not be watched": it is the whole rest of the
        // tree going unwatched, and it has a name and a remedy.
        assert!(is_watch_budget_exhausted(&enospc()));
        assert!(!is_watch_budget_exhausted(&notify::Error::io(
            std::io::Error::from_raw_os_error(13) // EACCES: one unreadable directory
        )));
    }

    #[test]
    fn test_the_budget_report_names_the_limit_and_what_it_cost() {
        // One line, not one per directory: a placement that fails at scale
        // fails thousands of times, and the diagnostics ring holds 500 entries
        // — the message that matters would be evicted by its own repetitions.
        let report = budget_report(18_000, 6_000).expect("exhaustion must be reported");
        assert!(report.contains("18000"), "how many directories went unwatched: {report}");
        assert!(report.contains("6000"), "how many are watched: {report}");
        assert!(
            report.contains("fs.inotify.max_user_watches"),
            "the remedy must be named: {report}"
        );
    }

    #[test]
    fn test_no_report_when_every_directory_was_watched() {
        assert_eq!(budget_report(0, 6_000), None);
    }

    #[test]
    fn test_the_share_is_a_percentage_of_the_kernel_limit() {
        // The daemon spends a share of `fs.inotify.max_user_watches` and leaves
        // the rest to other programs — a ceiling it imposes on itself, since
        // there is no way to reserve anything.
        assert_eq!(budget_cap(Some(524_288), 50), Some(262_144));
        assert_eq!(budget_cap(Some(8_192), 50), Some(4_096));
        assert_eq!(budget_cap(Some(1_000), 100), Some(1_000));
    }

    #[test]
    fn test_a_share_of_zero_still_leaves_room_to_watch_the_root() {
        // 0% is the reading of "spend nothing", but a repository that watches
        // literally nothing is indistinguishable from a broken one. One watch
        // is the floor.
        assert_eq!(budget_cap(Some(524_288), 0), Some(1));
    }

    #[test]
    fn test_no_limit_means_no_cap() {
        // Where the limit cannot be read — every backend that is not inotify,
        // and a Linux without /proc — nothing is capped.
        assert_eq!(budget_cap(None, 50), None);
    }

    // ── The watch budget ──────────────────────────────────────────────────────

    /// Creates `count` directories directly under the root, named so the walk
    /// order is stable enough to reason about.
    fn dirs(f: &Fixture, count: usize) {
        for i in 0..count {
            std::fs::create_dir_all(f.root.join(format!("d{i:03}"))).unwrap();
        }
    }

    #[test]
    fn test_the_cap_bounds_the_watches_and_names_what_it_dropped() {
        // One inotify watch per directory, and the daemon spends only its share
        // of the kernel's budget. What it cannot afford is not silently missing:
        // it comes back as a frontier to record, so the choice survives the
        // session.
        let f = Fixture::new(true);
        dirs(&f, 10);
        let mut f = f;

        let plan = f.plan(Some(4));
        assert_eq!(plan.dirs.len(), 4, "the cap is the cap (root included)");
        assert_eq!(plan.frontier.len(), 7, "every directory it could not take is named");
        assert!(plan.dirs.contains(&f.root), "the root is watched first");
    }

    #[test]
    fn test_no_cap_watches_everything() {
        let f = Fixture::new(true);
        dirs(&f, 10);
        let mut f = f;
        let plan = f.plan(None);
        assert_eq!(plan.dirs.len(), 11, "root + 10");
        assert!(plan.frontier.is_empty());
    }

    #[test]
    fn test_an_excluded_directory_and_its_subtree_go_unwatched() {
        // The field is inherited like `mf_watch`: excluding a directory excludes
        // what is under it, which is what lets a user free a whole folder's
        // worth of watches in one write.
        let f = Fixture::new(true);
        std::fs::create_dir_all(f.root.join("big/inner/deep")).unwrap();
        std::fs::create_dir_all(f.root.join("small")).unwrap();
        let mut f = f;
        f.mark_exceeded("/big", true);

        let watched = f.watched();
        assert!(watched.contains(&f.root.join("small")), "the rest is untouched");
        assert!(!watched.contains(&f.root.join("big")), "the excluded directory");
        assert!(!watched.contains(&f.root.join("big/inner")), "and everything under it");
        assert!(!watched.contains(&f.root.join("big/inner/deep")));
    }

    #[test]
    fn test_an_override_below_an_excluded_subtree_is_watched_again() {
        // Nearest ancestor decides, a direct value overrides — the same rule as
        // `mf_watch`. This is how a user keeps one directory live inside a
        // subtree they have otherwise given up on.
        let f = Fixture::new(true);
        std::fs::create_dir_all(f.root.join("big/inner/deep")).unwrap();
        let mut f = f;
        f.mark_exceeded("/big", true);
        f.mark_exceeded("/big/inner", false);

        let watched = f.watched();
        assert!(!watched.contains(&f.root.join("big")), "still excluded");
        assert!(watched.contains(&f.root.join("big/inner")), "the override is watched");
        assert!(
            watched.contains(&f.root.join("big/inner/deep")),
            "and inheritance resumes below it"
        );
    }
}
