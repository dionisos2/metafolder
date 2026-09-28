//! In-memory daemon state: the set of loaded repositories.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use metafolder_core::sync::MutexExt;
use serde::Serialize;
use uuid::Uuid;

use crate::config::RepoConfig;
use crate::daemon_config::DaemonSettings;
use crate::error::ApiError;
use crate::executor::FlushProgress;
use crate::phase::{Phase, Watchdog};
use crate::reconcile::ProgressFn;
use crate::repo::{self, OpenedRepo, RepoLocator};
use crate::tree_cache::TreeCache;

/// How long one filesystem event must take before the load report names it
/// with its cost. Below it the event is ordinary and saying so would drown the
/// one that is not.
const SLOW_EVENT: std::time::Duration = std::time::Duration::from_secs(1);

/// The published rule index of a repository, with the two counters that say
/// whether the ingest thread may filter against it (spec-file-tracking
/// "Filtering at ingestion", "Rules that are about to move").
#[derive(Default)]
struct RulesSlot {
    rules: Option<Arc<crate::eligibility::WatchRules>>,
    /// The HEAD the rules are known to describe: the one they were read at, or
    /// a later one whose revisions moved no rule.
    valid_at: Option<Option<i64>>,
    /// Bumped by the ingest thread once it has buffered a batch holding an
    /// event that touches a rule (a move or removal of a rule-carrying
    /// directory or of one of its ancestors).
    touched: u64,
    /// The `touched` a flush read *before* draining the buffer, recorded once
    /// that flush committed and the rules were re-read: every touching event
    /// counted in it has been applied.
    covered: u64,
}

/// One loaded repository. The SQLite connection and the tree cache each sit
/// behind their own mutex; blocking work runs in `spawn_blocking`.
pub struct RepoState {
    pub conn: Mutex<crate::store::Handle>,
    pub cache: Mutex<TreeCache>,
    pub config: RepoConfig,
    /// The repository's display name. Starts at `config.name` but is mutable
    /// (rename, spec-main "PATCH /repos/:repo") — persisted to `config.json` and
    /// the single source of truth for uniqueness and the repo listing.
    pub name: Mutex<String>,
    pub metafolder_dir: PathBuf,
    pub case_insensitive: bool,
    /// Watcher + executor; None until started (or in unit tests).
    pub handles: Mutex<Option<RepoHandles>>,
    /// Loaded user schema; replaced atomically on reload (spec-schema).
    pub schema: Mutex<Option<crate::schema::CompiledSchema>>,
    /// The per-repo embedded-metadata extraction map (spec-platform). Loaded
    /// (seeding/self-healing the on-disk file) in `activate`; initialised here to
    /// the baked-in default so a `RepoState` built without `activate` (unit
    /// tests) still extracts with sensible defaults.
    pub metadata_map: Mutex<crate::metadata_map::MetadataMap>,
    /// Coordinated-rollback lock (spec-event-log): `Some` while a rollback
    /// navigation is in progress, carrying its resolved target. Never
    /// persisted — a crash restarts unlocked.
    pub rollback_lock: Mutex<Option<RollbackLock>>,
    /// Observable background tasks for this repository (spec-tasks). In memory,
    /// separate from `conn` so progress reads never block behind a running
    /// reconcile.
    pub tasks: crate::tasks::TaskRegistry,
    /// The repository's declared mount points as of the last read that could
    /// take the repository (spec-file-tracking "Mount status"). Filled by the
    /// load and refreshed by every unblocked `GET …/mounts`; served as it stands
    /// while a long write holds the connection and the tree cache, where waiting
    /// for them would buy no freshness — a writer in flight has committed
    /// nothing, so this *is* the committed set.
    declared_mounts: Mutex<Arc<Vec<crate::mount::DeclaredMount>>>,
    /// Percentage of the kernel's watch limit this repository may spend
    /// (`[settings] watch-budget-share`).
    watch_budget_share: u8,
    /// Where the fanotify broker is probed at load (`[settings] watchd-socket`).
    watchd_socket: PathBuf,
    /// The kernel refused watches while this daemon was still under its own
    /// ceiling: another program holds the budget. A *state*, not a message —
    /// it lasts as long as the condition, so a client can keep it on screen
    /// (spec-file-tracking "Two different failures").
    starved_watches: std::sync::atomic::AtomicBool,
    /// Subtree roots carrying `mfr_watch_exceeded = true`, counted at each
    /// placement (see [`RepoState::exceeded_dirs`]).
    exceeded_dirs: std::sync::atomic::AtomicUsize,
    /// Whether the repository can serve data. False from the moment it is
    /// registered until [`RepoState::warm`] has built the accelerators and
    /// started the watcher. The accelerators are not optional — the query
    /// engine and the executor both run against them — so a repository that is
    /// not warm cannot answer slowly, it cannot answer at all: data endpoints
    /// return `503` while this is false (spec-main "POST /repos/load").
    ready: std::sync::atomic::AtomicBool,
    /// Quiet period the executor waits out before flushing (`[settings]
    /// watch-quiet-period-ms`). Held here so warming a repository needs nothing
    /// but the repository.
    watch_quiet_period: std::time::Duration,
    /// The watcher's buffered filesystem events, awaiting a flush
    /// (spec-file-tracking "Event batching"). In memory, deliberately: a daemon
    /// that is down misses every event anyway, and closing *that* gap needs a
    /// reconcile — which closes this one too. Persisting the buffer bought no
    /// coherence, cost a transaction on the watcher's hot path, and made a batch
    /// the executor could not apply outlive a restart.
    pub pending: Mutex<Vec<(crate::executor::FsEvent, Option<i64>)>>,
    /// How many events the watcher delivered under each path since the load
    /// (spec-file-tracking "Watch activity"). In memory, like the buffer, and
    /// separate from `conn`: counting is on the ingest path and reading it must
    /// answer while a flush holds the connection.
    pub watch_activity: Mutex<crate::watch_activity::WatchActivity>,
    /// The rule index (spec-file-tracking "The rule index") and how far the
    /// ingest filter may trust it. Behind its own lock, held for a pointer copy:
    /// the ingest thread reads it without ever waiting for the connection.
    watch_rules: Mutex<RulesSlot>,
    /// How much history this repository's event log keeps behind HEAD: its own
    /// `config.json` override where set, the daemon's `[settings]` otherwise.
    /// Applied by every writer built through [`Self::writer`].
    log_retention: crate::log::Retention,
    /// Mass-orphan circuit breaker (`[settings] orphan-cascade-limit`), read by
    /// the executor before applying a cascade.
    pub orphan_cascade_limit: usize,
    /// Ingestion of filesystem events is paused (spec-file-tracking "Pausing
    /// ingestion"): the watcher keeps buffering events into
    /// `pending_operation`, the executor applies none until a resume. Set by
    /// stopping a flush, and by `POST /watch/pause`. In memory like the task
    /// registry: a reload or a restart starts ingesting again.
    pub ingestion_paused: std::sync::atomic::AtomicBool,
    /// Where this repository's slow operations are written (spec-slow-log).
    /// Held per repository because the log lives inside it, and shared as an
    /// `Arc` because every instrumented path takes a clone.
    pub slowlog: Arc<metafolder_core::slowlog::Recorder>,
}

/// State of an in-progress coordinated operation. Both kinds suspend the
/// watcher's execution and refuse writes for the same reason: a client is about
/// to move files, and the metadata explaining those moves is not written yet
/// (spec-event-log "Rollback lock").
pub enum RollbackLock {
    /// A coordinated rollback navigation: the steps left to its target,
    /// planned once at `start` (nothing else writes while it runs).
    Navigate { plan: crate::log::NavPlan },
    /// A coordinated revert: the operations `start` fixed. `commit` may only
    /// narrow this set, never widen it.
    Revert { ops: Vec<i64> },
}

impl RepoState {
    /// Absolute path of `.metafolder/internal/` — the only part of the
    /// repository excluded from tracking (watcher and reconcile).
    pub fn internal_dir(&self) -> PathBuf {
        self.metafolder_dir.join(repo::INTERNAL_DIR)
    }

    /// Builds a `RepoState` with the default daemon settings (used by tests and
    /// by [`Self::from_opened_with`]).
    pub fn from_opened(opened: OpenedRepo) -> Self {
        Self::from_opened_with(opened, &DaemonSettings::default())
    }

    /// Builds a `RepoState`, applying the tunable daemon settings (here, the
    /// tree-cache node budget).
    pub fn from_opened_with(opened: OpenedRepo, settings: &DaemonSettings) -> Self {
        let repo_uuid = opened.config.repo_uuid;
        let name = Mutex::new(opened.config.name.clone());
        let log_retention = opened.config.log_retention(settings.log_retention());
        let slow_dir =
            metafolder_core::slowlog::slow_dir(&opened.metafolder_dir.join(repo::INTERNAL_DIR));
        let slowlog = Arc::new(metafolder_core::slowlog::Recorder::new(
            Some(slow_dir),
            "daemon",
            settings.slow_operation_threshold_ms,
        ));
        Self {
            conn: Mutex::new(opened.conn),
            // No forest in memory: the store answers (spec-storage increment
            // 4 e).
            cache: Mutex::new(TreeCache::new(opened.case_insensitive).without_forest()),
            config: opened.config,
            name,
            metafolder_dir: opened.metafolder_dir,
            case_insensitive: opened.case_insensitive,
            handles: Mutex::new(None),
            schema: Mutex::new(None),
            metadata_map: Mutex::new(
                crate::metadata_map::MetadataMap::parse(crate::metadata_map::DEFAULT)
                    .expect("baked default metadata map is valid"),
            ),
            rollback_lock: Mutex::new(None),
            tasks: crate::tasks::TaskRegistry::new(repo_uuid),
            declared_mounts: Mutex::new(Arc::new(Vec::new())),
            ready: std::sync::atomic::AtomicBool::new(false),
            watch_budget_share: settings.watch_budget_share,
            watchd_socket: settings.watchd_socket.clone(),
            starved_watches: std::sync::atomic::AtomicBool::new(false),
            exceeded_dirs: std::sync::atomic::AtomicUsize::new(0),
            watch_quiet_period: settings.watch_quiet_period(),
            pending: Mutex::new(Vec::new()),
            watch_activity: Mutex::new(crate::watch_activity::WatchActivity::new(
                metafolder_core::date::now_ms(),
                crate::watch_activity::DEFAULT_CAP,
            )),
            watch_rules: Mutex::new(RulesSlot::default()),
            orphan_cascade_limit: settings.orphan_cascade_limit,
            log_retention,
            ingestion_paused: std::sync::atomic::AtomicBool::new(false),
            slowlog,
        }
    }

    /// Opens a logged write on this repository. The only way a loaded
    /// repository is written: it is what applies the configured log retention,
    /// so a path that reaches for [`crate::log::Writer::begin`] directly keeps
    /// the whole history whatever the configuration says.
    pub fn writer<'c>(
        &self,
        conn: &'c mut dyn crate::store::Database,
        label: Option<String>,
    ) -> anyhow::Result<crate::log::Writer<'c>> {
        crate::log::Writer::begin_with_retention(conn, label, self.log_retention)
    }

    /// This repository's effective log retention.
    pub fn log_retention(&self) -> crate::log::Retention {
        self.log_retention
    }

    /// The repository's current (mutable) display name.
    pub fn name(&self) -> String {
        self.name.lock_recover().clone()
    }

    /// This repository's uuid, as diagnostics and tasks spell it.
    pub fn uuid(&self) -> Uuid {
        self.config.repo_uuid
    }

    /// This repository's listing info (the `GET /repos` / `GET /repos/:repo`
    /// shape), reading the live name.
    pub fn info(&self) -> RepoInfo {
        RepoInfo {
            repo_uuid: self.config.repo_uuid,
            name: self.name(),
            root: self.config.root.clone(),
            internal_dir: self.internal_dir(),
            created_at: self.config.created_at,
            system: self.config.system,
        }
    }

    /// Renames the repository: rewrites `config.json` with the new name, then
    /// swaps the in-memory name. Uniqueness is enforced by the caller
    /// ([`AppState::rename_repo`]).
    pub fn rename(&self, new_name: String) -> anyhow::Result<()> {
        let cfg = RepoConfig { name: new_name.clone(), ..self.config.clone() };
        cfg.write(&self.metafolder_dir)?;
        *self.name.lock_recover() = new_name;
        Ok(())
    }
    /// The rule index, current as of the connection's HEAD: re-read when a
    /// revision since the last read may have moved a rule, and published for
    /// the ingest thread. One `head` read when nothing changed.
    pub fn watch_rules(
        &self,
        conn: &dyn crate::store::Store,
    ) -> anyhow::Result<Arc<crate::eligibility::WatchRules>> {
        let head = conn.head()?;
        {
            let slot = self.watch_rules.lock_recover();
            if let (Some(rules), Some(valid_at)) = (&slot.rules, slot.valid_at) {
                if valid_at == head {
                    return Ok(rules.clone());
                }
            }
        }
        let _phase = metafolder_core::slowlog::phase("rules.load");
        let rules = Arc::new(crate::eligibility::WatchRules::load(conn, self.case_insensitive)?);
        let mut slot = self.watch_rules.lock_recover();
        slot.rules = Some(rules.clone());
        slot.valid_at = Some(rules.head());
        Ok(rules)
    }

    /// Brings the rule index in step with a revision just committed: re-read
    /// when it wrote a rule or moved a metarecord a rule depends on, otherwise
    /// only re-stamped with the new HEAD. A failure is logged — the next reader
    /// re-reads.
    fn settle_watch_rules(
        &self,
        conn: &dyn crate::store::Store,
        effects: &crate::log::WriteEffects,
    ) {
        let moved_a_rule = {
            let slot = self.watch_rules.lock_recover();
            match &slot.rules {
                None => return, // Never read: the first reader will.
                Some(rules) => {
                    effects.touches_watch()
                        || effects
                            .tree_ops()
                            .iter()
                            .any(|op| op.field() == "mfr_path" && rules.affects(op.uuid()))
                }
            }
        };
        let result = if moved_a_rule {
            let _phase = metafolder_core::slowlog::phase("settle.rules");
            crate::eligibility::WatchRules::load(conn, self.case_insensitive).map(|rules| {
                let mut slot = self.watch_rules.lock_recover();
                slot.valid_at = Some(rules.head());
                slot.rules = Some(Arc::new(rules));
            })
        } else {
            conn.head().map(|head| {
                let mut slot = self.watch_rules.lock_recover();
                // Only a stamp that described the state this revision started
                // from moves forward: one already behind (a revision that did
                // not settle, such as a flush) stays behind, and the next
                // reader re-reads.
                if slot.valid_at == Some(effects.base_head()) {
                    slot.valid_at = Some(head);
                }
            })
        };
        if let Err(e) = result {
            self.watch_rules.lock_recover().valid_at = None;
            crate::diagnostics::warn_for(
                "watcher",
                format!("could not bring the watch rules up to the new revision: {e:#}"),
                self.uuid(),
            );
        }
    }

    /// What the ingest thread filters with: the published rules, and whether
    /// they can be trusted to drop ineligible events — not while a buffered
    /// event that moves a rule is waiting for its flush.
    pub fn ingest_rules(&self) -> (Option<Arc<crate::eligibility::WatchRules>>, bool) {
        let slot = self.watch_rules.lock_recover();
        (slot.rules.clone(), slot.touched <= slot.covered)
    }

    /// Records that a batch touching a rule has been buffered: filtering stays
    /// off until a flush that drained it has committed ([`Self::rules_covered`]).
    pub fn rules_touched(&self) {
        self.watch_rules.lock_recover().touched += 1;
    }

    /// The touch count a flush reads *before* draining the buffer.
    pub fn rules_touch_mark(&self) -> u64 {
        self.watch_rules.lock_recover().touched
    }

    /// A flush that read `mark` before draining has committed, and the rules
    /// were re-read after it: the touching events it counted are in them.
    pub fn rules_covered(&self, mark: u64) {
        let mut slot = self.watch_rules.lock_recover();
        slot.covered = slot.covered.max(mark);
    }

    /// Locks the tree cache, recovering from a poisoned mutex. Unlike the
    /// connection (whose writes are transactional, so a panic mid-write is
    /// already rolled back), the in-memory cache can be left half-updated by a
    /// panic — and out of step with the rolled-back write — so its contents
    /// are discarded on recovery; it repopulates lazily from the DB. The
    /// poison flag is cleared so later locks take the normal fast path.
    /// See `docs/review-followups.md` (#5).
    pub fn lock_cache(&self) -> MutexGuard<'_, TreeCache> {
        match self.cache.lock() {
            Ok(guard) => guard,
            Err(poison) => {
                self.cache.clear_poison();
                let mut guard = poison.into_inner();
                guard.clear();
                guard
            }
        }
    }

    /// [`Self::lock_cache`] if the tree cache is free right now, `None` if a
    /// write holds it — for the reader that has a resident answer and only
    /// loses freshness by not waiting.
    pub fn try_lock_cache(&self) -> Option<MutexGuard<'_, TreeCache>> {
        match self.cache.try_lock() {
            Ok(guard) => Some(guard),
            Err(std::sync::TryLockError::Poisoned(poison)) => {
                self.cache.clear_poison();
                let mut guard = poison.into_inner();
                guard.clear();
                Some(guard)
            }
            Err(std::sync::TryLockError::WouldBlock) => None,
        }
    }

    /// The resident declared mount points (see the field): the answer
    /// `GET …/mounts` falls back on while the repository is busy.
    pub fn declared_mounts(&self) -> Arc<Vec<crate::mount::DeclaredMount>> {
        Arc::clone(&self.declared_mounts.lock_recover())
    }

    /// Replaces the resident declared mount points with a set just read from
    /// the database. Takes the `Arc` the caller keeps, so the answer it serves
    /// and the one it leaves behind are the same snapshot.
    pub fn set_declared_mounts(&self, mounts: Arc<Vec<crate::mount::DeclaredMount>>) {
        *self.declared_mounts.lock_recover() = mounts;
    }

    /// True while a coordinated rollback navigation holds the lock.
    pub fn is_rollback_locked(&self) -> bool {
        self.rollback_lock.lock_recover().is_some()
    }

    /// True while filesystem-event ingestion is paused for this repository.
    pub fn is_ingestion_paused(&self) -> bool {
        self.ingestion_paused.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Pauses ingestion and stops the flush in progress, if any: the running
    /// flush observes the cancellation at its next event, abandons the group it
    /// was applying and leaves the whole batch buffered. Returns whether a
    /// flush was actually asked to stop.
    pub fn pause_ingestion(&self) -> bool {
        self.ingestion_paused.store(true, std::sync::atomic::Ordering::Relaxed);
        match self.tasks.active_id(crate::tasks::TaskKind::Flush) {
            Some(id) => self.tasks.request_cancel(id) == crate::tasks::CancelOutcome::Requested,
            None => false,
        }
    }

    /// Resumes ingestion and pings the executor, so what accumulated while
    /// paused is flushed after the usual quiet period. No-op when not paused.
    pub fn resume_ingestion(&self) {
        self.ingestion_paused.store(false, std::sync::atomic::Ordering::Relaxed);
        let handles = self.handles.lock_recover();
        if let Some(handles) = handles.as_ref() {
            handles.executor.pinger().ping();
        }
    }

    /// Brings this repository's in-memory state back in step with a revision
    /// that has just been committed on `conn` — the tree cache and, when the
    /// write changed the watched scope, the inotify watch set.
    ///
    /// The tree cache is reconciled *cell by cell*
    /// ([`TreeCache::apply_cells`]). It used to be rebuilt outright after any
    /// write that touched a `tree_ref` row, which meant one full scan of the
    /// `field` table — seconds on a large repository, with the connection held
    /// throughout, so nothing else could be read while a single field was
    /// being set. Nothing rebuilds it here any more, whatever the write: the
    /// only rebuilds left are the initial load, an explicit one, and the paths
    /// that rewrite history wholesale *in one transaction* (the atomic
    /// rollback, the restore replay, the resync after an abandoned flush). A
    /// coordinated navigation *step* is not one of them — it applies a single
    /// operation, which names its own cells, and a rebuild per operation is
    /// what made going back over a large write cost minutes.
    pub fn settle(
        &self,
        conn: &dyn crate::store::Store,
        effects: &crate::log::WriteEffects,
    ) -> anyhow::Result<()> {
        if effects.touches_tree() {
            let _phase = metafolder_core::slowlog::phase("settle.tree");
            let mut cache = self.lock_cache();
            if !cache.apply_ops(effects.tree_ops()) {
                // Only before the repository's initial load, which through the
                // API cannot happen: it serves nothing until the forest is
                // resident (spec-main "POST /repos/load").
                cache.populate(conn)?;
            }
        }
        self.settle_watch_rules(conn, effects);
        if effects.touches_watch() {
            let _phase = metafolder_core::slowlog::phase("settle.watches");
            self.refresh_watches(conn);
        }
        Ok(())
    }

    /// Recomputes the watcher's eligible-directory set after a manual write that
    /// changed `mf_watch`/`mf_ignore` (spec-file-tracking "Watch and Ignore"),
    /// so a subtree just made eligible starts being watched immediately (and one
    /// just excluded stops). No-op when the watcher is not running (unit tests,
    /// or a repository being torn down). `conn` is the already-locked
    /// connection; the tree cache is locked here.
    pub fn refresh_watches(&self, conn: &dyn crate::store::Store) -> usize {
        // The ingest filter reads the published rules: a navigation that
        // restored or took away a rule without settling must not leave it
        // filtering against the old ones.
        if let Err(err) = self.watch_rules(conn) {
            crate::diagnostics::warn_for(
                "watcher",
                format!("could not re-read the watch rules: {err:#}"),
                self.uuid(),
            );
        }
        let cap = crate::watcher::budget_cap_for(self.watch_budget_share);
        let placement = {
            let handles = self.handles.lock_recover();
            let Some(handles) = handles.as_ref() else {
                return 0;
            };
            let mut cache = self.lock_cache();
            handles.watcher.refresh(conn, &mut cache, &self.config.root, &self.internal_dir(), cap)
        };
        if !placement.frontier.is_empty() {
            self.record_watch_frontier(&placement.frontier);
        }
        self.starved_watches.store(placement.starved > 0, std::sync::atomic::Ordering::Relaxed);
        let excluded = crate::store::Questions::holding(
            conn,
            crate::eligibility::WATCH_EXCEEDED,
            &metafolder_core::metarecord::Value::Bool(true),
        )
        .map_or(0, |v| v.len());
        self.exceeded_dirs.store(excluded, std::sync::atomic::Ordering::Relaxed);
        placement.watched
    }

    /// Records the subtree roots the watch budget could not afford, as
    /// `mfr_watch_exceeded = true` (spec-file-tracking "The watch budget").
    ///
    /// Only the frontier reaches here — the subtrees the placement did not
    /// enter — so a repository too large to watch does not also pay one write
    /// per directory to say so.
    pub fn record_watch_frontier(&self, frontier: &[String]) {
        let mut conn = self.conn.lock_recover();
        let mut cache = self.lock_cache();
        if let Err(err) =
            write_watch_frontier(self, &mut conn, &mut cache, &self.config.root, frontier)
        {
            crate::diagnostics::warn(
                "watcher",
                format!("could not record the watch frontier: {err:#}"),
            );
        }
    }

    /// Directories currently watched — one inotify watch each, on a budget
    /// shared with every other program on the machine that watches files.
    pub fn watched_dirs(&self) -> usize {
        self.handles.lock_recover().as_ref().map_or(0, |h| h.watcher.watched())
    }

    /// The active watch source's wire name — `GET /watch`'s `backend`
    /// (spec-file-tracking "Watch sources and regimes"). While the watcher is
    /// not running (unit tests, a repository being torn down) the platform's
    /// notify backend is named, so the view still answers.
    /// Why this repository is not on the fanotify broker, when it is not
    /// (`GET /watch` `backend_reason`); `None` on the broker, or before the
    /// watcher started.
    pub fn watch_backend_reason(&self) -> Option<String> {
        self.handles
            .lock_recover()
            .as_ref()
            .and_then(|h| h.watcher.backend_reason().map(str::to_string))
    }

    pub fn watch_backend(&self) -> &'static str {
        self.handles
            .lock_recover()
            .as_ref()
            .map_or_else(crate::watcher::inotify::platform_backend_name, |h| h.watcher.backend())
    }

    /// Whether the watch-budget vocabulary applies to this repository's source
    /// (spec-file-tracking "Watch sources and regimes"): `watch_budget` and
    /// `watched_dirs` carry nothing under the coverage regime, where one
    /// registration covers the tree and there is no per-directory state.
    pub fn watch_budget_regime(&self) -> bool {
        self.handles
            .lock_recover()
            .as_ref()
            .is_none_or(|h| h.watcher.regime() == crate::watcher::Regime::Budget)
    }

    /// The absolute paths of the directories currently watched, as a snapshot
    /// (what `POST /watch/check` answers against). Empty while the repository's
    /// watcher is not running.
    pub fn watched_dir_set(&self) -> HashSet<PathBuf> {
        self.handles.lock_recover().as_ref().map_or_else(HashSet::new, |h| h.watcher.watched_set())
    }

    /// Prepares a freshly loaded repository to serve: reads its declared mount
    /// points. There is nothing else to warm — the queries and the forest are
    /// answered by the store itself (spec-storage increment 4) — but the load
    /// is still reported through `progress` `(phase, done, total)`, a no-op
    /// for the synchronous callers (startup auto-load, `init`).
    pub fn warmup(&self, progress: ProgressFn) -> Result<(), ApiError> {
        let conn = self.conn.lock_recover();
        progress("mounts", None, None);
        // The declared mount points, read once here so they are resident from
        // the start: a repository is very often opened *while* something writes
        // to it (the load's own event replay, an auto-reconcile), and the first
        // listing must not be the one that loses the unavailable-volume marking.
        // A handful of rows off an indexed field — no phase of its own.
        self.set_declared_mounts(Arc::new(crate::mount::declared_set(
            &conn,
            &mut self.lock_cache(),
        )?));
        Ok(())
    }

    /// Reads the repository's configuration files: the user schema and the
    /// embedded-metadata map (spec-schema, spec-platform "Configuration").
    ///
    /// Deliberately *not* part of [`Self::warm`]. These are small files that
    /// need no accelerator, and an invalid one makes the repository bad rather
    /// than slow: it must fail the load itself, with the `400` naming the
    /// offending constraint, instead of surfacing later as a warmup that
    /// happens to have failed on a repository already registered.
    pub fn load_config(&self) -> Result<(), ApiError> {
        let who = self.name();
        {
            let _p = Phase::begin(&who, "load the schema");
            let schema = crate::schema::load_for_repo(&self.metafolder_dir, &self.config)
                .map_err(ApiError::bad_request)?;
            *self.schema.lock_recover() = schema;
        }
        {
            let _p = Phase::begin(&who, "load the metadata map");
            let metadata_map = crate::metadata_map::MetadataMap::load_or_seed(&self.metafolder_dir)
                .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
            *self.metadata_map.lock_recover() = metadata_map;
        }
        Ok(())
    }

    /// Makes the repository usable: builds the accelerators, then activates it,
    /// then declares it ready.
    ///
    /// This is the *whole* load, in the one order that is not a special case.
    /// The executor works against the query index and the resident forest at
    /// runtime; it must do so from its very first flush too, rather than
    /// replaying a backlog through cold database walks while the index it will
    /// need is built behind it.
    ///
    /// `progress` reports `(phase, done, total)` for the load progress bar; it
    /// is a no-op for the synchronous callers (startup auto-load, `init`).
    /// Where this repository's backups go by default: `internal/backups/`,
    /// which is never tracked.
    fn backups_dir(&self) -> PathBuf {
        self.internal_dir().join("backups")
    }

    /// Takes a verified backup (`mf repo backup`, spec-storage increment 5)
    /// into `dest` — which must not exist — or, by default, into a new
    /// `internal/backups/manual-<time>/`.
    pub fn backup(&self, dest: Option<PathBuf>) -> Result<crate::backup::BackupInfo, ApiError> {
        let dest = match dest {
            Some(dest) => {
                if dest.exists() {
                    return Err(ApiError::bad_request(format!(
                        "{} already exists; a backup goes to a new directory",
                        dest.display()
                    )));
                }
                dest
            }
            None => {
                let stamp = metafolder_core::date::iso8601_from_ms(metafolder_core::date::now_ms())
                    .replace(':', "-");
                self.backups_dir().join(format!("manual-{stamp}"))
            }
        };
        let conn = self.conn.lock_recover();
        crate::backup::write_backup(&**conn, &self.metafolder_dir, &dest)
            .map_err(|e| ApiError::internal(format!("backup failed: {e:#}")))
    }

    /// Takes the automatic backup when it is due: when the one in
    /// `internal/backups/auto/` is older than `every_days` days (or missing).
    /// One slot, replaced — and only by a backup that checks clean, so a
    /// damaged store keeps the last good one. `every_days = 0` turns it off.
    pub fn auto_backup_if_due(
        &self,
        now_ms: i64,
        every_days: u32,
    ) -> Result<Option<crate::backup::BackupInfo>, ApiError> {
        if every_days == 0 {
            return Ok(None);
        }
        let dest = self.backups_dir().join("auto");
        let interval = i64::from(every_days) * 24 * 3600 * 1000;
        if let Some(last) = crate::backup::read_info(&dest) {
            if now_ms - last.created_at_ms < interval {
                return Ok(None);
            }
        }
        let conn = self.conn.lock_recover();
        crate::backup::write_backup(&**conn, &self.metafolder_dir, &dest)
            .map(Some)
            .map_err(|e| ApiError::internal(format!("automatic backup failed: {e:#}")))
    }

    /// What no longer holds together in the store (`mf repo check`,
    /// spec-storage increment 5); empty when healthy.
    pub fn check_store(&self) -> Result<Vec<String>, ApiError> {
        let conn = self.conn.lock_recover();
        let mut problems = crate::store::Begin::check(&conn)
            .map_err(|e| ApiError::internal(format!("the check could not run: {e:#}")))?;
        problems.extend(
            crate::store::one_position_problems(&**conn)
                .map_err(|e| ApiError::internal(format!("the check could not run: {e:#}")))?,
        );
        Ok(problems)
    }

    /// Derives again what the store derives (`mf repo reindex`).
    pub fn reindex_store(&self) -> Result<(), ApiError> {
        let mut conn = self.conn.lock_recover();
        crate::store::Begin::reindex(&mut conn)
            .map_err(|e| ApiError::internal(format!("reindex failed: {e:#}")))
    }

    pub fn warm(self: &Arc<Self>, progress: ProgressFn) -> Result<(), ApiError> {
        self.warmup(progress)?;
        self.activate()?;
        self.ready.store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    /// Applies whatever the watcher buffered, then starts the watcher and its
    /// executor and places the watches. Runs *after* [`Self::warmup`], so every
    /// step of it — the replay's path resolutions above all — is served from
    /// the accelerators rather than from cold database walks.
    fn activate(self: &Arc<Self>) -> Result<(), ApiError> {
        // Each step is announced: the replay below applies whatever the
        // filesystem did while the daemon was down, which on a repository that
        // moved a lot is the longest part of a load (spec-main "Startup
        // report").
        let who = self.name();
        {
            let mut p = Phase::begin(&who, "replay the buffered filesystem events");
            // The backlog is whatever the filesystem did while the daemon was
            // down: it is the one phase whose size is unknowable from outside,
            // so it reports its own.
            // The replay reports per event *and* per scanned directory entry;
            // one line a second is what a person can read, and is enough to
            // tell "still moving, here" from "stuck, here".
            let throttle = std::cell::RefCell::new(Phase::progress_throttle());
            // The steps below announce themselves *before* they run, which says
            // nothing once one of them stops coming back — so a watcher outside
            // the flush reports whatever step stays current.
            let watchdog = Watchdog::start(&who, SLOW_EVENT);
            // The step names alone ("resolve path") do not say which event is
            // stuck; the current event labels them.
            let current = std::cell::RefCell::new(String::new());
            let stats =
                crate::executor::flush_pending_reported(self, &|progress| match progress {
                    FlushProgress::Buffered(n) => {
                        eprintln!("[load {who}]   {n} event(s) buffered")
                    }
                    FlushProgress::Compacted(n) => {
                        eprintln!("[load {who}]   {n} event(s) after compaction")
                    }
                    // The first and the last event always get a line: without
                    // them a batch shorter than the interval says nothing at
                    // all — which is exactly the case that looked like a hang.
                    FlushProgress::Applying { index, total, event } => {
                        let what = crate::executor::describe(event);
                        if index == 1 || index == total || throttle.borrow_mut().ready() {
                            eprintln!("[load {who}]   event {index}/{total}: {what}");
                        }
                        *current.borrow_mut() = format!("event {index}/{total}: {what}");
                        watchdog.doing(current.borrow().clone());
                    }
                    FlushProgress::Scanning { dir, ingested } => {
                        if throttle.borrow_mut().ready() {
                            eprintln!(
                                "[load {who}]     scanning {}: {ingested} entries ingested",
                                dir.display()
                            );
                        }
                        // The count is part of what the scan declares, so a
                        // scan that is *progressing* keeps resetting the
                        // watchdog's clock and it stays quiet — while one that
                        // stops on a single entry is reported like any other
                        // stuck step.
                        watchdog.doing(format!(
                            "{} — scanning {} ({ingested} entries in)",
                            current.borrow(),
                            dir.display()
                        ));
                    }
                    // A step line is only ever printed because the throttle let
                    // it through, which means the event has already been running
                    // for a while: exactly the case where knowing the step is
                    // the answer.
                    FlushProgress::Step { name } => {
                        watchdog.doing(format!("{} — {name}", current.borrow()))
                    }
                    // A fast event says nothing; a slow one is named with what
                    // it cost, so the report keeps a record of where the time
                    // went even after the load finishes.
                    FlushProgress::Applied { index, total, elapsed } => {
                        if elapsed >= SLOW_EVENT {
                            eprintln!("[load {who}]   event {index}/{total} took {elapsed:?}");
                        }
                    }
                })?;
            // Stopped as soon as the flush returns: the last step stays current
            // until the watcher is dropped, and a tick landing in that window
            // reports a step that has already finished.
            drop(watchdog);
            p.detail(format!("{} events, {} revisions", stats.events, stats.revisions));
        }
        let quiet = self.watch_quiet_period;
        let executor = crate::executor::spawn(self, quiet);
        let watcher = {
            let _p = Phase::begin(&who, "start the watcher");
            crate::watcher::start(self, executor.pinger())?
        };
        *self.handles.lock_recover() = Some(RepoHandles { watcher, executor });

        // The watches go last: placing them walks every eligible directory, and
        // that walk reads each directory's eligibility from the tree cache the
        // warmup has just filled instead of asking the database per directory.
        {
            let mut p = Phase::begin(&who, "place the filesystem watches");
            let conn = self.conn.lock_recover();
            let watched = self.refresh_watches(&conn);
            // What coverage looks like is regime-specific (spec-file-tracking
            // "Watch sources and regimes"): one inotify watch per directory —
            // worth stating, the budget is per user and shared — or the whole
            // tree under one registration.
            p.detail(if self.watch_budget_regime() {
                format!("{watched} directories")
            } else {
                format!("tree covered ({})", self.watch_backend())
            });
        }
        Ok(())
    }

    /// How long the executor waits with no new event before flushing.
    pub fn watch_quiet_period(&self) -> std::time::Duration {
        self.watch_quiet_period
    }

    /// The share of the kernel's watch limit this repository may spend.
    pub fn watch_budget_share(&self) -> u8 {
        self.watch_budget_share
    }

    /// Where the fanotify broker is probed at load (`[settings] watchd-socket`).
    pub fn watchd_socket(&self) -> &Path {
        &self.watchd_socket
    }

    /// How many subtree roots carry `mfr_watch_exceeded = true` — what the
    /// budget could not afford, and what `mf watch exceeded` reshapes.
    ///
    /// Kept in memory rather than counted on demand: `GET /watch` must answer
    /// while a flush holds the connection — that is the whole point of being
    /// able to `pause` one — so it may take no lock the flush could be holding.
    /// Refreshed by every placement, which is when it can change (a write to
    /// the field triggers one, `Writer::touched_watch`).
    pub fn exceeded_dirs(&self) -> usize {
        self.exceeded_dirs.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Whether the kernel is refusing watches although this daemon is under its
    /// own ceiling (spec-file-tracking "Two different failures").
    pub fn starved_watches(&self) -> bool {
        self.starved_watches.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Whether the repository can serve data (see [`RepoState::ready`]).
    pub fn is_ready(&self) -> bool {
        self.ready.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Rejects a metadata write with `423 Locked` while a rollback navigation
    /// is in progress (spec-event-log "Rollback lock").
    pub fn ensure_writable(&self) -> Result<(), ApiError> {
        if self.is_rollback_locked() {
            Err(ApiError::locked(
                "repository is in rollback lock; complete or abort the navigation first",
            ))
        } else {
            Ok(())
        }
    }
}

/// Writes `mfr_watch_exceeded = true` on each subtree root of `frontier`,
/// creating the directory's metarecord when it has none yet.
fn write_watch_frontier(
    repo: &RepoState,
    conn: &mut dyn crate::store::Database,
    cache: &mut TreeCache,
    root: &Path,
    frontier: &[String],
) -> anyhow::Result<()> {
    let mut writer = repo.writer(conn, None)?;
    for rel in frontier {
        let path = crate::relpath::RelPath::from_display(rel);
        let uuid = match cache.resolve_path(writer.store(), "mfr_path", rel)? {
            Some(uuid) => uuid,
            None => {
                // Not tracked yet — a directory the placement met before
                // reconcile did. It gets the metarecord its exclusion hangs on.
                let parent = crate::executor::ensure_parent_metarecords(
                    &mut writer,
                    cache,
                    root,
                    &path,
                    &[],
                )?;
                let name = path.name().cloned().unwrap_or_default();
                let created = writer.create_metarecord(vec![
                    metafolder_core::metarecord::Field::new(
                        "mfr_path",
                        metafolder_core::metarecord::Value::TreeRef {
                            parent: Some(parent),
                            name: name.clone(),
                        },
                    ),
                    metafolder_core::metarecord::Field::new(
                        "mfr_type",
                        metafolder_core::metarecord::Value::String("dir".into()),
                    ),
                ])?;
                cache.apply_insert("mfr_path", Some(parent), &name, created.uuid);
                created.uuid
            }
        };
        writer.set_field(
            uuid,
            crate::eligibility::WATCH_EXCEEDED,
            metafolder_core::metarecord::Value::Bool(true),
        )?;
    }
    writer.commit()
}

/// Background machinery of a loaded repository. Held by the RepoState so it
/// is dropped (watcher stopped, executor joined) when the repo is unloaded.
pub struct RepoHandles {
    pub watcher: crate::watcher::WatcherHandle,
    pub executor: crate::executor::ExecutorHandle,
}

#[derive(Default)]
pub struct AppState {
    repos: Mutex<HashMap<Uuid, Arc<RepoState>>>,
    /// Shipped default schema copied into each new repo at init (spec-schema).
    /// `None` (the default, used by tests) disables seeding.
    seed_schema_path: Option<PathBuf>,
    /// Tunable UX/performance settings from `config.toml`'s `[settings]`, applied
    /// to every repository this state opens (tree-cache budget, watcher quiet
    /// period). Defaults when unset (tests, no config file).
    settings: DaemonSettings,
}

/// Public description of a loaded repository (`GET /repos`).
#[derive(Debug, Serialize)]
pub struct RepoInfo {
    #[serde(with = "metafolder_core::metarecord::hex_uuid")]
    pub repo_uuid: Uuid,
    pub name: String,
    pub root: PathBuf,
    /// `.metafolder/internal/`, always excluded from tracking; exposed so
    /// clients can flag it without guessing the metafolder location.
    pub internal_dir: PathBuf,
    pub created_at: u64,
    /// A daemon-internal repository (spec-sync plan repo), hidden from the
    /// default `GET /repos` listing.
    pub system: bool,
}

impl AppState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Configures the shipped default schema seeded into each new repo at init
    /// (`<config>/daemon/schema.default.json`). `None` disables seeding.
    pub fn with_seed_schema(mut self, path: Option<PathBuf>) -> Self {
        self.seed_schema_path = path;
        self
    }

    /// Sets the tunable settings (`config.toml` `[settings]`) applied to every
    /// repository this state opens.
    pub fn with_settings(mut self, settings: DaemonSettings) -> Self {
        self.settings = settings;
        self
    }

    /// Initialises a new repository and registers it as loaded.
    pub fn init_repo(
        &self,
        root: &Path,
        metafolder: Option<&Path>,
        name: Option<&str>,
        system: bool,
    ) -> Result<Uuid, ApiError> {
        let opened = repo::init_repository(root, metafolder, name, system)?;
        let uuid = opened.config.repo_uuid;
        self.ensure_name_available(&opened.config.name)?;
        // Seed the per-repo schema from the shipped default (best-effort),
        // before activate() reads it.
        if let Some(src) = self.seed_schema_path.as_deref() {
            repo::seed_schema_file(&opened.metafolder_dir, src);
        }
        let repo_state = Arc::new(RepoState::from_opened_with(opened, &self.settings));
        repo_state.load_config()?;
        // A fresh repository is tiny, so warm it synchronously (no progress bar):
        // `init` returns a repository that already answers.
        repo_state.warm(&|_, _, _| {})?;
        self.repos.lock_recover().insert(uuid, repo_state);
        Ok(uuid)
    }

    /// Loads an existing repository. Loading an already-loaded repository is
    /// idempotent and returns its UUID (the exclusive SQLite lock would make
    /// a second real open fail anyway).
    pub fn load_repo(&self, locator: RepoLocator) -> Result<Uuid, ApiError> {
        let metafolder_dir = match &locator {
            RepoLocator::Root(root) => root
                .canonicalize()
                .map_err(|_| {
                    ApiError::bad_request(format!(
                        "Cannot resolve path {root:?}: the root directory must exist"
                    ))
                })?
                .join(".metafolder"),
            RepoLocator::Metafolder(dir) => dir.clone(),
        };
        if RepoConfig::exists(&metafolder_dir) {
            let config = RepoConfig::read(&metafolder_dir)?;
            if self.repos.lock_recover().contains_key(&config.repo_uuid) {
                return Ok(config.repo_uuid);
            }
        }
        let opened = repo::load_repository(RepoLocator::Metafolder(metafolder_dir))?;
        let uuid = opened.config.repo_uuid;
        self.ensure_name_available(&opened.config.name)?;
        // Registered, not yet ready: the caller warms it — synchronously, or as
        // the observable `load` task `POST /repos/load` returns. Until then it
        // reports its state and refuses data (`RepoState::ready`).
        let repo_state = Arc::new(RepoState::from_opened_with(opened, &self.settings));
        // Before registering: an invalid schema must fail the load, not leave a
        // registered repository that never becomes ready.
        repo_state.load_config()?;
        self.repos.lock_recover().insert(uuid, repo_state);
        Ok(uuid)
    }

    /// Rejects a name already held by a loaded repository — names are unique
    /// among loaded repos, so the CLI's `-n <name>` selector resolves to exactly
    /// one UUID (spec-main "Global selection flags").
    fn ensure_name_available(&self, name: &str) -> Result<(), ApiError> {
        if self.repos.lock_recover().values().any(|r| r.name() == name) {
            return Err(ApiError::conflict(format!(
                "a repository named '{name}' is already loaded; names must be unique"
            )));
        }
        Ok(())
    }

    /// Takes the automatic backup of every loaded repository that is due
    /// (`auto-backup-days`); a failure is a warning in that repository's
    /// diagnostics, its previous backup untouched.
    pub fn run_auto_backups(&self) {
        let days = self.settings.auto_backup_days;
        if days == 0 {
            return;
        }
        let repos: Vec<Arc<RepoState>> = self
            .repos
            .lock_recover()
            .values()
            .filter(|r| !r.config.system && r.is_ready())
            .cloned()
            .collect();
        for repo in repos {
            match repo.auto_backup_if_due(metafolder_core::date::now_ms(), days) {
                Ok(Some(info)) => {
                    eprintln!("[backup] {}: {}", repo.name(), info.path.display());
                }
                Ok(None) => {}
                Err(e) => crate::diagnostics::warn_for("backup", e.message, repo.uuid()),
            }
        }
    }

    /// Runs [`Self::run_auto_backups`] now, then every hour, for as long as
    /// the daemon lives.
    pub fn start_auto_backups(self: &Arc<Self>) {
        let state = Arc::downgrade(self);
        std::thread::spawn(move || loop {
            let Some(live) = state.upgrade() else { return };
            live.run_auto_backups();
            drop(live);
            std::thread::sleep(std::time::Duration::from_secs(3600));
        });
    }

    /// Unloads a repository and waits until its store is released — a
    /// request still running holds it a moment longer, and its store (and
    /// its lock) goes with the last reference. Answers where it lives, to
    /// load it back.
    fn release(&self, repo_uuid: Uuid) -> Result<RepoLocator, ApiError> {
        let repo_state = self.repo(repo_uuid)?;
        let locator = RepoLocator::Metafolder(repo_state.metafolder_dir.clone());
        let released = Arc::downgrade(&repo_state);
        drop(repo_state);
        self.unload_repo(repo_uuid)?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while released.strong_count() > 0 {
            if std::time::Instant::now() > deadline {
                let _ = self.reload(locator.clone());
                return Err(ApiError::conflict("the repository is still in use; try again"));
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        Ok(locator)
    }

    /// Restores a repository from a backup (`mf repo restore`, spec-storage
    /// increment 5): `from`, or its most recent one. A loaded repository is
    /// released, restored and loaded back — on its old store when the
    /// restore failed, which then changed nothing; one that is not loaded
    /// (its store may be what no longer loads) is restored and then loaded.
    pub fn restore_repo(
        &self,
        locator: RepoLocator,
        from: Option<PathBuf>,
    ) -> Result<(Uuid, crate::backup::Restored), ApiError> {
        // Resolved by hand: `config.json` may be what was lost.
        let metafolder = match &locator {
            RepoLocator::Root(root) => root
                .canonicalize()
                .map_err(|_| {
                    ApiError::bad_request(format!(
                        "Cannot resolve path {root:?}: the root directory must exist"
                    ))
                })?
                .join(".metafolder"),
            RepoLocator::Metafolder(dir) => dir.clone(),
        };
        if !metafolder.is_dir() {
            return Err(ApiError::bad_request(format!(
                "No repository found at {metafolder:?} (no such directory)"
            )));
        }
        let loaded = RepoConfig::read(&metafolder)
            .ok()
            .map(|config| config.repo_uuid)
            .filter(|uuid| self.repos.lock_recover().contains_key(uuid));
        let metafolder = match loaded {
            Some(uuid) => match self.release(uuid)? {
                RepoLocator::Metafolder(dir) => dir,
                RepoLocator::Root(_) => unreachable!("release answers the metafolder"),
            },
            None => metafolder,
        };
        let restored = crate::backup::restore(&metafolder, from.as_deref());
        if restored.is_ok() || loaded.is_some() {
            let back = self.reload(RepoLocator::Metafolder(metafolder.clone()));
            if restored.is_ok() {
                back?;
            }
        }
        let restored = restored.map_err(ApiError::from)?;
        let uuid = RepoConfig::read(&metafolder)?.repo_uuid;
        Ok((uuid, restored))
    }

    /// Loads a repository back and warms it before it answers.
    fn reload(&self, locator: RepoLocator) -> Result<(), ApiError> {
        let uuid = self.load_repo(locator)?;
        self.repo(uuid)?.warm(&|_, _, _| {})
    }

    /// Unloads a repository: removes it from the loaded set, stops its watcher
    /// and executor, and releases the exclusive SQLite lock — so it can be
    /// re-loaded or opened by another daemon (spec-main "Repository management").
    ///
    /// An unknown repository is a 404 (no idempotency claimed). The unload is
    /// refused with 409 if:
    /// - a coordinated-rollback navigation is in progress (its lock must not be
    ///   silently dropped — complete or abort it first), or
    /// - a cancellable task (reconcile/query) is in flight: the caller is asked
    ///   to stop it first (`POST …/tasks/:id/cancel`), so the repository is
    ///   never pulled out from under running work. Transient `flush` tasks do
    ///   not block the unload.
    /// - a `load` warmup is in flight: it holds the connection, so the unload
    ///   waits for it to finish (warmup is not cancellable).
    pub fn unload_repo(&self, repo_uuid: Uuid) -> Result<(), ApiError> {
        let removed = {
            let mut repos = self.repos.lock_recover();
            let Some(repo_state) = repos.get(&repo_uuid) else {
                return Err(ApiError::not_found(format!("Repository not found: {repo_uuid}")));
            };
            if repo_state.is_rollback_locked() {
                return Err(ApiError::conflict(
                    "repository is in rollback lock; complete or abort the navigation first",
                ));
            }
            if repo_state.tasks.has_active_cancellable() {
                return Err(ApiError::conflict(
                    "a task is in progress; stop it first, then unload",
                ));
            }
            if repo_state.tasks.has_active_load() {
                // The warmup holds the connection; removing the repo now would
                // leave its database locked with no reachable task to wait on.
                return Err(ApiError::conflict(
                    "repository is warming up; wait for the load to finish, then unload",
                ));
            }
            repos.remove(&repo_uuid)
            // The `repos` guard is released at the end of this block, before the
            // `Arc` is dropped below.
        };
        // Dropping the last `Arc` runs `RepoHandles::drop` (watcher stopped,
        // executor joined) and closes the connection (releasing the lock). Done
        // outside the map lock so the executor-thread join cannot block another
        // repository operation that needs the map.
        drop(removed);
        Ok(())
    }

    /// Fetches a loaded repository or fails with 404.
    pub fn repo(&self, repo_uuid: Uuid) -> Result<Arc<RepoState>, ApiError> {
        self.repos
            .lock_recover()
            .get(&repo_uuid)
            .cloned()
            .ok_or_else(|| ApiError::not_found(format!("Repository not found: {repo_uuid}")))
    }

    /// The repository, if it can serve *data*.
    ///
    /// A repository is registered before it is warm, so that its state — its
    /// entry in the listing, and the `load` task carrying the phase it is on —
    /// is readable while it warms. Its data is not: the query engine and the
    /// executor both run against accelerators that are not built yet, so there
    /// is no slower answer to give, only none (spec-main "POST /repos/load").
    pub fn ready_repo(&self, repo_uuid: Uuid) -> Result<Arc<RepoState>, ApiError> {
        let repo = self.repo(repo_uuid)?;
        if !repo.is_ready() {
            return Err(ApiError::unavailable(format!(
                "repository {repo_uuid} is still loading; watch its `load` task"
            )));
        }
        Ok(repo)
    }

    /// Loaded repositories, sorted by UUID. `include_system` keeps daemon-internal
    /// repos (spec-sync plan repos) that are otherwise hidden.
    pub fn list_repos(&self, include_system: bool) -> Vec<RepoInfo> {
        let repos = self.repos.lock_recover();
        let mut infos: Vec<RepoInfo> =
            repos.values().map(|r| r.info()).filter(|i| include_system || !i.system).collect();
        infos.sort_by_key(|i| i.repo_uuid);
        infos
    }

    /// One loaded repository's info, or 404.
    pub fn repo_info(&self, repo_uuid: Uuid) -> Result<RepoInfo, ApiError> {
        Ok(self.repo(repo_uuid)?.info())
    }

    /// Renames a loaded repository, keeping names unique among loaded repos
    /// (409 on clash) and persisting to `config.json`.
    pub fn rename_repo(&self, repo_uuid: Uuid, new_name: &str) -> Result<RepoInfo, ApiError> {
        let target = {
            let repos = self.repos.lock_recover();
            if repos.iter().any(|(u, r)| *u != repo_uuid && r.name() == new_name) {
                return Err(ApiError::conflict(format!(
                    "a repository named '{new_name}' is already loaded; names must be unique"
                )));
            }
            repos
                .get(&repo_uuid)
                .cloned()
                .ok_or_else(|| ApiError::not_found(format!("Repository not found: {repo_uuid}")))?
        };
        target
            .rename(new_name.to_string())
            .map_err(|e| ApiError::internal(format!("failed to persist the rename: {e}")))?;
        Ok(target.info())
    }

    /// All tasks across every loaded repository (global `GET /tasks`).
    pub fn all_tasks(&self) -> Vec<crate::tasks::TaskView> {
        let repos = self.repos.lock_recover();
        let mut tasks: Vec<crate::tasks::TaskView> =
            repos.values().flat_map(|r| r.tasks.list()).collect();
        tasks.sort_by(|a, b| a.started_at.cmp(&b.started_at).then(a.id.cmp(&b.id)));
        tasks
    }
}
