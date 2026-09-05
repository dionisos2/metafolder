//! In-memory daemon state: the set of loaded repositories.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use metafolder_core::sync::MutexExt;
use rusqlite::Connection;
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

/// One loaded repository. The SQLite connection and the tree cache each sit
/// behind their own mutex; blocking work runs in `spawn_blocking`.
pub struct RepoState {
    pub conn: Mutex<Connection>,
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
    /// Derived in-memory query accelerator (spec-indexing). Rebuilt from the
    /// `field` table whenever the log HEAD it was built at no longer matches
    /// the current HEAD (it carries no incremental maintenance yet), and only
    /// consulted while fresh — so it never serves stale results. `None` until
    /// the first query builds it.
    pub index: Mutex<Option<crate::index::RepoIndex>>,
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
    /// Mass-orphan circuit breaker (`[settings] orphan-cascade-limit`), read by
    /// the executor before applying a cascade.
    pub orphan_cascade_limit: usize,
    /// Ingestion of filesystem events is paused (spec-file-tracking "Pausing
    /// ingestion"): the watcher keeps buffering events into
    /// `pending_operation`, the executor applies none until a resume. Set by
    /// stopping a flush, and by `POST /watch/pause`. In memory like the task
    /// registry: a reload or a restart starts ingesting again.
    pub ingestion_paused: std::sync::atomic::AtomicBool,
}

/// State of an in-progress coordinated rollback navigation.
pub struct RollbackLock {
    /// Resolved target operation id; `None` is the empty state.
    pub target: Option<i64>,
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
        Self {
            conn: Mutex::new(opened.conn),
            cache: Mutex::new(TreeCache::new(opened.case_insensitive)),
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
            index: Mutex::new(None),
            ready: std::sync::atomic::AtomicBool::new(false),
            watch_quiet_period: settings.watch_quiet_period(),
            pending: Mutex::new(Vec::new()),
            orphan_cascade_limit: settings.orphan_cascade_limit,
            ingestion_paused: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// The repository's current (mutable) display name.
    pub fn name(&self) -> String {
        self.name.lock_recover().clone()
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

    /// Recomputes the watcher's eligible-directory set after a manual write that
    /// changed `mf_watch`/`mf_ignore` (spec-file-tracking "Watch and Ignore"),
    /// so a subtree just made eligible starts being watched immediately (and one
    /// just excluded stops). No-op when the watcher is not running (unit tests,
    /// or a repository being torn down). `conn` is the already-locked
    /// connection; the tree cache is locked here.
    pub fn refresh_watches(&self, conn: &Connection) -> usize {
        let handles = self.handles.lock_recover();
        let Some(handles) = handles.as_ref() else {
            return 0;
        };
        let mut cache = self.lock_cache();
        handles.watcher.refresh(conn, &mut cache, &self.config.root, &self.internal_dir())
    }

    /// Directories currently watched — one inotify watch each, on a budget
    /// shared with every other program on the machine that watches files.
    pub fn watched_dirs(&self) -> usize {
        self.handles.lock_recover().as_ref().map_or(0, |h| h.watcher.watched())
    }

    /// Warms the in-memory accelerators of a freshly loaded repository:
    /// eagerly populates the tree cache (so tree navigation is served from
    /// memory — spec-file-tracking "Tree Cache") and builds the query index (so
    /// the first query pays no build cost — spec-indexing). Both are
    /// best-effort: a failure just leaves the repository in DB-fallback mode,
    /// which is correct, only slower. `progress` reports `(phase, done, total)`
    /// for the load progress bar; it is a no-op for the synchronous callers
    /// (startup auto-load, `init`).
    ///
    /// Holds the connection for its duration (a single bulk read), so queries
    /// on this repository wait until it finishes — the load progress bar tells
    /// the user why. Idempotent enough: re-running on an already-warm repo just
    /// rebuilds, so callers skip it when [`TreeCache::is_complete`] already holds.
    pub fn warmup(&self, progress: ProgressFn) -> Result<(), ApiError> {
        let conn = self.conn.lock_recover();
        // Per-phase timings are logged (`[warmup <name>] …`): a persistent load
        // report, so a slow phase on a large repository is visible without a
        // profiler. See also the `[tree cache]` and `[watcher]` split lines.
        let who = self.name();

        // Build the index first: its single scan of the whole `field` table is
        // the load's cold-I/O floor (on first open the table is read from disk),
        // and it reports a determinate progress bar. Populating the tree cache
        // afterwards re-reads the same, now warm, pages — so it is fast, where
        // run first it would silently absorb that cold cost under an
        // indeterminate spinner. The single scan over `field` collects the
        // TreeRef forest too, so the tree cache is then built from those rows in
        // memory — no second scan.
        //
        // Neither is best-effort any more. They used to be, because a
        // repository without them fell back to the SQL engine; the executor and
        // the query engine now both work against them, so a repository that
        // cannot build them is one that cannot serve, and the load says so
        // rather than degrading quietly.
        let mut forest = Vec::new();
        {
            let _p = Phase::begin(&who, "build the query index");
            let index = crate::index::RepoIndex::build_reported_collecting(
                &conn,
                &mut forest,
                &|done, total| progress("index", Some(done), Some(total)),
                &|| false, // the load warmup is not cancellable (spec-tasks)
            )
            .map_err(|e| ApiError::internal(format!("failed to build the query index: {e:#}")))?;
            *self.index.lock_recover() = Some(index);
        }

        progress("tree cache", None, None);
        {
            let mut p = Phase::begin(&who, "populate the tree cache");
            p.detail(format!("{} nodes, from the index scan", forest.len()));
            self.lock_cache().populate_from_forest(forest);
        }
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
            // One inotify watch per directory: worth stating, because the
            // budget is per user and shared (spec-file-tracking "File Watcher").
            p.detail(format!("{watched} directories"));
        }
        Ok(())
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
