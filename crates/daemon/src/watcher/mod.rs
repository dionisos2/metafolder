//! The filesystem watcher (doc "Watcher"): translates the
//! source's events into [`crate::executor::FsEvent`]s, enqueues them in the
//! persistent buffer and pings the executor. Events under
//! `.metafolder/internal/` (the daemon's own database writes) are skipped;
//! names travel as their exact bytes (doc "Tree names").
//!
//! One pipeline, two *sources* behind it ([`Source`]; doc "Watch sources and regimes"): [`inotify`]
//! — the notify backend, one non-recursive
//! watch per eligible directory, the *budget* regime — and [`fanotify`], the
//! client of the fanotify broker covering the tree with one kernel
//! registration per mount, the *coverage* regime (doc "The fanotify broker").
//! Exactly one source is active per repository at a time; translation,
//! buffering, compaction and the executor are shared and do not care which
//! source produced an event.
//!
//! **The event callback must never block.** A source only hands events to the
//! ingest thread ([`start`]); everything that can touch the database or the
//! watch set runs there.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;

use metafolder_core::metarecord::TreeName;
use metafolder_core::sync::MutexExt;

use crate::eligibility::{self, WatchRules};
use crate::executor::{self, ExecutorPinger, FsEvent};
use crate::relpath::RelPath;
use crate::state::RepoState;
use crate::tree_cache::TreeCache;

pub mod fanotify;
pub mod inotify;

pub use inotify::{budget_cap_for, kernel_watch_limit};

/// How the kernel covers a tree (doc "Watch sources and regimes"): the *budget* regime holds one
/// watch per directory and can run out
/// of them, the *coverage* regime has one registration for the whole tree and
/// no budget at all. This decides whether the budget-only fields of `GET /watch`
/// (=watched_dirs=, =watch_budget=) carry anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Regime {
    Budget,
    Coverage,
}

/// What covers paths, as `POST /watch/check` answers against it
/// (doc "Checking whether a path is watched"): the live watch set in the budget
/// regime, the tree itself under coverage — where the only ways a path is
/// *not* covered are the structural skips the reason ladder reports anyway.
#[derive(Debug, Clone, Copy)]
pub enum Coverage<'a> {
    /// The budget regime: the directories holding a watch right now.
    Watches(&'a HashSet<PathBuf>),
    /// The coverage regime: one kernel registration reaches the whole tree.
    Tree,
}

/// What a watch source does to sit behind [`WatcherHandle`] (doc "Watch sources and regimes"):
/// keep live coverage in step with the
/// repository's eligibility, report what is covered, and react to the arrivals
/// and departures in the event stream. Everything downstream — translation,
/// buffering, compaction, the executor — is shared; only the way the kernel
/// covers the tree differs. The conformance battery in `tests/` runs against
/// every source.
pub(crate) trait Source: Send + Sync {
    /// The wire name of this source (`GET /watch` `backend`).
    fn name(&self) -> &'static str;
    /// Whether the watch-budget vocabulary applies to this source.
    fn regime(&self) -> Regime;
    /// Brings live coverage in line with the repository's eligibility within
    /// the budget `cap` (`None` = uncapped), returning what the placement
    /// achieved ([`Placement`]).
    fn refresh(
        &self,
        conn: &dyn crate::store::Store,
        cache: &TreeCache,
        root: &Path,
        internal_dir: &Path,
        cap: Option<usize>,
    ) -> Placement;
    /// How many directories are covered right now.
    fn watched(&self) -> usize;
    /// A snapshot of the absolute paths currently covered — what
    /// `POST /repos/:repo/watch/check` answers against.
    fn watched_set(&self) -> HashSet<PathBuf>;
    /// Keeps coverage in step with the directory arrivals and departures in
    /// `events`.
    fn maintain(
        &self,
        repo: &RepoState,
        root: &Path,
        internal_dir: &Path,
        events: &[(FsEvent, Option<i64>)],
    );
}

/// The most events the ingest thread folds into a single hand-over. Large
/// enough that a mass arrival costs a handful of locks rather than one per
/// event, small enough that the executor still sees the first events of a long
/// stream without waiting for it to end.
const MAX_INGEST_BATCH: usize = 4096;

/// What one placement achieved (doc "The watch budget").
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
    // Dropping the last strong `Arc` drops the source (stopping event
    // delivery). The event callback holds only a `Weak`, so it is not a cycle.
    source: Arc<dyn Source>,
    /// Why the fanotify source was not taken, when it was not: `GET /watch`
    /// `backend_reason`, so `mf watch status` can say it where the user looks.
    fallback: Option<String>,
}

impl WatcherHandle {
    /// Why this repository is not on the fanotify broker (no broker at the
    /// socket, the root refused…); `None` when it is.
    pub fn backend_reason(&self) -> Option<&str> {
        self.fallback.as_deref()
    }

    /// What the active source is called on the wire (`GET /watch` `backend`):
    /// "inotify", "fanotify", … (doc "Watch sources and regimes").
    pub fn backend(&self) -> &'static str {
        self.source.name()
    }

    /// Whether the watch-budget vocabulary applies to the active source.
    pub fn regime(&self) -> Regime {
        self.source.regime()
    }

    /// How many directories are currently watched — one inotify watch each in
    /// the budget regime (see [`inotify::is_watch_budget_exhausted`]).
    pub fn watched(&self) -> usize {
        self.source.watched()
    }

    /// A snapshot of the absolute paths currently watched — what
    /// `POST /repos/:repo/watch/check` answers against (the live set, not the
    /// placement's target: a starved directory is absent).
    pub fn watched_set(&self) -> HashSet<PathBuf> {
        self.source.watched_set()
    }

    /// Brings the watch set in line with the repository's eligibility, within
    /// the budget `cap` (`None` = uncapped). Called after a manual write
    /// changes `mf_watch`/`mf_ignore`. Takes the already-locked connection to
    /// avoid re-locking it.
    pub fn refresh(
        &self,
        conn: &dyn crate::store::Store,
        cache: &TreeCache,
        root: &Path,
        internal_dir: &Path,
        cap: Option<usize>,
    ) -> Placement {
        self.source.refresh(conn, cache, root, internal_dir, cap)
    }
}

pub fn start(repo: &Arc<RepoState>, pinger: ExecutorPinger) -> Result<WatcherHandle> {
    let root = repo.config.root.clone();
    let internal_dir = repo.internal_dir();

    // The ingest thread does everything that can block — the database enqueue
    // and the coverage maintenance for new directories. It ends when the sender
    // dies with the source (repository unloaded).
    let (tx, rx) = std::sync::mpsc::channel::<Vec<(FsEvent, Option<i64>)>>();
    let (source, fallback) = open_source(repo, tx)?;

    // Weaks: neither the ingest thread nor the callback may keep the repository
    // (and its exclusive lock) or the source alive.
    let repo_weak = Arc::downgrade(repo);
    let source_weak = Arc::downgrade(&source);

    let ingest_root = root.clone();
    let ingest_internal = internal_dir.clone();
    std::thread::spawn(move || {
        while let Ok(events) = rx.recv() {
            // Take everything already queued behind this delivery: the source
            // hands events over a few at a time, and each delivery costs a lock
            // and a ping. Coalescing them turns a mass arrival into one of each
            // per batch. Capped so a continuous stream still reaches the
            // executor promptly instead of growing one unbounded batch.
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
            let source = source_weak.upgrade();
            ingest(&repo, &ingest_root, &ingest_internal, &pinger, source.as_deref(), events);
        }
    });

    // No initial placement here: the eligible-directory walk needs the tree
    // cache, so it is deferred to the end of the load warmup (which populates the
    // cache) via `RepoState::refresh_watches` — there each directory's
    // eligibility is served from memory instead of a per-directory DB walk. Until
    // then the watcher holds no watches (a fresh repo watches nothing anyway).
    Ok(WatcherHandle { source, fallback })
}

/// Opens the watch source (doc "Watch sources and regimes"):
/// the fanotify broker's stream when one answers at `[settings] watchd-socket`,
/// the notify source otherwise — announced, never silent. Probed once, at
/// load: a restart is what re-decides (a broker that comes up later is picked
/// up by the next load).
/// The source, and why the broker was not taken when it was not.
fn open_source(
    repo: &Arc<RepoState>,
    tx: std::sync::mpsc::Sender<Vec<(FsEvent, Option<i64>)>>,
) -> Result<(Arc<dyn Source>, Option<String>)> {
    let socket = repo.watchd_socket().to_path_buf();
    match fanotify::Source::start(repo, &socket, tx.clone()) {
        Ok(source) => Ok((source, None)),
        Err(err) => {
            let reason = format!("{err:#}");
            crate::diagnostics::warn_for(
                "watcher",
                format!(
                    "no fanotify broker at {}: {reason} — watching with the inotify source \
                     (one watch per directory)",
                    socket.display()
                ),
                repo.uuid(),
            );
            Ok((inotify::Source::start(repo, tx)?, Some(reason)))
        }
    }
}

// ── Watch check (doc "Checking whether a path is watched") ───────────────────────────

/// Why [`explain_watched`] decided the way it did — what stands between the
/// path and the watcher recording a change at it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchedReason {
    /// The path is tracked and the watch on its covering directory reports it.
    Watched,
    /// Not tracked at all: the eligibility algorithm said no (the path itself,
    /// or — for an entry — its containing directory).
    Untracked,
    /// Tracked, but inside a subtree carrying `mfr_watch_exceeded = true`: the
    /// budget's frontier, or a deliberate `mf watch exceeded set`.
    Excluded,
    /// Under a declared mount point with nothing mounted: frozen until the
    /// volume returns (doc "Offline subtrees").
    Offline,
    /// Inside the daemon's own runtime directory: never watched, whatever the
    /// eligibility says.
    Internal,
    /// Tracked, eligible, not excluded, not offline — yet the covering
    /// directory holds no watch right now: the kernel refused (starved).
    Unwatched,
}

impl WatchedReason {
    /// The wire form used by `POST /repos/:repo/watch/check`.
    pub fn as_str(self) -> &'static str {
        match self {
            WatchedReason::Watched => "watched",
            WatchedReason::Untracked => "untracked",
            WatchedReason::Excluded => "excluded",
            WatchedReason::Offline => "offline",
            WatchedReason::Internal => "internal",
            WatchedReason::Unwatched => "unwatched",
        }
    }
}

/// The watched state of one path, as `POST /repos/:repo/watch/check` reports
/// it. The eligibility explanations are carried along so a client can show
/// *why* without a second call.
#[derive(Debug, Clone)]
pub struct WatchedStatus {
    pub watched: bool,
    pub reason: WatchedReason,
    /// The directory whose watch covers the path (`""` is the repository
    /// root): itself for a directory, its containing directory for anything
    /// else. The watch set's truth is about directories — this is what the
    /// answer was computed against.
    pub watched_dir: String,
    /// The path's own eligibility dry run (what `POST /eligibility` answers).
    pub eligibility: eligibility::Explanation,
    /// The covering directory's own eligibility dry run — `eligibility` for a
    /// directory path, its containing directory's otherwise. A file whose name
    /// no pattern matches can still be unwatchable because its directory is
    /// pruned (cascading skip), and only this tells the two apart.
    pub dir_eligibility: eligibility::Explanation,
    /// The path of the metarecord carrying the `mfr_watch_exceeded = true`
    /// that excludes it ([`WatchedReason::Excluded`] only).
    pub excluded_by: Option<String>,
    /// The offline mount point the path sits under
    /// ([`WatchedReason::Offline`] only).
    pub offline_mount: Option<String>,
}

/// The watched state of a batch of repo-root-relative paths, answering
/// `POST /repos/:repo/watch/check`. `watched` is the *live watch set's* truth —
/// one step past eligibility: a change at a path is recorded when the path is
/// eligible AND the covering directory holds a watch, so a tracked file inside
/// an excluded subtree or on an unplugged volume is still not watched, and the
/// reason says which. Read-only; the rule index and one offline-mounts
/// snapshot serve the whole batch.
pub fn explain_watched(
    conn: &dyn crate::store::Store,
    cache: &TreeCache,
    rules: &WatchRules,
    root: &Path,
    internal_dir: &Path,
    coverage: Coverage<'_>,
    rel_paths: &[String],
) -> Result<Vec<WatchedStatus>> {
    let mut offline = None;
    rel_paths
        .iter()
        .map(|rel| {
            explain_watched_one(conn, cache, rules, root, internal_dir, coverage, rel, &mut offline)
        })
        .collect()
}

/// [`explain_watched`] for one path. The reason ladder follows the placement
/// walk's own order — eligibility first (the path, then its covering
/// directory), then the structural skips (the daemon's internals, an unplugged
/// volume, a recorded exclusion), and "starved" only when nothing else
/// explains the absence of a watch.
#[allow(clippy::too_many_arguments)]
fn explain_watched_one(
    conn: &dyn crate::store::Store,
    cache: &TreeCache,
    rules: &WatchRules,
    root: &Path,
    internal_dir: &Path,
    coverage: Coverage<'_>,
    rel_path: &str,
    offline: &mut Option<crate::mount::OfflineMounts>,
) -> Result<WatchedStatus> {
    let is_dir = dir_like(conn, cache, root, rel_path)?;
    let cover = if is_dir { rel_path.to_string() } else { parent_of(rel_path) };
    let covered = match coverage {
        Coverage::Watches(set) => set.contains(&abs_of(root, &cover)),
        Coverage::Tree => covered_by_tree(conn, cache, rules, root, internal_dir, &cover, offline),
    };
    let eligibility = rules.explain(&rules.rel_of_text(rel_path))?;
    let dir_eligibility =
        if is_dir { eligibility.clone() } else { rules.explain(&rules.rel_of_text(&cover))? };
    let watched = covered && eligibility.eligible;
    let (reason, excluded_by, offline_mount) = if watched {
        (WatchedReason::Watched, None, None)
    } else if !eligibility.eligible || !dir_eligibility.eligible {
        // Two cases, one verdict: the path itself is untracked (the common
        // one — mf_watch or a pattern decided), or only its covering
        // directory is (a pattern matched the directory alone, so the walk
        // pruned it and the file beneath can never be reached).
        (WatchedReason::Untracked, None, None)
    } else if abs_of(root, &cover).starts_with(internal_dir) {
        (WatchedReason::Internal, None, None)
    } else {
        let mounts = offline
            .get_or_insert_with(|| crate::mount::offline(conn, cache, root).unwrap_or_default());
        if let Some(mount) = mounts.paths().iter().find(|m| covers(m, &cover)) {
            (WatchedReason::Offline, None, Some((*mount).clone()))
        } else if let Some(by) = rules.exceeded_by(&rules.rel_of_text(&cover)) {
            (WatchedReason::Excluded, Some(by), None)
        } else {
            (WatchedReason::Unwatched, None, None)
        }
    };
    Ok(WatchedStatus {
        watched,
        reason,
        watched_dir: cover,
        eligibility,
        dir_eligibility,
        excluded_by,
        offline_mount,
    })
}

/// Whether the path denotes a directory: the disk first (symlink metadata —
/// a symlinked directory is never watched, matching the walk's
/// `file_type().is_dir()`), the metarecord's `mfr_type` when the path is gone
/// (an orphan's stale path still says what it was), else a file — a
/// not-yet-existing path is treated as the file that would appear there.
fn dir_like(
    conn: &dyn crate::store::Store,
    cache: &TreeCache,
    root: &Path,
    rel: &str,
) -> Result<bool> {
    if std::fs::symlink_metadata(abs_of(root, rel))
        .map(|md| md.file_type().is_dir())
        .unwrap_or(false)
    {
        return Ok(true);
    }
    match cache.resolve_path(conn, "mfr_path", rel)? {
        Some(uuid) => {
            Ok(conn.string_fields(uuid, "mfr_type")?.first().map(|t| t == "dir").unwrap_or(false))
        }
        None => Ok(false),
    }
}

/// The parent directory of a repo-root-relative path (`""` for the root and
/// for a top-level entry — the root's own parent).
fn parent_of(rel: &str) -> String {
    match rel.rfind('/') {
        Some(0) | None => String::new(),
        Some(i) => rel[..i].to_string(),
    }
}

/// The absolute path of a repo-root-relative `rel` (`""` is the root itself).
fn abs_of(root: &Path, rel: &str) -> PathBuf {
    if rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel.trim_start_matches('/'))
    }
}

/// Whether the offline mount point `m` is `rel` or an ancestor of it.
fn covers(m: &str, rel: &str) -> bool {
    rel == m || (rel.len() > m.len() && rel.starts_with(m) && rel.as_bytes()[m.len()] == b'/')
}

/// What the coverage regime knows about a covering directory: the kernel's
/// registration reaches the whole tree, so the only ways a directory is *not*
/// covered are the structural skips the reason ladder reports anyway — the
/// daemon's own internals, an unplugged volume, a recorded exclusion
/// (doc "Watch sources and regimes").
fn covered_by_tree(
    conn: &dyn crate::store::Store,
    cache: &TreeCache,
    rules: &WatchRules,
    root: &Path,
    internal_dir: &Path,
    cover: &str,
    offline: &mut Option<crate::mount::OfflineMounts>,
) -> bool {
    if abs_of(root, cover).starts_with(internal_dir) {
        return false;
    }
    let mounts =
        offline.get_or_insert_with(|| crate::mount::offline(conn, cache, root).unwrap_or_default());
    if mounts.paths().iter().any(|m| covers(m, cover)) {
        return false;
    }
    rules.exceeded_by(&rules.rel_of_text(cover)).is_none()
}

/// Converts an absolute path to the internal repo-root-relative form, keeping
/// each component's exact bytes — a POSIX name need not be UTF-8, and such a
/// file is watched like any other (doc "Tree names"). None for
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
        // semantics (full refresh + hash invalidation).
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
    source: Option<&dyn Source>,
    events: Vec<(FsEvent, Option<i64>)>,
) {
    // Counted as delivered, before any filter: what the kernel sends is the
    // load, whether or not it is recorded (doc "Watch activity").
    repo.watch_activity.lock_recover().record(&events);
    let Some((rules, trusted)) = ingest_rules(repo) else {
        // The rules could not be read: buffer everything, the flush decides.
        executor::enqueue_all(repo, events.clone());
        pinger.ping();
        if let Some(source) = source {
            source.maintain(repo, root, internal_dir, &events);
        }
        return;
    };
    // A batch that moves a rule is not filtered, and turns filtering off until
    // a flush has applied it (doc "Event batching"). Counted *after* buffering, so that a flush
    // that reads the count
    // before draining has the batch.
    let touching = touches_rules(&rules, &events);
    // The coverage regime honours `mfr_watch_exceeded` by *dropping* what
    // happens under it; the budget regime honours it by never seeing it (no
    // watch is placed there). A move across the boundary keeps only its
    // visible side — exactly what a move out of the watched tree looks like
    // (doc "Watch sources and regimes").
    let events = match source.map(Source::regime) {
        Some(Regime::Coverage) => drop_excluded(&rules, &events),
        _ => events,
    };
    // What the flush would turn away anyway (doc "Event batching"): dropped here, it never keeps
    // the quiet period open nor makes
    // a flush.
    let events = if trusted && !touching { drop_ineligible(&rules, events) } else { events };
    if events.is_empty() {
        return;
    }
    // Buffering is a push onto an in-memory vector: it cannot fail, and it does
    // not touch the repository's connection — so a mass arrival no longer
    // queues behind whatever holds it.
    executor::enqueue_all(repo, events.clone());
    if touching {
        repo.rules_touched();
    }
    pinger.ping();

    // Keep the live watch set in step with directories that appeared or vanished
    // (recursive watching is re-implemented here per-directory, so unlike
    // notify's own recursive mode this must be done by hand). The executor's
    // `scan_dir` ingests any content already inside a new directory; here we only
    // register the inotify watches for its *future* events.
    if let Some(source) = source {
        source.maintain(repo, root, internal_dir, &events);
    }
}

/// The rule index the ingest thread filters with, and whether it may drop
/// ineligible events ([`RepoState::ingest_rules`]). Read once from the store
/// when nothing has published it yet — only before the first flush or write of
/// the repository. `None` when that read fails.
fn ingest_rules(repo: &RepoState) -> Option<(Arc<WatchRules>, bool)> {
    let (rules, trusted) = repo.ingest_rules();
    if let Some(rules) = rules {
        return Some((rules, trusted));
    }
    let conn = repo.conn.lock_recover();
    match repo.watch_rules(&conn) {
        Ok(rules) => Some((rules, trusted)),
        Err(err) => {
            crate::diagnostics::warn_for(
                "watcher",
                format!("could not read the watch rules: {err:#}"),
                repo.uuid(),
            );
            None
        }
    }
}

/// Whether one of `events` could move a rule: a creation, removal or rename
/// at a path that holds a rule or is an ancestor of one. A modification moves
/// nothing.
fn touches_rules(rules: &WatchRules, events: &[(FsEvent, Option<i64>)]) -> bool {
    events.iter().any(|(ev, _)| match ev {
        FsEvent::Create(p) | FsEvent::Remove(p) | FsEvent::RenameFrom(p) | FsEvent::RenameTo(p) => {
            rules.touches(p)
        }
        FsEvent::Rename(a, b) => rules.touches(a) || rules.touches(b),
        FsEvent::ModifyData(_) | FsEvent::ModifyMeta(_) => false,
    })
}

/// Drops the events the flush would turn away as ineligible
/// (doc "Event batching"), and only those whose dropping
/// cannot change what the flush does: a `Rename` with an eligible side is kept
/// whole (the flush chooses between a move and a stale path), and a one-sided
/// `Rename(From)` / `Rename(To)` is always kept — dropping one half would break
/// the rename correlation. A path whose rules cannot be evaluated (a pattern
/// that does not compile) is kept for the flush to report.
fn drop_ineligible(
    rules: &WatchRules,
    events: Vec<(FsEvent, Option<i64>)>,
) -> Vec<(FsEvent, Option<i64>)> {
    let eligible = |p: &RelPath| rules.is_eligible(p).unwrap_or(true);
    events
        .into_iter()
        .filter(|(ev, _)| match ev {
            FsEvent::Create(p)
            | FsEvent::Remove(p)
            | FsEvent::ModifyData(p)
            | FsEvent::ModifyMeta(p) => eligible(p),
            FsEvent::Rename(a, b) => eligible(a) || eligible(b),
            FsEvent::RenameFrom(_) | FsEvent::RenameTo(_) => true,
        })
        .collect()
}

/// The coverage-regime filter for `mfr_watch_exceeded`: events under an
/// excluded subtree are dropped, and a move across the boundary keeps only its
/// visible side.
fn drop_excluded(
    rules: &WatchRules,
    events: &[(FsEvent, Option<i64>)],
) -> Vec<(FsEvent, Option<i64>)> {
    let is_excluded = |p: &RelPath| rules.exceeded_by(p).is_some();
    let mut out = Vec::with_capacity(events.len());
    for (ev, tracker) in events {
        let kept = match ev {
            FsEvent::Create(p)
            | FsEvent::Remove(p)
            | FsEvent::RenameFrom(p)
            | FsEvent::RenameTo(p)
            | FsEvent::ModifyData(p)
            | FsEvent::ModifyMeta(p) => (!is_excluded(p)).then(|| (ev.clone(), *tracker)),
            FsEvent::Rename(a, b) => {
                match (is_excluded(a), is_excluded(b)) {
                    (false, false) => Some((FsEvent::Rename(a.clone(), b.clone()), *tracker)),
                    // The far side is in the excluded dark: what the daemon
                    // sees is an arrival / a departure, never a move.
                    (true, false) => Some((FsEvent::RenameTo(b.clone()), None)),
                    (false, true) => Some((FsEvent::RenameFrom(a.clone()), None)),
                    (true, true) => None,
                }
            }
        };
        out.extend(kept);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        drop_excluded, drop_ineligible, explain_watched, relative, touches_rules, Coverage,
        FsEvent, WatchedReason, WatchedStatus,
    };
    use crate::eligibility::WatchRules;
    use crate::log::Writer;
    use crate::tree_cache::TreeCache;
    use metafolder_core::metarecord::{Field, Value};
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};

    /// An in-memory repository whose root metarecord carries `mf_watch = true`,
    /// backed by a real temporary directory — the shape `explain_watched` and
    /// `drop_excluded` need (eligibility from the store, names on disk).
    struct Fixture {
        conn: crate::kvstore::KvStore,
        _store_dir: crate::kvstore::TestDir,
        cache: TreeCache,
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir()
                .join("metafolder-tests")
                .join(format!("metafolder_cov_{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(root.join("dir")).unwrap();
            std::fs::create_dir_all(root.join("other")).unwrap();
            let (mut conn, store_dir) = crate::kvstore::test_store();
            let mut w = Writer::begin(&mut conn, None).unwrap();
            w.create_metarecord(vec![
                Field::new("mfr_path", Value::TreeRef { parent: None, name: "".into() }),
                Field::new("mf_watch", Value::Bool(true)),
            ])
            .unwrap();
            w.commit().unwrap();
            Self { conn, _store_dir: store_dir, cache: TreeCache::new(false), root }
        }

        fn internal_dir(&self) -> PathBuf {
            self.root.join(".metafolder").join("internal")
        }

        fn explain(&mut self, coverage: Coverage<'_>, paths: &[&str]) -> Vec<WatchedStatus> {
            let internal = self.internal_dir();
            let paths: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
            let rules = self.rules();
            explain_watched(
                &self.conn,
                &self.cache,
                &rules,
                &self.root,
                &internal,
                coverage,
                &paths,
            )
            .unwrap()
        }

        fn rules(&self) -> WatchRules {
            WatchRules::load(&self.conn, false).unwrap()
        }

        /// Gives `rel` its own metarecord carrying `mfr_watch_exceeded`.
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
            w.set_field(created.uuid, crate::eligibility::WATCH_EXCEEDED, Value::Bool(value))
                .unwrap();
            w.commit().unwrap();
        }

        /// The internal form of an absolute-under-root path, as the sources
        /// build it from what they see.
        fn rel(&self, under_root: &str) -> super::RelPath {
            let internal = self.internal_dir();
            relative(&self.root, &internal, &self.root.join(under_root.trim_start_matches('/')))
                .unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn test_under_coverage_every_eligible_path_is_watched() {
        let mut fx = Fixture::new();
        let got = fx.explain(Coverage::Tree, &["/dir/x"]);
        assert!(got[0].watched, "{:?}", got[0]);
        assert_eq!(got[0].reason, WatchedReason::Watched);
    }

    #[test]
    fn test_an_exclusion_is_honoured_under_coverage_too() {
        // The one piece of watch-budget vocabulary both regimes share
        // (doc "Watch sources and regimes"): `true` means
        // *leave this subtree uncovered*, wherever the coverage comes from.
        let mut fx = Fixture::new();
        fx.mark_exceeded("/dir", true);
        let got = fx.explain(Coverage::Tree, &["/dir/x"]);
        assert!(!got[0].watched, "{:?}", got[0]);
        assert_eq!(got[0].reason, WatchedReason::Excluded);
        assert_eq!(got[0].excluded_by.as_deref(), Some("/dir"));
    }

    #[test]
    fn test_the_internal_directory_is_never_covered() {
        let mut fx = Fixture::new();
        std::fs::create_dir_all(fx.internal_dir().join("sub")).unwrap();
        let got = fx.explain(Coverage::Tree, &["/.metafolder/internal/sub/db"]);
        assert!(!got[0].watched, "{:?}", got[0]);
        assert_eq!(got[0].reason, WatchedReason::Internal);
    }

    #[test]
    fn test_the_budget_regime_answer_comes_from_the_set() {
        let mut fx = Fixture::new();
        let mut set = HashSet::new();
        set.insert(fx.root.join("dir"));
        let got = fx.explain(Coverage::Watches(&set), &["/dir/x", "/other/x"]);
        assert!(got[0].watched, "{:?}", got[0]);
        // Tracked, uncovered, no recorded reason: the starved answer — the one
        // reason the coverage regime can never give.
        assert_eq!(got[1].reason, WatchedReason::Unwatched, "{:?}", got[1]);
    }

    #[test]
    fn test_an_excluded_subtree_drops_its_events_and_keeps_the_visible_side_of_a_move() {
        let mut fx = Fixture::new();
        fx.mark_exceeded("/dir", true);
        let wire = vec![
            (FsEvent::Create(fx.rel("/dir/x")), None),
            (FsEvent::Create(fx.rel("/other/x")), None),
            (FsEvent::Rename(fx.rel("/dir/a"), fx.rel("/other/b")), None),
            (FsEvent::Rename(fx.rel("/other/c"), fx.rel("/dir/d")), None),
            (FsEvent::Rename(fx.rel("/dir/e"), fx.rel("/dir/f")), None),
        ];
        let kept = drop_excluded(&fx.rules(), &wire);
        let shapes: Vec<&str> = kept
            .iter()
            .map(|(ev, _)| match ev {
                FsEvent::Create(_) => "create",
                FsEvent::Rename(_, _) => "rename",
                FsEvent::RenameFrom(_) => "rename_from",
                FsEvent::RenameTo(_) => "rename_to",
                _ => "other",
            })
            .collect();
        assert_eq!(shapes, vec!["create", "rename_to", "rename_from"], "{shapes:?}");
    }

    /// Gives `rel` (whose parent is tracked) its own metarecord with `fields`.
    fn carry(fx: &mut Fixture, rel: &str, fields: Vec<Field>) {
        let (parent_rel, name) = rel.rsplit_once('/').unwrap();
        let parent = fx.cache.resolve_path(&fx.conn, "mfr_path", parent_rel).unwrap().unwrap();
        let mut all = vec![Field::new(
            "mfr_path",
            Value::TreeRef { parent: Some(parent), name: name.into() },
        )];
        all.extend(fields);
        let mut w = Writer::begin(&mut fx.conn, None).unwrap();
        w.create_metarecord(all).unwrap();
        w.commit().unwrap();
    }

    #[test]
    fn test_ingest_drops_what_the_flush_would_turn_away_and_nothing_else() {
        let mut fx = Fixture::new();
        carry(&mut fx, "/dir", vec![Field::new("mf_watch", Value::Bool(false))]);
        carry(&mut fx, "/other", vec![Field::new("mf_ignore", Value::String(r"\.tmp$".into()))]);
        let wire = vec![
            (FsEvent::Create(fx.rel("/dir/x")), None),
            (FsEvent::ModifyData(fx.rel("/dir/x")), None),
            (FsEvent::Remove(fx.rel("/dir/y")), None),
            (FsEvent::ModifyMeta(fx.rel("/other/a.tmp")), None),
            (FsEvent::Create(fx.rel("/other/a.txt")), None),
            // One eligible side keeps the whole move: the flush decides.
            (FsEvent::Rename(fx.rel("/dir/a"), fx.rel("/other/b")), None),
            (FsEvent::Rename(fx.rel("/other/c"), fx.rel("/dir/d")), None),
            (FsEvent::Rename(fx.rel("/dir/e"), fx.rel("/other/f.tmp")), None),
            // Halves of a move are never dropped: their correlation would break.
            (FsEvent::RenameFrom(fx.rel("/dir/g")), Some(7)),
            (FsEvent::RenameTo(fx.rel("/dir/h")), Some(7)),
        ];
        let kept: Vec<String> = drop_ineligible(&fx.rules(), wire)
            .iter()
            .map(|(ev, _)| crate::executor::describe(ev))
            .collect();
        assert_eq!(kept.len(), 5, "{kept:?}");
        assert!(kept.iter().any(|k| k.contains("/other/a.txt")), "{kept:?}");
        assert!(kept.iter().any(|k| k.contains("/other/b")), "{kept:?}");
        assert!(kept.iter().any(|k| k.contains("/other/c")), "{kept:?}");
        assert!(kept.iter().any(|k| k.contains("/dir/g")), "{kept:?}");
        assert!(kept.iter().any(|k| k.contains("/dir/h")), "{kept:?}");
    }

    #[test]
    fn test_a_batch_that_moves_a_rule_is_recognised() {
        let mut fx = Fixture::new();
        carry(&mut fx, "/dir", vec![Field::new("mf_watch", Value::Bool(false))]);
        let rules = fx.rules();
        let touching = |ev: FsEvent| touches_rules(&rules, &[(ev, None)]);
        assert!(touching(FsEvent::Rename(fx.rel("/dir"), fx.rel("/moved"))));
        assert!(touching(FsEvent::Remove(fx.rel("/dir"))));
        assert!(touching(FsEvent::RenameFrom(fx.rel("/dir"))));
        assert!(touching(FsEvent::Create(fx.rel("/dir"))));
        assert!(!touching(FsEvent::ModifyMeta(fx.rel("/dir"))), "a modification moves nothing");
        assert!(!touching(FsEvent::Remove(fx.rel("/dir/inside"))), "below a rule: governed only");
        assert!(!touching(FsEvent::Remove(fx.rel("/other"))));
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
}
